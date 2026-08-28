//! Stateful conversation executor.
//!
//! Exposes each step of the conversation pipeline as a public function so consumers
//! can compose them directly (e.g. as Praxis filters). [`ExecuteRequest`] is the
//! primary entry point; [`execute`] is a convenience shim for callers that don't
//! need per-request configuration.

use std::sync::Arc;

use async_stream::stream;
use either::Either;
use tokio::sync::mpsc;
use tracing::debug;

use super::compaction::{compact_items, maybe_compact_context};
use super::gateway::{
    GatewayCallResult, LoopDecision, append_gateway_calls_to_new_input, append_output_items_to_input,
    append_tool_outputs, classify_round, compaction_event_plans, complete_gateway_event_plans,
    emit_gateway_completed_events, emit_gateway_start_events, emit_response_start_events,
    execute_and_emit_output_calls, execute_output_calls, gateway_event_plans, has_client_owned_calls,
    is_client_custom_call, is_gateway_owned_call, public_output_items,
};
use super::gateway_accumulator::{GatewayStreamAccumulator, StreamEvent, error_sse_chunk};
use crate::events::EventFrame;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::inference::DONE_MARKER;
use crate::executor::persist::persist_if_needed;
use crate::executor::prepare::prepare_request_tools;
use crate::executor::rehydrate::rehydrate_conversation;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::upstream::{emit_deferred_stream_events, fetch_blocking_payload, fetch_stream_payload};
use crate::tool::{ToolRegistry, mcp::handler::list_tools_output_item};
use crate::types::io::{InputItem, OutputItem, ResponseUsage, ResponsesInput, ToolChoice};
use crate::types::request_response::{IncompleteDetails, RequestPayload, ResponsePayload};
use crate::utils::common::utcnow_str;

pub use crate::executor::inference::BoxStream;

const MAX_GATEWAY_TOOL_ROUNDS: usize = 10;

fn add_usage(total: ResponseUsage, usage: ResponseUsage) -> ResponseUsage {
    ResponseUsage {
        input_tokens: total.input_tokens.saturating_add(usage.input_tokens),
        output_tokens: total.output_tokens.saturating_add(usage.output_tokens),
        total_tokens: total.total_tokens.saturating_add(usage.total_tokens),
        input_tokens_details: crate::types::io::InputTokenDetails {
            cached_tokens: total
                .input_tokens_details
                .cached_tokens
                .saturating_add(usage.input_tokens_details.cached_tokens),
        },
        output_tokens_details: crate::types::io::OutputTokenDetails {
            reasoning_tokens: total
                .output_tokens_details
                .reasoning_tokens
                .saturating_add(usage.output_tokens_details.reasoning_tokens),
        },
    }
}

fn accumulate_usage(total: &mut Option<ResponseUsage>, usage: Option<ResponseUsage>) {
    if let Some(usage) = usage {
        *total = Some(total.map_or(usage, |current| add_usage(current, usage)));
    }
}

struct AbortOnDrop<T> {
    handle: tokio::task::JoinHandle<T>,
}

impl<T> AbortOnDrop<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self { handle }
    }
}

impl<T> std::ops::Deref for AbortOnDrop<T> {
    type Target = tokio::task::JoinHandle<T>;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl<T> std::ops::DerefMut for AbortOnDrop<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handle
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if !self.handle.is_finished() {
            self.handle.abort();
        }
    }
}

async fn run_until_gateway_tools_complete(
    ctx: RequestContext,
    registry: ToolRegistry,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
    stream_upstream: bool,
    mut stream: Option<(&mut GatewayStreamAccumulator, &mpsc::UnboundedSender<StreamEvent>)>,
) -> ExecutorResult<(ResponsePayload, RequestContext, ToolRegistry)> {
    if ctx.original_request.input.has_compaction_trigger() {
        let (payload, ctx) = run_compaction_trigger(ctx, exec_ctx, auth).await?;
        if let Some((stream_accumulator, stream_sender)) = stream.as_mut() {
            emit_response_start_events(&payload, stream_accumulator, stream_sender)?;
            let event_plans = compaction_event_plans(&payload.output, 0);
            emit_gateway_start_events(&event_plans, stream_accumulator, stream_sender)?;
            emit_gateway_completed_events(&payload.output, &event_plans, stream_accumulator, stream_sender)?;
        }
        return Ok((payload, ctx, registry));
    }

    run_gateway_tool_loop(ctx, registry, exec_ctx, auth, stream_upstream, stream).await
}

