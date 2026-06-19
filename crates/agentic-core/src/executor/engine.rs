//! Agentic loop executor.
//!
//! Exposes each step of the loop as a public function so consumers can compose
//! them directly (e.g. as Praxis filters). [`execute`] is the convenience entry
//! point that composes all steps with the default control flow.

use std::pin::Pin;
use std::sync::Arc;

use async_stream::stream;
use either::Either;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use tracing::warn;

use crate::executor::accumulator::ResponseAccumulator;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::modes::{ConversationHandler, ResponseHandler};
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::storage::InOutItem;
use crate::types::event::ResponseStatus;
use crate::types::io::{
    FunctionTool, FunctionToolCall, FunctionToolResultMessage, InputItem, OutputItem, ResponsesInput, ResponsesTool,
    resolve_tool_choice, resolve_tools,
};
use crate::types::request_response::{RequestPayload, ResponsePayload};
use crate::utils::common::serialize_to_string;
use crate::utils::uuid7_str;
use crate::vector_search::types::SearchOptions;

use std::time::Duration;

/// SSE stream of raw lines sent to the client (`data: …\n\n` per event).
pub type BoxStream = Pin<Box<dyn Stream<Item = String> + Send>>;

/// Wire-format marker signalling end-of-stream to the client.
const DONE_MARKER: &str = "data: [DONE]\n\n";

/// Fetch the next raw bytes chunk from a streaming response.
///
/// Returns `Ok(Some(bytes))` on data, `Ok(None)` when the stream ends cleanly,
/// and `Err` on a network failure or chunk timeout.
async fn next_chunk<S>(stream: &mut S, timeout: Duration) -> ExecutorResult<Option<bytes::Bytes>>
where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    let item = if timeout.is_zero() {
        stream.next().await
    } else {
        tokio::time::timeout(timeout, stream.next()).await.map_err(|_| {
            ExecutorError::StreamError("chunk timeout: no data received within the configured window".into())
        })?
    };
    item.transpose().map_err(ExecutorError::NetworkError)
}

/// Build, send, and validate an HTTP POST to the LLM backend.
///
/// Shared by both the blocking path (caller reads `.text()`) and the streaming
/// path (caller reads `.bytes_stream()`). Maps connect/timeout failures and
/// non-2xx status codes to [`ExecutorError::LLMRequest`].
async fn send_request(
    client: &reqwest::Client,
    url: &str,
    body: String,
    auth: Option<&str>,
) -> ExecutorResult<reqwest::Response> {
    let mut req = client.post(url).header("Content-Type", "application/json").body(body);
    if let Some(key) = auth {
        req = req.bearer_auth(key);
    }

    let resp = req.send().await.map_err(|e| ExecutorError::LLMRequest {
        status: if e.is_timeout() {
            http::StatusCode::GATEWAY_TIMEOUT
        } else {
            http::StatusCode::BAD_GATEWAY
        },
        body: if e.is_timeout() {
            "upstream timeout".into()
        } else {
            "upstream unavailable".into()
        },
    })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        // Log and discard any error reading the error body — the status code
        // is the primary signal; an empty body is acceptable here.
        let body = resp
            .text()
            .await
            .inspect_err(|e| tracing::debug!("failed to read error response body: {e}"))
            .unwrap_or_default();
        return Err(ExecutorError::LLMRequest {
            status: http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR),
            body,
        });
    }

    Ok(resp)
}

/// Makes a non-streaming HTTP POST to the LLM backend and returns the full JSON body.
///
/// Used by [`run_blocking`] so it can pass the result to [`ResponseAccumulator::from_json`].
async fn fetch_response_json(
    upstream_json: String,
    url: &str,
    client: &reqwest::Client,
    auth: Option<&str>,
) -> ExecutorResult<String> {
    let resp = send_request(client, url, upstream_json, auth).await?;
    // Preserve the reqwest::Error as the typed source (NetworkError).
    resp.text().await.map_err(ExecutorError::NetworkError)
}