async fn run_gateway_tool_loop(
    mut ctx: RequestContext,
    registry: ToolRegistry,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
    stream_upstream: bool,
    mut stream: Option<(&mut GatewayStreamAccumulator, &mpsc::UnboundedSender<StreamEvent>)>,
) -> ExecutorResult<(ResponsePayload, RequestContext, ToolRegistry)> {
    let mut executors = exec_ctx.gateway_executors.request_scoped();
    let mut registry = registry
        .build_prepared_with_handlers(ctx.enriched_request.tools.as_mut(), &mut executors)
        .await?;
    let mut combined_output: Vec<_> = registry
        .mcp_list_tools_items()
        .iter()
        .map(list_tools_output_item)
        .collect();
    let mut combined_usage = None;

    for round in 0..MAX_GATEWAY_TOOL_ROUNDS {
        let compaction_usage = maybe_compact_context(&mut ctx, exec_ctx, auth).await?;
        accumulate_usage(&mut combined_usage, compaction_usage);
        let output_offset = combined_output.len();
        let (mut payload, deferred_stream_events): (ResponsePayload, Vec<_>) = if stream_upstream {
            let stream_payload = fetch_stream_payload(
                &ctx,
                exec_ctx,
                auth,
                &mut registry,
                stream
                    .as_mut()
                    .map(|(accumulator, sender)| (&mut **accumulator, *sender)),
                output_offset,
            )
            .await?;
            (stream_payload.payload, stream_payload.deferred_events)
        } else {
            (
                fetch_blocking_payload(&ctx, exec_ctx, auth, &registry).await?,
                Vec::new(),
            )
        };
        registry.restore_final_payload_output(&mut payload.output);
        accumulate_usage(&mut combined_usage, payload.usage.take());
        let current_output = std::mem::take(&mut payload.output);
        if matches!(payload.status.as_str(), "error" | "failed") {
            combined_output.extend(current_output);
            finalize_loop(&mut payload, combined_output, combined_usage, &ctx, &registry);
            return Ok((payload, ctx, registry));
        }
        log_custom_tool_calls(&current_output, &ctx.response_id);
        let has_client_owned = has_client_owned_calls(&current_output, &registry);
        let gateway_results = execute_and_emit_round_output_calls(
            &current_output,
            &registry,
            output_offset,
            deferred_stream_events,
            &ctx,
            stream
                .as_mut()
                .map(|(accumulator, sender)| (&mut **accumulator, *sender)),
        )
        .await?;
        combined_output.extend(public_output_items(&current_output, &registry, &gateway_results));

        if payload.status == "incomplete" {
            record_gateway_round_input(&mut ctx, &current_output, &registry, gateway_results);
            finalize_loop(&mut payload, combined_output, combined_usage, &ctx, &registry);
            return Ok((payload, ctx, registry));
        }

        match classify_round(has_client_owned, &gateway_results, round, MAX_GATEWAY_TOOL_ROUNDS) {
            // Client-owned calls (plain function or Codex namespace tools) are
            // handed back to the caller. Gateway calls in the same turn are
            // still recorded so the returned conversation is complete.
            LoopDecision::RequiresClientAction => {
                record_gateway_round_input(&mut ctx, &current_output, &registry, gateway_results);
                finalize_loop(&mut payload, combined_output, combined_usage, &ctx, &registry);
                return Ok((payload, ctx, registry));
            }
            // No gateway work remains — this turn is the final response.
            LoopDecision::Done => {
                finalize_loop(&mut payload, combined_output, combined_usage, &ctx, &registry);
                return Ok((payload, ctx, registry));
            }
            // Budget exhausted while the model was still requesting gateway
            // tools: surface the accumulated work as a partial
            // `status: "incomplete"` response instead of failing the request.
            // The final round's gateway calls and outputs are recorded so a
            // continuation is not fed a dangling tool call.
            LoopDecision::Incomplete(reason) => {
                record_gateway_round_input(&mut ctx, &current_output, &registry, gateway_results);
                finalize_loop(&mut payload, combined_output, combined_usage, &ctx, &registry);
                "incomplete".clone_into(&mut payload.status);
                payload.incomplete_details = Some(IncompleteDetails { reason: Some(reason) });
                return Ok((payload, ctx, registry));
            }
            // Gateway tools ran and rounds remain; feed outputs back and loop.
            LoopDecision::Continue => {
                ctx.enriched_request.tool_choice = Some(ToolChoice::Auto);
                append_output_items_to_input(&mut ctx.enriched_request.input, &current_output);
                record_gateway_round_input(&mut ctx, &current_output, &registry, gateway_results);
            }
        }
    }

    unreachable!("the final round returns Done, RequiresClientAction, or Incomplete");
}

fn record_gateway_round_input(
    ctx: &mut RequestContext,
    output: &[OutputItem],
    registry: &ToolRegistry,
    results: Vec<GatewayCallResult>,
) {
    append_gateway_calls_to_new_input(ctx, output, registry);
    append_tool_outputs(ctx, results.into_iter().map(|result| result.input_item).collect());
}

fn log_custom_tool_calls(output: &[OutputItem], response_id: &str) {
    for item in output {
        if let OutputItem::CustomToolCall(call) = item {
            debug!(
                response_id,
                call_id = %call.call_id,
                name = %call.name,
                input_bytes = call.input.len(),
                "custom tool call requires client execution"
            );
        }
    }
}

/// Codex CLI remote-compaction V2: the client appends a `compaction_trigger`
/// item to the input and expects the server to run its own summarization turn
/// and stream back exactly one `compaction` output item plus `response.completed`.
/// The trigger never reaches the upstream model; the summary inference is a
/// normal blocking call against the same backend as standalone compaction.
async fn run_compaction_trigger(
    mut ctx: RequestContext,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
) -> ExecutorResult<(ResponsePayload, RequestContext)> {
    let model = ctx.enriched_request.model.clone();
    let instructions = ctx.enriched_request.instructions.clone();
    let input = std::mem::replace(&mut ctx.enriched_request.input, ResponsesInput::Items(Vec::new()));
    let (mut compacted, usage) = compact_items(&model, input, instructions.as_deref(), exec_ctx, auth).await?;
    let Some(InputItem::Compaction(compaction)) = compacted.pop() else {
        unreachable!("compact_items always appends a compaction item");
    };
    ctx.new_input_items = compacted;
    let mut payload = ResponsePayload {
        id: ctx.response_id.clone(),
        object: "response".to_owned(),
        created_at: utcnow_str(),
        model,
        status: "completed".to_owned(),
        output: vec![OutputItem::Compaction(compaction)],
        usage: Some(usage),
        incomplete_details: None,
        error: None,
        previous_response_id: ctx.original_request.previous_response_id.clone(),
        conversation_id: ctx.conversation_id.clone(),
        instructions,
        tools: None,
        tool_choice: None,
    };
    ctx.inject_ids(&mut payload);
    Ok((payload, ctx))
}

async fn execute_and_emit_round_output_calls(
    output_items: &[OutputItem],
    registry: &ToolRegistry,
    output_offset: usize,
    deferred_events: Vec<EventFrame>,
    ctx: &RequestContext,
    stream: Option<(&mut GatewayStreamAccumulator, &mpsc::UnboundedSender<StreamEvent>)>,
) -> ExecutorResult<Vec<GatewayCallResult>> {
    match (deferred_events.is_empty(), stream) {
        (true, stream) => execute_and_emit_output_calls(output_items, registry, output_offset, stream).await,
        (false, Some((stream_accumulator, stream_sender))) => {
            execute_and_emit_ordered_output_calls(
                output_items,
                registry,
                output_offset,
                deferred_events,
                ctx,
                stream_accumulator,
                stream_sender,
            )
            .await
        }
        (false, None) => execute_and_emit_output_calls(output_items, registry, output_offset, None).await,
    }
}