/// Step 1 — Build [`RequestContext`] by rehydrating conversation history.
///
/// `request` is moved into the context as `enriched_request`; one clone is taken
/// for `original_request` so the engine retains an unmodified copy for persistence
/// and ID resolution.
///
/// Dispatches based on `store` flag and which ID is present:
/// - `previous_response_id`: rehydrate from the prior response checkpoint
/// - `conversation_id`:      rehydrate from the conversation
/// - no ids:                 forward only the new input
///
/// # Errors
/// Returns [`ExecutorError`] if storage is unavailable or a referenced ID does not exist.
pub async fn rehydrate_conversation(
    request: RequestPayload,
    exec_ctx: &ExecutionContext,
) -> ExecutorResult<RequestContext> {
    let response_id = uuid7_str("resp_");
    let new_input_items: Vec<InputItem> = Vec::from(&request.input);

    // One clone for the unmodified original; `request` is moved as enriched_request.
    let original_request = request.clone();
    let mut ctx = RequestContext {
        enriched_request: request,
        original_request,
        new_input_items,
        response_id,
        conversation_id: None,
    };

    if ctx.original_request.conversation_id.is_some() && ctx.original_request.previous_response_id.is_some() {
        return Err(ExecutorError::InvalidRequest(
            "provide only one of conversation_id or previous_response_id".into(),
        ));
    }

    if ctx.original_request.conversation_id.is_some() {
        rehydrate_from_conversation(&mut ctx, exec_ctx).await?;
        return Ok(ctx);
    }

    if ctx.original_request.previous_response_id.is_some() {
        rehydrate_from_response(&mut ctx, exec_ctx).await?;
        return Ok(ctx);
    }

    ctx.enriched_request.input = ResponsesInput::Items(ctx.new_input_items.clone());
    Ok(ctx)
}

/// Hydrates `ctx` from the previous response chain.
///
/// Loads the stored response, rehydrates its history items, resolves effective
/// tools and tool choice from the stored metadata, and prepends the history to
/// the enriched request input.
async fn rehydrate_from_response(ctx: &mut RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<()> {
    let stored = exec_ctx.resp_handler.get(ctx).await?;
    let history = exec_ctx.resp_handler.rehydrate(ctx).await?;

    let mut items = InOutItem::into_input_items(history);
    items.reserve(ctx.new_input_items.len());
    items.extend(ctx.new_input_items.iter().cloned());

    ctx.enriched_request.previous_response_id = None;
    ctx.enriched_request.input = ResponsesInput::Items(items);
    ctx.enriched_request.tools = resolve_tools(
        ctx.original_request.tools.as_deref(),
        stored.metadata.effective_tools.as_deref(),
        ctx.original_request.tools.is_some(),
    );
    ctx.enriched_request.tool_choice = resolve_tool_choice(
        &ctx.original_request.tool_choice,
        &stored.metadata.effective_tool_choice,
        false,
    );
    ctx.conversation_id = stored.conversation_id;
    Ok(())
}

/// Hydrates `ctx` from the conversation store.
///
/// Gets or creates the conversation (depending on `store`) and rehydrates its
/// history in parallel, then prepends the history items to the enriched request input.
async fn rehydrate_from_conversation(ctx: &mut RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<()> {
    let (conv_data, history) = tokio::try_join!(
        async {
            if ctx.original_request.store {
                exec_ctx.conv_handler.get_or_create(ctx).await
            } else {
                exec_ctx.conv_handler.get(ctx).await
            }
        },
        exec_ctx.conv_handler.rehydrate(ctx),
    )?;

    let mut items = InOutItem::into_input_items(history);
    items.reserve(ctx.new_input_items.len());
    items.extend(ctx.new_input_items.iter().cloned());

    ctx.enriched_request.input = ResponsesInput::Items(items);
    ctx.conversation_id = Some(conv_data.conversation_id);
    Ok(())
}

/// Step 2 — Call the LLM inference backend; yields raw SSE lines (`data: …`).
///
/// Always requests `stream=true` upstream. Stops on `[DONE]`.
///
/// # Errors
/// Each stream item is `Result<String, ExecutorError>`. The stream yields `Err` on:
/// - [`ExecutorError::LLMRequest`] — connect timeout (504), connection failure (502),
///   or non-2xx HTTP status from the backend
/// - [`ExecutorError::NetworkError`] — network failure while reading the response body
pub fn call_inference(
    upstream_json: String,
    url: String,
    client: Arc<reqwest::Client>,
    auth: Option<String>,
    chunk_timeout: Duration,
) -> impl Stream<Item = Result<String, ExecutorError>> + Send + 'static {
    stream! {
        let resp = match send_request(&client, &url, upstream_json, auth.as_deref()).await {
            Ok(r) => r,
            Err(e) => { yield Err(e); return; }
        };

        let mut bytes = resp.bytes_stream();
        let mut buf = String::with_capacity(8192);

        loop {
            let chunk = match next_chunk(&mut bytes, chunk_timeout).await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => { yield Err(e); return; }
            };

            match std::str::from_utf8(&chunk) {
                Ok(s) => buf.push_str(s),
                Err(_) => buf.push_str(&String::from_utf8_lossy(&chunk)),
            }

            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim_end_matches('\r');
                match line {
                    "data: [DONE]" => return,
                    l if l.starts_with("data: ") => yield Ok(l.to_string()),
                    _ => {}
                }
                buf.drain(..=pos);
            }
        }
    }
}