async fn execute_and_emit_ordered_output_calls(
    output_items: &[OutputItem],
    registry: &ToolRegistry,
    output_offset: usize,
    deferred_events: Vec<EventFrame>,
    ctx: &RequestContext,
    stream_accumulator: &mut GatewayStreamAccumulator,
    stream_sender: &mpsc::UnboundedSender<StreamEvent>,
) -> ExecutorResult<Vec<GatewayCallResult>> {
    let mut events_by_output = Vec::with_capacity(output_items.len());
    events_by_output.resize_with(output_items.len(), Vec::new);
    let mut remaining_events = Vec::new();
    for frame in deferred_events {
        let Some(output_index) = frame
            .wire
            .output_index
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| *index < events_by_output.len())
        else {
            remaining_events.push(frame);
            continue;
        };
        events_by_output[output_index].push(frame);
    }

    let mut event_plans = gateway_event_plans(output_items, registry, output_offset);
    let first_gateway_index = output_items
        .iter()
        .position(|item| matches!(item, OutputItem::FunctionCall(call) if is_gateway_owned_call(call, registry)));
    let first_gateway_run_end = first_gateway_index
        .filter(|start| {
            !output_items[..*start]
                .iter()
                .any(|item| matches!(item, OutputItem::FunctionCall(call) if is_client_custom_call(call, registry)))
        })
        .map_or(0, |start| {
            output_items[start..]
                .iter()
                .take_while(
                    |item| matches!(item, OutputItem::FunctionCall(call) if is_gateway_owned_call(call, registry)),
                )
                .count()
                .saturating_add(start)
        });
    let first_gateway_run_len = first_gateway_run_end.saturating_sub(first_gateway_index.unwrap_or(0));
    emit_gateway_start_events(&event_plans[..first_gateway_run_len], stream_accumulator, stream_sender)?;

    let gateway_results = execute_output_calls(output_items, registry).await?;
    complete_gateway_event_plans(&mut event_plans, &gateway_results);
    let mut gateway_index = 0;
    for (index, item) in output_items.iter().enumerate() {
        if matches!(item, OutputItem::FunctionCall(call) if is_gateway_owned_call(call, registry)) {
            let plan = &event_plans[gateway_index..=gateway_index];
            let result = &gateway_results[gateway_index..=gateway_index];
            if index >= first_gateway_run_end {
                emit_gateway_start_events(plan, stream_accumulator, stream_sender)?;
            }
            emit_gateway_completed_events(result, plan, stream_accumulator, stream_sender)?;
            emit_deferred_stream_events(
                std::mem::take(&mut events_by_output[index]),
                ctx,
                registry,
                stream_accumulator,
                stream_sender,
                output_offset,
            )?;
            gateway_index += 1;
        } else {
            emit_deferred_stream_events(
                std::mem::take(&mut events_by_output[index]),
                ctx,
                registry,
                stream_accumulator,
                stream_sender,
                output_offset,
            )?;
        }
    }
    emit_deferred_stream_events(
        remaining_events,
        ctx,
        registry,
        stream_accumulator,
        stream_sender,
        output_offset,
    )?;
    Ok(gateway_results)
}

/// Move accumulated output/usage onto the terminating round's payload and
/// inject the response/conversation IDs. The payload's `model`/`created_at`/
/// `status` from the latest inference turn are preserved.
fn finalize_loop(
    payload: &mut ResponsePayload,
    combined_output: Vec<crate::types::io::OutputItem>,
    combined_usage: Option<ResponseUsage>,
    ctx: &RequestContext,
    registry: &ToolRegistry,
) {
    payload.output = combined_output;
    payload.usage = combined_usage;
    ctx.inject_ids(payload);
    if let Some(tools) = registry.tool_search_response_tools() {
        payload.tools = Some(tools);
        payload.tool_choice = Some(ctx.enriched_request.tool_choice.clone().unwrap_or_default());
    }
}

async fn run_blocking(
    ctx: RequestContext,
    registry: ToolRegistry,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
) -> ExecutorResult<ResponsePayload> {
    let (payload, ctx, registry) = run_until_gateway_tools_complete(ctx, registry, exec_ctx, auth, false, None).await?;

    let ch = exec_ctx.conv_handler.clone();
    let rh = exec_ctx.resp_handler.clone();
    persist_if_needed(payload.clone(), ctx, registry, ch, rh).await?;

    Ok(payload)
}

fn run_stream(
    ctx: RequestContext,
    registry: ToolRegistry,
    exec_ctx: Arc<ExecutionContext>,
    auth: Option<String>,
) -> BoxStream {
    Box::pin(stream! {
        let failure_context = StreamFailureContext::from(&ctx);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let exec_ctx_for_run = Arc::clone(&exec_ctx);
        let event_tx_for_run = event_tx.clone();
        let stream_accumulator = GatewayStreamAccumulator::new();
        let mut run_handle = AbortOnDrop::new(tokio::spawn(async move {
            let mut stream_accumulator = stream_accumulator;
            let result = run_until_gateway_tools_complete(
                ctx,
                registry,
                exec_ctx_for_run.as_ref(),
                auth.as_deref(),
                true,
                Some((&mut stream_accumulator, &event_tx_for_run)),
            )
            .await;
            (result, stream_accumulator)
        }));

        let mut next_sequence_number = 0;
        loop {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    yield consume_stream_event(event, &mut next_sequence_number);
                }
                result = &mut run_handle.handle => {
                    match result {
                        Err(e) => {
                            for chunk in panicked_stream_chunks(&e, &mut event_rx, &mut next_sequence_number) {
                                yield chunk;
                            }
                        }
                        Ok((Err(e), mut stream_accumulator)) => {
                            while let Ok(event) = event_rx.try_recv() {
                                yield consume_stream_event(event, &mut next_sequence_number);
                            }
                            if e.is_invalid_upstream_tool_search() {
                                let payload = failure_context.failed_payload(&e);
                                match stream_accumulator.terminal_response_chunk(&payload) {
                                    Ok(chunk) => yield chunk,
                                    Err(serialize_error) => yield stream_accumulator.executor_error_chunk(&serialize_error),
                                }
                            } else {
                                yield stream_accumulator.executor_error_chunk(&e);
                            }
                            yield DONE_MARKER.to_string();
                        }
                        Ok((Ok((payload, ctx, registry)), mut stream_accumulator)) => {
                            while let Ok(event) = event_rx.try_recv() {
                                yield consume_stream_event(event, &mut next_sequence_number);
                            }
                            // Codex may close its WebSocket as soon as it receives
                            // `response.completed`. Persist before exposing that
                            // event so a custom call/output continuation cannot be
                            // cancelled by the client disconnect.
                            let ch = exec_ctx.conv_handler.clone();
                            let rh = exec_ctx.resp_handler.clone();
                            let mut terminal_accumulator = stream_accumulator.clone();
                            let terminal_chunk = terminal_accumulator.terminal_response_chunk(&payload);
                            match persist_if_needed(payload, ctx, registry, ch, rh).await {
                                Ok(()) => match terminal_chunk {
                                    Ok(chunk) => yield chunk,
                                    Err(e) => yield stream_accumulator.executor_error_chunk(&e),
                                },
                                Err(e) => yield stream_accumulator.executor_error_chunk(&e),
                            }
                            yield DONE_MARKER.to_string();
                        }
                    }
                    break;
                }
            }
        }
    })
}

struct StreamFailureContext {
    response_id: String,
    conversation_id: Option<String>,
    model: String,
    previous_response_id: Option<String>,
    instructions: Option<String>,
}

impl From<&RequestContext> for StreamFailureContext {
    fn from(ctx: &RequestContext) -> Self {
        Self {
            response_id: ctx.response_id.clone(),
            conversation_id: ctx.conversation_id.clone(),
            model: ctx.enriched_request.model.clone(),
            previous_response_id: ctx.original_request.previous_response_id.clone(),
            instructions: ctx.original_request.instructions.clone(),
        }
    }
}

impl StreamFailureContext {
    fn failed_payload(&self, error: &ExecutorError) -> ResponsePayload {
        ResponsePayload {
            id: self.response_id.clone(),
            object: "response".to_owned(),
            created_at: utcnow_str(),
            model: self.model.clone(),
            status: "failed".to_owned(),
            output: Vec::new(),
            usage: None,
            incomplete_details: None,
            error: Some(serde_json::json!({
                "message": error.error_message(),
                "type": error.error_type(),
                "code": error.error_code(),
            })),
            previous_response_id: self.previous_response_id.clone(),
            conversation_id: self.conversation_id.clone(),
            instructions: self.instructions.clone(),
            tools: None,
            tool_choice: None,
        }
    }
}

fn consume_stream_event(event: StreamEvent, next_sequence_number: &mut u64) -> String {
    *next_sequence_number = event.sequence_number.saturating_add(1);
    event.content
}

fn stream_task_failure_chunk(error: &tokio::task::JoinError, sequence_number: u64) -> String {
    error_sse_chunk(&format!("stream task failed: {error}"), sequence_number)
}

fn panicked_stream_chunks(
    error: &tokio::task::JoinError,
    event_rx: &mut mpsc::UnboundedReceiver<StreamEvent>,
    next_sequence_number: &mut u64,
) -> Vec<String> {
    let mut chunks = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        chunks.push(consume_stream_event(event, next_sequence_number));
    }
    chunks.push(stream_task_failure_chunk(error, *next_sequence_number));
    chunks.push(DONE_MARKER.to_owned());
    chunks
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