/// Step 3 — Persist the completed response to storage.
///
/// Skipped if [`ResponseStatus`] is not `Completed`/`Incomplete` or `payload.id` is empty.
/// Routes to [`ConversationHandler`] when `ctx.conversation_id` is set,
/// otherwise [`ResponseHandler`].
///
/// # Errors
/// Returns [`ExecutorError`] if the storage operation fails.
pub async fn persist_response(
    payload: ResponsePayload,
    ctx: RequestContext,
    conv_handler: ConversationHandler,
    resp_handler: ResponseHandler,
) -> ExecutorResult<()> {
    // Use typed enum — no hardcoded status strings.
    if !matches!(
        payload.status.parse::<ResponseStatus>().unwrap_or_default(),
        ResponseStatus::Completed | ResponseStatus::Incomplete
    ) || payload.id.is_empty()
    {
        return Ok(());
    }

    // Move output items from payload; handlers build ResponseMetadata from ctx internally.
    let output_items = payload.output;

    if ctx.conversation_id.is_some() {
        conv_handler.execute_turn(ctx, output_items).await
    } else {
        resp_handler.execute_turn(ctx, output_items).await
    }
}

fn contains_file_search(tools: Option<&[ResponsesTool]>) -> bool {
    tools.is_some_and(|tools| tools.iter().any(|tool| matches!(tool, ResponsesTool::FileSearch(_))))
}

#[derive(Clone)]
struct FileSearchConfig {
    store_ids: Vec<String>,
    options: SearchOptions,
}

fn file_search_config(tools: Option<&[ResponsesTool]>) -> ExecutorResult<FileSearchConfig> {
    let mut store_ids = Vec::new();
    let mut options = None::<SearchOptions>;

    for tool in tools.into_iter().flatten() {
        match tool {
            ResponsesTool::FileSearch(tool) => {
                store_ids.extend(tool.vector_store_ids.iter().filter(|id| !id.is_empty()).cloned());
                if options
                    .as_ref()
                    .is_some_and(|existing| existing != &tool.search_options)
                {
                    return Err(ExecutorError::InvalidRequest(
                        "multiple file_search tools with different search options are not supported".into(),
                    ));
                }
                options.get_or_insert_with(|| tool.search_options.clone());
            }
            ResponsesTool::Function(_) | ResponsesTool::Unknown => {}
        }
    }

    if store_ids.is_empty() {
        return Err(ExecutorError::InvalidRequest(
            "file_search requires at least one vector_store_ids entry".into(),
        ));
    }

    Ok(FileSearchConfig {
        store_ids,
        options: options.unwrap_or_default(),
    })
}

fn file_search_function_tool() -> ResponsesTool {
    ResponsesTool::Function(FunctionTool {
        name: "file_search".to_string(),
        description: Some("Search attached vector stores for relevant file content.".to_string()),
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query to run against the vector store."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })),
        strict: Some(true),
    })
}

fn translate_file_search_tools(tools: Option<&[ResponsesTool]>) -> Option<Vec<ResponsesTool>> {
    let tools = tools?;
    let mut translated = Vec::with_capacity(tools.len());
    for tool in tools {
        match tool {
            ResponsesTool::Function(tool) => translated.push(ResponsesTool::Function(tool.clone())),
            ResponsesTool::FileSearch(_) => translated.push(file_search_function_tool()),
            ResponsesTool::Unknown => {}
        }
    }
    Some(translated)
}

fn file_search_calls(output: &[OutputItem]) -> ExecutorResult<Vec<FunctionToolCall>> {
    let mut file_search_calls = Vec::new();
    let mut other_tool_names = Vec::new();

    for item in output {
        match item {
            OutputItem::FunctionCall(call) if call.name == "file_search" => file_search_calls.push(call.clone()),
            OutputItem::FunctionCall(call) => other_tool_names.push(call.name.clone()),
            OutputItem::Message(_) | OutputItem::Reasoning(_) | OutputItem::Unknown => {}
        }
    }

    if !file_search_calls.is_empty() && !other_tool_names.is_empty() {
        return Err(ExecutorError::ToolExecution(format!(
            "mixed tool calls are not supported in file_search loop: {}",
            other_tool_names.join(", ")
        )));
    }

    Ok(file_search_calls)
}

#[derive(Deserialize)]
struct FileSearchArguments {
    query: String,
}

fn query_from_arguments(arguments: &str) -> ExecutorResult<String> {
    let args = serde_json::from_str::<FileSearchArguments>(arguments)
        .map_err(|err| ExecutorError::ToolExecution(format!("invalid file_search arguments: {err}")))?;

    if args.query.trim().is_empty() {
        return Err(ExecutorError::ToolExecution(
            "file_search query argument is required".into(),
        ));
    }

    Ok(args.query)
}

fn append_input_item(input: &mut ResponsesInput, item: InputItem) {
    let mut items = Vec::<InputItem>::from(&*input);
    items.push(item);
    *input = ResponsesInput::Items(items);
}

async fn run_file_search_loop(mut ctx: RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<ResponsePayload> {
    if ctx.original_request.stream {
        return Err(ExecutorError::InvalidRequest(
            "streaming file_search requests are not supported".into(),
        ));
    }

    let Some(vector_search) = exec_ctx.vector_search.as_ref() else {
        return Err(ExecutorError::InvalidRequest(
            "file_search requires a configured vector search backend".into(),
        ));
    };

    let file_search = file_search_config(ctx.enriched_request.tools.as_deref())?;
    ctx.enriched_request.tools = translate_file_search_tools(ctx.enriched_request.tools.as_deref());
    let url = exec_ctx.responses_url();

    for _ in 0..exec_ctx.max_iterations {
        let upstream_json =
            serialize_to_string(&ctx.enriched_request.to_upstream_request(false)).map_err(ExecutorError::JsonError)?;
        let body = fetch_response_json(upstream_json, &url, &exec_ctx.client, exec_ctx.client_auth.as_deref()).await?;
        let acc = ResponseAccumulator::from_json(&body, ctx.conversation_id.as_deref())?;
        let mut payload = acc.finalize(
            &ctx.enriched_request.model,
            ctx.original_request.previous_response_id.as_deref(),
            ctx.original_request.instructions.as_deref(),
        );

        let tool_calls = file_search_calls(&payload.output)?;
        if tool_calls.is_empty() {
            ctx.inject_ids(&mut payload);
            let should_persist = ctx.original_request.store
                || ctx.original_request.previous_response_id.is_some()
                || ctx.original_request.conversation_id.is_some();
            if should_persist {
                let ch = exec_ctx.conv_handler.clone();
                let rh = exec_ctx.resp_handler.clone();
                if let Err(e) = persist_response(payload.clone(), ctx, ch, rh).await {
                    warn!("persist failed: {e}");
                }
            }
            return Ok(payload);
        }

        for call in tool_calls {
            let input_call = InputItem::FunctionCall(call.clone());
            append_input_item(&mut ctx.enriched_request.input, input_call.clone());
            ctx.new_input_items.push(input_call);

            let query = query_from_arguments(&call.arguments)?;
            let mut results = Vec::new();
            for store_id in &file_search.store_ids {
                match vector_search.search(store_id, &query, &file_search.options).await {
                    Ok(mut store_results) => results.append(&mut store_results),
                    Err(err) => {
                        return Err(ExecutorError::ToolExecution(format!(
                            "file_search vector lookup failed for vector store {store_id}: {err}"
                        )));
                    }
                }
            }

            let output =
                serialize_to_string(&serde_json::json!({ "results": results })).map_err(ExecutorError::JsonError)?;
            let result_item = InputItem::FunctionCallOutput(FunctionToolResultMessage {
                call_id: call.call_id,
                output,
            });
            append_input_item(&mut ctx.enriched_request.input, result_item.clone());
            ctx.new_input_items.push(result_item);
        }
    }

    Err(ExecutorError::MaxIterations {
        max_iterations: exec_ctx.max_iterations,
    })
}

async fn run_blocking(ctx: RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<ResponsePayload> {
    let url = exec_ctx.responses_url();
    // Non-streaming request: stream=false → full JSON body → from_json.
    let upstream_json =
        serialize_to_string(&ctx.enriched_request.to_upstream_request(false)).map_err(ExecutorError::JsonError)?;

    let body = fetch_response_json(upstream_json, &url, &exec_ctx.client, exec_ctx.client_auth.as_deref()).await?;

    let acc = ResponseAccumulator::from_json(&body, ctx.conversation_id.as_deref())?;
    let mut payload = acc.finalize(
        &ctx.enriched_request.model,
        ctx.original_request.previous_response_id.as_deref(),
        ctx.original_request.instructions.as_deref(),
    );
    ctx.inject_ids(&mut payload);

    let should_persist = ctx.original_request.store
        || ctx.original_request.previous_response_id.is_some()
        || ctx.original_request.conversation_id.is_some();
    if should_persist {
        let ch = exec_ctx.conv_handler.clone();
        let rh = exec_ctx.resp_handler.clone();
        if let Err(e) = persist_response(payload.clone(), ctx, ch, rh).await {
            warn!("persist failed: {e}");
        }
    }

    Ok(payload)
}

fn run_stream(ctx: RequestContext, exec_ctx: Arc<ExecutionContext>) -> BoxStream {
    let url = exec_ctx.responses_url();
    // Streaming request: stream=true → SSE lines → from_stream.
    let upstream_json = match serialize_to_string(&ctx.enriched_request.to_upstream_request(true)) {
        Ok(s) => s,
        Err(e) => {
            return Box::pin(stream! {
                yield format!("data: {{\"error\": \"serialize error: {e}\"}}\n\n");
                yield DONE_MARKER.to_string();
            });
        }
    };

    // Persist when store=true, or when an ID is passed — context continuity must
    // be preserved even if the caller sets store=false.
    let should_persist = ctx.original_request.store
        || ctx.original_request.previous_response_id.is_some()
        || ctx.original_request.conversation_id.is_some();

    Box::pin(stream! {
        let line_stream = Box::pin(call_inference(
            upstream_json,
            url,
            Arc::clone(&exec_ctx.client),
            exec_ctx.client_auth.clone(),
            exec_ctx.streaming_timeout,
        ));

        // from_stream feeds SSE lines to a spawn_blocking worker via channel.
        // All JSON parsing is CPU-bound and runs off the async executor.
        match ResponseAccumulator::from_stream(line_stream, ctx.conversation_id.as_deref()).await {
            Err(e) => {
                yield format!("data: {{\"error\": \"{e}\"}}\n\n");
                yield DONE_MARKER.to_string();
            }
            Ok(acc) => {
                let mut payload = acc.finalize(
                    &ctx.enriched_request.model,
                    ctx.original_request.previous_response_id.as_deref(),
                    ctx.original_request.instructions.as_deref(),
                );
                ctx.inject_ids(&mut payload);
                yield payload.as_responses_chunk();
                yield DONE_MARKER.to_string();

                if should_persist {
                    let ch = exec_ctx.conv_handler.clone();
                    let rh = exec_ctx.resp_handler.clone();
                    if let Err(e) = persist_response(payload, ctx, ch, rh).await {
                        warn!("persist failed: {e}");
                    }
                }
            }
        }
    })
}

/// Create a new conversation and return its data.
///
/// Exposes the conversation-creation step as a standalone function so callers
/// (e.g. `agentic-server`, Praxis filters, or tests) can pre-create a
/// conversation before submitting response turns.
///
/// # Errors
/// Returns [`ExecutorError`] if the conversation store is unavailable.
pub async fn create_conversation(exec_ctx: &ExecutionContext) -> ExecutorResult<crate::ConversationData> {
    exec_ctx.conv_handler.create().await
}

/// Run the full agentic loop.
///
/// Returns `Either::Left(ResponsePayload)` for non-streaming requests, or
// TODO: replace with a builder — ExecuteRequest::new(payload, ctx).auth(token).run().await
/// `Either::Right(BoxStream)` for streaming, each yielded `String` is an SSE
/// line ready to forward to the client.
///
/// # Errors
/// Returns [`ExecutorError`] if rehydration or (non-streaming) LLM inference fails.
pub async fn execute(
    request: RequestPayload,
    exec_ctx: Arc<ExecutionContext>,
) -> ExecutorResult<Either<ResponsePayload, BoxStream>> {
    let ctx = rehydrate_conversation(request, &exec_ctx).await?;
    if contains_file_search(ctx.enriched_request.tools.as_deref()) {
        return Ok(Either::Left(run_file_search_loop(ctx, &exec_ctx).await?));
    }
    if ctx.original_request.stream {
        Ok(Either::Right(run_stream(ctx, exec_ctx)))
    } else {
        Ok(Either::Left(run_blocking(ctx, &exec_ctx).await?))
    }
}