/// Builder for a stateful conversation turn.
///
/// ```ignore
/// ExecuteRequest::new(payload, exec_ctx).with_auth(token).run().await
/// ```
pub struct ExecuteRequest {
    payload: RequestPayload,
    exec_ctx: Arc<ExecutionContext>,
    client_auth: Option<String>,
}

impl ExecuteRequest {
    #[must_use]
    pub fn new(payload: RequestPayload, exec_ctx: Arc<ExecutionContext>) -> Self {
        Self {
            payload,
            exec_ctx,
            client_auth: None,
        }
    }

    /// Override the bearer token for this request only; does not touch the shared [`ExecutionContext`].
    #[must_use]
    pub fn with_auth(mut self, token: Option<String>) -> Self {
        self.client_auth = token;
        self
    }

    /// Execute one stateful conversation turn.
    ///
    /// Returns `Either::Left(ResponsePayload)` for non-streaming requests, or
    /// `Either::Right(BoxStream)` for streaming, where each yielded `String` is
    /// a complete SSE frame ready to forward to the client.
    ///
    /// # Errors
    /// Returns [`ExecutorError`] if rehydration or (non-streaming) LLM inference fails.
    pub async fn run(self) -> ExecutorResult<Either<ResponsePayload, BoxStream>> {
        debug!(
            model = %self.payload.model,
            store = self.payload.store,
            stream = self.payload.stream,
            has_previous_response_id = self.payload.previous_response_id.is_some(),
            has_conversation_id = self.payload.conversation_id.is_some(),
            tools = self.payload.tools.as_ref().map_or(0, Vec::len),
            "executor received responses request"
        );
        let ctx = rehydrate_conversation(self.payload, &self.exec_ctx).await?;
        let (ctx, registry) =
            prepare_request_tools(ctx, &self.exec_ctx.conv_handler, &self.exec_ctx.resp_handler).await?;
        if ctx.original_request.stream {
            Ok(Either::Right(run_stream(
                ctx,
                registry,
                self.exec_ctx,
                self.client_auth,
            )))
        } else {
            Ok(Either::Left(
                run_blocking(ctx, registry, &self.exec_ctx, self.client_auth.as_deref()).await?,
            ))
        }
    }
}

/// Execute one stateful conversation turn.
///
/// Thin shim over [`ExecuteRequest`] for callers that don't need per-request auth override.
///
/// # Errors
/// Returns [`ExecutorError`] if rehydration or (non-streaming) LLM inference fails.
pub async fn execute(
    request: RequestPayload,
    exec_ctx: Arc<ExecutionContext>,
) -> ExecutorResult<Either<ResponsePayload, BoxStream>> {
    ExecuteRequest::new(request, exec_ctx).run().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::modes::{ConversationHandler, ResponseHandler};
    use crate::storage::{ConversationStore, InOutItem, ResponseStore, create_pool_with_schema};
    use futures::StreamExt;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn summary_upstream_response() -> serde_json::Value {
        serde_json::json!({
            "id": "resp_upstream",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [{
                "id": "msg_upstream",
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": "durable summary",
                    "annotations": []
                }]
            }],
            "usage": {
                "input_tokens": 12,
                "output_tokens": 3,
                "total_tokens": 15
            },
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
    }

    async fn trigger_execution_context(
        captured: Arc<Mutex<Option<serde_json::Value>>>,
    ) -> (ExecutionContext, tokio::task::JoinHandle<()>) {
        let captured_for_route = Arc::clone(&captured);
        let app = axum::Router::new().route(
            "/v1/responses",
            axum::routing::post(move |body: axum::body::Bytes| async move {
                let value =
                    serde_json::from_slice::<serde_json::Value>(&body).expect("captured upstream body must be JSON");
                *captured_for_route.lock().await = Some(value);
                axum::Json(summary_upstream_response())
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock inference server");
        let address = listener.local_addr().expect("mock server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let exec_ctx = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::disabled()),
            ResponseHandler::new(ResponseStore::disabled()),
            Arc::new(reqwest::Client::new()),
            format!("http://{address}"),
        );
        (exec_ctx, server)
    }

    #[tokio::test]
    async fn compaction_trigger_returns_single_compaction_item_without_upstream_trigger() {
        let captured = Arc::new(Mutex::new(None));
        let (exec_ctx, server) = trigger_execution_context(Arc::clone(&captured)).await;

        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "stream": false,
            "store": false,
            "input": [
                {"role": "user", "content": "remember banana"},
                {"type": "compaction_trigger"}
            ]
        }))
        .expect("valid trigger request");
        let Either::Left(response) = ExecuteRequest::new(payload, Arc::new(exec_ctx))
            .run()
            .await
            .expect("trigger request succeeds")
        else {
            panic!("non-streaming trigger request must return a payload");
        };

        assert_eq!(response.status, "completed");
        assert_eq!(response.output.len(), 1);
        let OutputItem::Compaction(item) = &response.output[0] else {
            panic!("expected exactly one compaction output item");
        };
        assert_eq!(item.encrypted_content, "durable summary");
        assert!(item.id.as_deref().is_some_and(|id| id.starts_with("cmp_")));
        assert_eq!(response.usage.as_ref().map(|usage| usage.total_tokens), Some(15));

        let upstream = captured.lock().await.take().expect("summary inference ran");
        assert!(
            !upstream.to_string().contains("compaction_trigger"),
            "trigger must never reach the upstream model"
        );
        assert!(upstream.to_string().contains("CONTEXT CHECKPOINT COMPACTION"));
        server.abort();
    }

    #[tokio::test]
    async fn compaction_trigger_persists_checkpoint_only_as_output() {
        let captured = Arc::new(Mutex::new(None));
        let (mut exec_ctx, server) = trigger_execution_context(Arc::clone(&captured)).await;
        let pool = create_pool_with_schema(Some("sqlite::memory:"))
            .await
            .expect("create response store");
        let response_store = ResponseStore::new(pool);
        exec_ctx.resp_handler = ResponseHandler::new(response_store.clone());

        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "store": true,
            "input": [
                {"role": "user", "content": "remember banana"},
                {"type": "compaction_trigger"}
            ]
        }))
        .expect("valid trigger request");
        let Either::Left(response) = ExecuteRequest::new(payload, Arc::new(exec_ctx))
            .run()
            .await
            .expect("trigger request succeeds")
        else {
            panic!("non-streaming trigger request must return a payload");
        };

        let history = response_store
            .rehydrate(&response.id)
            .await
            .expect("compaction trigger response rehydrates");
        assert_eq!(history.len(), 2);
        assert!(matches!(history[0], InOutItem::Input(InputItem::Message(_))));
        assert!(matches!(history[1], InOutItem::Output(OutputItem::Compaction(_))));

        let model_input = ResponsesInput::Items(InOutItem::into_input_items(history));
        let serialized = serde_json::to_value(model_input.model_input()).expect("model input serializes");
        assert_eq!(serialized.as_array().map(Vec::len), Some(2));
        assert_eq!(serialized[0]["content"], "remember banana");
        assert_eq!(serialized[1]["role"], "assistant");
        assert_eq!(serialized[1]["content"][0]["text"], "durable summary");
        server.abort();
    }

    #[tokio::test]
    async fn compaction_trigger_lowers_tool_search_history_and_retains_loaded_metadata() {
        let captured = Arc::new(Mutex::new(None));
        let (mut exec_ctx, server) = trigger_execution_context(Arc::clone(&captured)).await;
        let pool = create_pool_with_schema(Some("sqlite::memory:"))
            .await
            .expect("create response store");
        let response_store = ResponseStore::new(pool);
        exec_ctx.resp_handler = ResponseHandler::new(response_store.clone());

        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "store": true,
            "parallel_tool_calls": false,
            "tools": [
                {
                    "type": "tool_search",
                    "execution": "client",
                    "description": "Find a tool",
                    "parameters": {"type": "object"}
                },
                {
                    "type": "function",
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object"},
                    "defer_loading": true
                }
            ],
            "input": [
                {"role": "user", "content": "find weather"},
                {
                    "type": "tool_search_call",
                    "id": "tsc_search",
                    "call_id": "call_search",
                    "arguments": {"query": "weather"}
                },
                {
                    "type": "tool_search_output",
                    "call_id": "call_search",
                    "tools": [{
                        "type": "function",
                        "name": "get_weather",
                        "description": "Get weather",
                        "parameters": {"type": "object"},
                        "defer_loading": true
                    }]
                },
                {"type": "compaction_trigger"}
            ]
        }))
        .expect("valid tool-search trigger request");
        let Either::Left(response) = ExecuteRequest::new(payload, Arc::new(exec_ctx))
            .run()
            .await
            .expect("tool-search trigger request succeeds")
        else {
            panic!("non-streaming trigger request must return a payload");
        };

        let [OutputItem::Compaction(compaction)] = response.output.as_slice() else {
            panic!("tool-search compaction trigger must return one compaction item");
        };
        assert_eq!(compaction.encrypted_content, "durable summary");

        let upstream = captured.lock().await.take().expect("summary inference ran");
        let upstream_input = upstream["input"].as_array().expect("summary input items");
        assert!(upstream_input.iter().any(|item| {
            item["type"] == "function_call" && item["name"] == "tool_search" && item["call_id"] == "call_search"
        }));
        assert!(
            upstream_input
                .iter()
                .any(|item| { item["type"] == "function_call_output" && item["call_id"] == "call_search" })
        );
        assert!(upstream_input.iter().all(|item| {
            !matches!(
                item["type"].as_str(),
                Some("tool_search_call" | "tool_search_output" | "compaction_trigger")
            )
        }));

        let stored = response_store.get(&response.id).await.expect("stored trigger response");
        let loaded = stored
            .metadata
            .tool_search_loaded_tools
            .as_deref()
            .expect("loaded tool metadata retained");
        assert_eq!(loaded.len(), 1);
        let loaded = serde_json::to_value(&loaded[0]).expect("loaded tool serializes");
        assert_eq!(loaded["name"], "get_weather");
        assert_eq!(loaded["defer_loading"], true);
        server.abort();
    }

    #[tokio::test]
    async fn compaction_trigger_streams_one_compaction_item_then_completed() {
        let captured = Arc::new(Mutex::new(None));
        let (exec_ctx, server) = trigger_execution_context(Arc::clone(&captured)).await;

        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "stream": true,
            "store": false,
            "input": [
                {"role": "user", "content": "remember banana"},
                {"type": "compaction_trigger"}
            ]
        }))
        .expect("valid trigger request");
        let Either::Right(stream) = ExecuteRequest::new(payload, Arc::new(exec_ctx))
            .run()
            .await
            .expect("trigger request succeeds")
        else {
            panic!("streaming trigger request must return a stream");
        };

        let chunks: Vec<String> = stream.collect().await;
        let mut event_types = Vec::new();
        let mut compaction_done_count = 0;
        for chunk in chunks {
            let body = chunk.strip_suffix("\n\n").expect("SSE frame terminator");
            let data = body
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("SSE data line");
            let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
                continue; // terminal [DONE] marker
            };
            let event_type = event["type"].as_str().expect("event type");
            event_types.push(event_type.to_owned());
            if matches!(event_type, "response.created" | "response.in_progress") {
                assert_eq!(event["response"]["output"], serde_json::json!([]));
                assert!(event["response"]["usage"].is_null());
            }
            if event_type == "response.output_item.done" && event["item"]["type"] == "compaction" {
                compaction_done_count += 1;
                assert_eq!(event["item"]["encrypted_content"], "durable summary");
            }
        }

        assert_eq!(
            event_types,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(compaction_done_count, 1);
        assert!(
            !captured
                .lock()
                .await
                .take()
                .expect("summary inference ran")
                .to_string()
                .contains("compaction_trigger")
        );
        server.abort();
    }

    #[tokio::test]
    async fn stream_task_panic_after_event_uses_next_sequence_number_for_error() {
        let accumulator = GatewayStreamAccumulator::new();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut accumulator = accumulator;
            let event = accumulator
                .process_sse_line(r#"data: {"type":"response.created"}"#, 0)
                .expect("event should be emitted");
            event_tx
                .send(StreamEvent {
                    content: "event".to_owned(),
                    sequence_number: event.sequence_number().expect("event should be numbered"),
                })
                .expect("test receiver should remain open");
            panic!("test task panic");
        });

        let error = task.await.expect_err("task should panic");
        let mut next_sequence_number = 0;
        let chunks = panicked_stream_chunks(&error, &mut event_rx, &mut next_sequence_number);
        let mut error_lines = chunks[1].lines();
        assert_eq!(error_lines.next(), Some("event: error"));
        let error_data = error_lines
            .next()
            .and_then(|line| line.strip_prefix("data: "))
            .expect("SSE data");
        assert!(error_lines.all(str::is_empty), "unexpected SSE frame content");
        let error_event: serde_json::Value =
            serde_json::from_str(error_data).expect("error chunk should be valid JSON");

        assert_eq!(chunks[0], "event");
        assert_eq!(error_event["type"], "error");
        assert_eq!(error_event["sequence_number"], 1);
        assert_eq!(chunks[2], DONE_MARKER);
    }
}
