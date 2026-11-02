mod estimate;
use crate::executor::inference::UpstreamHeaders;
pub(crate) use estimate::{estimate_history_tokens, estimate_input_tokens};

mod carry_over;
mod context;
mod summary;

pub(crate) use carry_over::CompactionCarryOver;
use carry_over::public_calls;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use either::Either;

use super::telemetry::FailureCategory;
use super::telemetry::metrics::Stage;
use super::telemetry::stages::CompactionTrigger;
use context::item_has_meaningful_context;
use summary::completed_summary_text;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::pending_calls::resolved_prefix_len;
use crate::executor::persist::persist_prepared_turn;
use crate::executor::pipeline::AgentPipeline;
use crate::executor::prepare::prepare_request_tools;
use crate::executor::rehydrate::rehydrate_conversation;
use crate::executor::relay::StreamRelay;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::upstream::fetch_blocking_payload;
use crate::tool::{CodexNamespaceHandler, ToolSearchState};
use crate::types::event::MessageStatus;
use crate::types::io::input::{InputTextContent, latest_compaction_window};
use crate::types::io::{
    CompactionItem, InOutItem, InputContent, InputItem, InputMessage, InputMessageContent, ResponseUsage,
    ResponsesInput,
};
use crate::types::request_response::{CompactRequest, CompactedResponse, RequestPayload};
use crate::utils::common::{utcnow_str, uuid7_str};

const COMPACTION_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a concise handoff summary that preserves current progress, decisions, constraints, unresolved work, and critical references for the next model. Return only the summary.";

fn retained_user_window(items: &[InputItem]) -> Vec<InputItem> {
    let window = latest_compaction_window(items);
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let InputItem::Message(message) = item else {
                return None;
            };
            if message.role != "user"
                || window.is_some_and(|window| index < window.latest_index() && !window.retains_user_item(index, item))
            {
                return None;
            }
            let mut retained = message.clone();
            retained.id = Some(uuid7_str("msg_"));
            retained.status = Some(MessageStatus::Completed);
            if let InputMessageContent::Text(text) = &retained.content {
                retained.content =
                    InputMessageContent::Parts(vec![InputContent::InputText(InputTextContent::new(text.clone()))]);
            }
            Some(InputItem::Message(retained))
        })
        .collect()
}

fn finish_compacted_window(mut output: Vec<InputItem>, summary: String) -> Vec<InputItem> {
    output.push(InputItem::Compaction(CompactionItem {
        agent: None,
        id: Some(uuid7_str("cmp_")),
        encrypted_content: summary,
    }));
    output
}

#[cfg(test)]
fn build_compacted_window(items: &[InputItem], summary: String) -> Vec<InputItem> {
    finish_compacted_window(retained_user_window(items), summary)
}

fn request_payload(model: String, input: ResponsesInput, instructions: Option<String>) -> RequestPayload {
    RequestPayload {
        model,
        input,
        instructions,
        previous_response_id: None,
        conversation: None,
        tools: None,
        tool_choice: None,
        stream: false,
        store: false,
        include: None,
        reasoning: None,
        text: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        max_tool_calls: None,
        ignore_eos: None,
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        prompt_cache_options: None,
        service_tier: None,
        cache_salt: None,
        multi_agent: None,
        context_management: None,
        background: None,
        prompt: None,
        extra: HashMap::new(),
    }
}

/// Summarize an already-resolved item history and return its canonical compacted window.
/// Normalize history against the source declarations before removing tools from the summary request.
///
/// # Errors
///
/// Returns an invalid-request error for empty input, an upstream error for an unusable model
/// summary, and propagates inference and serialization failures.
pub(crate) async fn compact_items(
    request: &RequestPayload,
    input: ResponsesInput,
    exec_ctx: &ExecutionContext,
    auth: Option<&UpstreamHeaders>,
) -> ExecutorResult<(Vec<InputItem>, ResponseUsage, Option<String>)> {
    compact_items_with_trigger(request, input, exec_ctx, auth, CompactionTrigger::InputItem).await
}

#[tracing::instrument(name = "agentic.compaction", skip_all, fields(
    agentic.compaction.operation = "summarize", agentic.compaction.trigger = trigger.as_str()
))]
async fn compact_items_with_trigger(
    request: &RequestPayload,
    input: ResponsesInput,
    exec_ctx: &ExecutionContext,
    auth: Option<&UpstreamHeaders>,
    trigger: CompactionTrigger,
) -> ExecutorResult<(Vec<InputItem>, ResponseUsage, Option<String>)> {
    let timer = exec_ctx.metrics.stage(Stage::Compaction);
    let summarized = summarize_items(request, input, exec_ctx, auth).await;
    timer.finish_result(&summarized);
    summarized
}

async fn summarize_items(
    request: &RequestPayload,
    input: ResponsesInput,
    exec_ctx: &ExecutionContext,
    auth: Option<&UpstreamHeaders>,
) -> ExecutorResult<(Vec<InputItem>, ResponseUsage, Option<String>)> {
    request.validate_prompt_cache_options(exec_ctx.responses_config.prompt_cache_options_supported)?;
    let original_items = Vec::from(input);
    if !original_items.iter().any(item_has_meaningful_context) {
        return Err(ExecutorError::InvalidRequest(
            "compaction requires non-empty input or previous_response_id context".to_owned(),
        ));
    }

    let compacted = retained_user_window(&original_items);
    let mut summary_items: Vec<InputItem> = original_items
        .into_iter()
        .filter(|item| !item.is_compaction_trigger())
        .collect();
    summary_items.push(InputItem::Message(InputMessage {
        role: "user".to_owned(),
        content: InputMessageContent::Text(COMPACTION_PROMPT.to_owned()),
        ..Default::default()
    }));
    let instructions = request.instructions.clone();
    let original_request = request_payload(
        request.model.clone(),
        ResponsesInput::Items(Vec::new()),
        instructions.clone(),
    );
    let mut enriched_request =
        request_payload(request.model.clone(), ResponsesInput::from(summary_items), instructions);
    // The summary offers no tools. Resolve searched top-level function names from the source before discarding
    // its declarations, while preserving actual namespace members in the summary's public history.
    let names = request.top_level_function_names();
    let functions = names.iter().map(String::as_str).collect();
    enriched_request.input = CodexNamespaceHandler
        .resolve_input(None, &functions, Cow::Owned(enriched_request.input))
        .into_owned();
    enriched_request.prompt_cache_key.clone_from(&request.prompt_cache_key);
    enriched_request.prompt_cache_retention = request.prompt_cache_retention;
    enriched_request.prompt_cache_options = request.prompt_cache_options;
    enriched_request.service_tier.clone_from(&request.service_tier);
    let ctx = RequestContext {
        multi_agent_tree: None,
        original_request,
        enriched_request,
        new_input_items: Vec::new(),
        response_id: uuid7_str("resp_"),
        conversation_id: None,
        conversation_version: None,
        continuation: None,
    };
    let mut agent = AgentPipeline::new(ctx, None, StreamRelay::detached());
    let response =
        fetch_blocking_payload(&mut agent, exec_ctx, auth, &crate::tool::ToolRegistry::default(), None).await?;
    exec_ctx.metrics.record_response_usage(response.usage.as_ref());
    let summary = completed_summary_text(&response)?;

    Ok((
        finish_compacted_window(compacted, summary),
        response.usage.unwrap_or_default(),
        response.service_tier,
    ))
}

/// Apply the first configured compaction threshold to the resolved request history.
///
/// Returns the summarization usage when compaction ran. The configuration remains on the
/// client-facing request context but is never part of [`crate::types::request_response::UpstreamRequest`].
///
/// # Errors
///
/// Propagates inference errors from the summarization request.
pub(crate) async fn maybe_compact_context(
    ctx: &mut RequestContext,
    exec_ctx: &ExecutionContext,
    auth: Option<&UpstreamHeaders>,
) -> ExecutorResult<Option<ResponseUsage>> {
    let threshold = ctx
        .enriched_request
        .context_management
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|entry| entry.type_ == "compaction")
        .and_then(|entry| entry.compact_threshold);
    let Some(threshold) = threshold else {
        return Ok(None);
    };
    let estimated_tokens = estimate_input_tokens(&ctx.enriched_request.input);
    if estimated_tokens <= threshold {
        return Ok(None);
    }

    tracing::debug!(
        estimated_tokens,
        threshold,
        "automatically compacting resolved response input"
    );
    // Commit the replacement only after a usable summary is available.
    let items = Vec::from(&ctx.enriched_request.input);
    let multi_agent = ctx
        .enriched_request
        .multi_agent
        .as_ref()
        .is_some_and(|config| config.enabled);
    let prefix = if multi_agent {
        resolved_prefix_len(&items)?
    } else {
        items.len()
    };
    if !items[..prefix].iter().any(item_has_meaningful_context) {
        return Ok(None);
    }
    // Single-agent compaction covers every item. Items stored again come from the request's own
    // public items; the model input may keep a lowered call, which the model sees lowered anyway.
    let compacted_items = match (&ctx.enriched_request.input, multi_agent) {
        (input @ ResponsesInput::Items(_), false) => Either::Left(input.input_items()),
        _ => Either::Right(items[..prefix].iter().map(Cow::Borrowed)),
    };
    let own_items = ctx.new_input_items.iter().filter_map(InOutItem::as_input);
    let suffix = if multi_agent { &items[prefix..] } else { &[][..] };
    let CompactionCarryOver {
        open_calls,
        stored: carried,
    } = CompactionCarryOver::new(compacted_items, own_items, suffix);
    let input = ResponsesInput::from(items[..prefix].to_vec());
    let (mut compacted, usage, _) = compact_items_with_trigger(
        &ctx.enriched_request,
        input,
        exec_ctx,
        auth,
        CompactionTrigger::ContextManagement,
    )
    .await?;
    if multi_agent {
        compacted.extend(
            items[..prefix]
                .iter()
                .filter(|item| matches!(item, InputItem::McpListTools(_)))
                .cloned(),
        );
    }
    let mut model_input = compacted.clone();
    model_input.extend(open_calls);
    model_input.extend_from_slice(suffix);
    let mut stored = carried.insert_into(compacted);
    stored.extend_from_slice(suffix);
    ctx.enriched_request.input = ResponsesInput::from(model_input);
    ctx.new_input_items = stored.into_iter().map(Into::into).collect();
    if let Some(continuation) = &mut ctx.continuation {
        continuation.mark_history_replaced();
    }
    Ok(Some(usage))
}

/// Compact direct input or a stored previous-response chain into a reusable item window.
///
/// # Errors
///
/// Returns an invalid-request error when neither input nor a previous response ID is supplied,
/// and propagates history, inference, and persistence failures.
pub async fn compact_response(
    request: CompactRequest,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
) -> ExecutorResult<CompactedResponse> {
    let auth = auth.map(|token| UpstreamHeaders::Bearer(token.to_owned()));
    if request.input.is_none() && request.previous_response_id.is_none() {
        return Err(ExecutorError::InvalidRequest(
            "compaction requires input or previous_response_id".to_owned(),
        ));
    }

    let mut payload = request_payload(
        request.model,
        request.input.unwrap_or_else(|| ResponsesInput::Items(Vec::new())),
        request.instructions,
    );
    payload.previous_response_id = request.previous_response_id;
    payload.service_tier = request.service_tier;
    payload.prompt_cache_retention = request.prompt_cache_retention;
    payload.prompt_cache_options = request.prompt_cache_options;
    payload.prompt_cache_key = request.prompt_cache_key;
    let ctx = rehydrate_conversation(payload, exec_ctx).await?;
    // OpenAI excludes multi-agent execution from the standalone compact endpoint,
    // including stored trees and public history attributed to a multi-agent run.
    if ctx.multi_agent_tree.is_some() || ctx.enriched_request.input.has_multi_agent_history() {
        return Err(ExecutorError::InvalidRequest(
            "/responses/compact is not supported with multi_agent".into(),
        ));
    }
    let (mut ctx, tool_search_state) =
        prepare_request_tools(ctx, &exec_ctx.conv_handler, &exec_ctx.resp_handler).await?;
    let tool_search_metadata = tool_search_state.map(ToolSearchState::into_public_metadata);
    let input = std::mem::replace(&mut ctx.enriched_request.input, ResponsesInput::Items(Vec::new()));
    // The returned window keeps every open async call, in its public form, so a client replaying it
    // stays self-contained; storage adds only this request's own items (see `CompactionCarryOver`).
    let CompactionCarryOver {
        open_calls,
        stored: carried,
    } = CompactionCarryOver::new(
        input.input_items(),
        ctx.new_input_items.iter().filter_map(InOutItem::as_input),
        &[],
    );
    // A history call's public identity comes from what was stored, not from the tools this request
    // declares: the model input above lowered it, and the declarations may have changed since.
    // History is read again only when an open call came from it.
    let from_history: HashSet<&str> = open_calls
        .iter()
        .filter_map(InputItem::call_id)
        .filter(|call_id| !ctx.new_input_items.iter().any(|own| own.call_id() == Some(call_id)))
        .collect();
    let mut history_calls = if from_history.is_empty() {
        HashMap::new()
    } else {
        public_calls(exec_ctx.resp_handler.rehydrate(&ctx).await?, &from_history)
    };
    let (compacted, usage, service_tier) = compact_items_with_trigger(
        &ctx.enriched_request,
        input,
        exec_ctx,
        auth.as_ref(),
        CompactionTrigger::Explicit,
    )
    .await?;
    // A call this request supplied is already public; a call from history takes its stored form.
    let public_call = |item: InputItem| -> InputItem {
        let public = item.call_id().and_then(|call_id| {
            ctx.new_input_items
                .iter()
                .filter_map(InOutItem::as_input)
                .find(|own| own.call_id() == Some(call_id))
                .map(Cow::into_owned)
                .or_else(|| history_calls.remove(call_id))
        });
        public.unwrap_or(item)
    };
    let mut output = compacted.clone();
    output.extend(open_calls.into_iter().map(public_call));

    let response_id = ctx.response_id.clone();
    ctx.new_input_items = carried.insert_into(compacted).into_iter().map(Into::into).collect();
    let timer = exec_ctx.metrics.stage(Stage::Persist);
    match persist_prepared_turn(
        ctx,
        tool_search_metadata,
        Vec::new(),
        None,
        &exec_ctx.conv_handler,
        &exec_ctx.resp_handler,
    )
    .await
    {
        Ok(()) => timer.finish(None),
        // Without storage nothing was written, so there is no stage to time.
        Err(ExecutorError::Storage(crate::StorageError::NotConfigured)) => timer.discard(),
        Err(error) => {
            timer.finish(Some(FailureCategory::from(&error)));
            return Err(error);
        }
    }

    Ok(CompactedResponse {
        service_tier,
        id: response_id,
        object: "response.compaction".to_owned(),
        created_at: utcnow_str(),
        output,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::estimate::ESTIMATED_IMAGE_TOKENS;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::routing::post;

    use crate::executor::modes::{ConversationHandler, ResponseHandler};
    use crate::executor::request::{ExecutionContext, RequestContext};
    use crate::storage::{ConversationStore, ResponseMetadata, ResponseStore, create_pool_with_schema};
    use crate::types::event::MessageStatus;
    use crate::types::io::InOutItem;
    use crate::types::io::{
        CompactionItem, CustomToolCallOutputMessage, FunctionToolResultMessage, InputContent, InputImageContent,
        InputItem, InputMessage, InputMessageContent, ResponsesInput, ToolCallOutput, ToolOutputContent,
    };
    use crate::types::request_response::ContextManagement;

    use super::{
        build_compacted_window, compact_response, completed_summary_text, estimate_history_tokens,
        estimate_input_tokens, maybe_compact_context, request_payload,
    };

    fn user_message(text: &str) -> InputItem {
        InputItem::Message(InputMessage {
            role: "user".to_owned(),
            content: InputMessageContent::Text(text.to_owned()),
            ..Default::default()
        })
    }

    fn inline_image(encoded_bytes: usize) -> InputImageContent {
        InputImageContent {
            image_url: Some(format!("data:image/png;base64,{}", "A".repeat(encoded_bytes))),
            detail: Some("auto".to_owned()),
            ..InputImageContent::default()
        }
    }

    fn image_message(encoded_bytes: usize) -> InputItem {
        InputItem::Message(InputMessage {
            role: "user".to_owned(),
            content: InputMessageContent::Parts(vec![InputContent::InputImage(inline_image(encoded_bytes))]),
            ..Default::default()
        })
    }

    fn function_image_output(encoded_bytes: usize) -> InputItem {
        InputItem::FunctionCallOutput(FunctionToolResultMessage {
            id: None,
            status: None,
            call_id: "call_view_image".to_owned(),
            output: ToolCallOutput::Content(vec![ToolOutputContent::InputImage(inline_image(encoded_bytes))]),
        })
    }

    fn custom_image_output(encoded_bytes: usize) -> InputItem {
        InputItem::CustomToolCallOutput(CustomToolCallOutputMessage {
            status: None,
            id: None,
            call_id: "call_view_image".to_owned(),
            name: Some("view_image".to_owned()),
            output: ToolCallOutput::Content(vec![ToolOutputContent::InputImage(inline_image(encoded_bytes))]),
        })
    }

    fn context_with_threshold(input: ResponsesInput, threshold: u64) -> RequestContext {
        let original_request = request_payload("test-model".to_owned(), ResponsesInput::Items(Vec::new()), None);
        let mut enriched_request = request_payload("test-model".to_owned(), input, None);
        enriched_request.context_management = Some(vec![ContextManagement {
            type_: "compaction".to_owned(),
            compact_threshold: Some(threshold),
        }]);
        RequestContext {
            multi_agent_tree: None,
            original_request,
            enriched_request,
            new_input_items: Vec::new(),
            response_id: "resp_test".to_owned(),
            conversation_id: None,
            conversation_version: None,
            continuation: None,
        }
    }

    fn assert_text_growth(cases: impl IntoIterator<Item = (&'static str, serde_json::Value, serde_json::Value)>) {
        for (label, short, long) in cases {
            let short: ResponsesInput = serde_json::from_value(short).expect("valid short input");
            let long: ResponsesInput = serde_json::from_value(long).expect("valid long input");
            assert!(
                estimate_input_tokens(&long) > estimate_input_tokens(&short),
                "{label} should contribute to the estimate"
            );
        }
    }

    fn summary_response() -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
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
        }))
    }

    async fn mock_execution_context(
        response_store: ResponseStore,
    ) -> (
        ExecutionContext,
        Arc<Mutex<Vec<serde_json::Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().route(
            "/v1/responses",
            post({
                let requests = Arc::clone(&requests);
                move |body: axum::body::Bytes| {
                    let requests = Arc::clone(&requests);
                    async move {
                        let request = serde_json::from_slice(&body).expect("valid request JSON");
                        requests.lock().expect("request capture lock").push(request);
                        summary_response()
                    }
                }
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
            ResponseHandler::new(response_store),
            Arc::new(reqwest::Client::new()),
            format!("http://{address}"),
        );
        (exec_ctx, requests, server)
    }

    fn compact_request() -> crate::CompactRequest {
        serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "input": [{"role": "user", "content": "important context"}]
        }))
        .expect("valid compact request")
    }

    #[test]
    fn compacted_window_retains_user_messages_and_replaces_old_compaction() {
        let items = vec![
            InputItem::Compaction(CompactionItem {
                agent: None,
                id: Some("cmp_old".to_owned()),
                encrypted_content: "old summary".to_owned(),
            }),
            user_message("first"),
            InputItem::FunctionCallOutput(FunctionToolResultMessage {
                id: None,
                status: None,
                call_id: "call_1".to_owned(),
                output: "tool output".into(),
            }),
            user_message("second"),
        ];

        let output = build_compacted_window(&items, "new summary".to_owned());

        assert_eq!(
            output
                .iter()
                .filter(|item| matches!(item, InputItem::Message(_)))
                .count(),
            2
        );
        assert_eq!(
            output
                .iter()
                .filter(|item| matches!(item, InputItem::Compaction(_)))
                .count(),
            1
        );
        assert!(matches!(output.last(), Some(InputItem::Compaction(_))));
        for item in output.iter().filter_map(|item| match item {
            InputItem::Message(message) => Some(message),
            _ => None,
        }) {
            assert_eq!(item.status, Some(MessageStatus::Completed));
            assert!(item.id.as_deref().is_some_and(|id| id.starts_with("msg_")));
        }
    }

    #[test]
    fn token_estimate_counts_large_json_numbers() {
        assert_text_growth([(
            "tool-search argument numbers",
            serde_json::json!([{
                "type": "tool_search_call", "id": "ts_1", "call_id": "call_1",
                "arguments": {"values": vec![1_u64; 64]}
            }]),
            serde_json::json!([{
                "type": "tool_search_call", "id": "ts_1", "call_id": "call_1",
                "arguments": {"values": vec![u64::MAX; 64]}
            }]),
        )]);
    }

    /// A search output counts as the text the model reads: functions with their schemas, namespaces by name and
    /// description. A loaded namespace's members reach the model as tool declarations, not in the input.
    #[test]
    fn token_estimate_counts_the_search_output_the_model_reads() {
        let base = serde_json::json!([{
            "type": "tool_search_output", "call_id": "call_1", "tools": [
                {
                    "type": "namespace", "name": "workspace", "description": "x", "extra_context": "x",
                    "tools": [{"type": "function", "name": "lookup", "description": "x", "parameters": {"type": "object"}}]
                },
                {
                    "type": "function", "name": "fetch", "description": "x",
                    "parameters": {"type": "object", "description": "x", "enum": [1]}, "extra_context": "x"
                }
            ]
        }]);
        let long_text = serde_json::json!("substantial tool context ".repeat(256));
        let with = |pointer: &str, value: &serde_json::Value| {
            let mut long = base.clone();
            *long.pointer_mut(pointer).expect("existing tool field") = value.clone();
            long
        };
        for (label, pointer, value) in [
            ("namespace description", "/0/tools/0/description", &long_text),
            ("function description", "/0/tools/1/description", &long_text),
            ("function schema", "/0/tools/1/parameters/description", &long_text),
            ("function extra", "/0/tools/1/extra_context", &long_text),
            (
                "schema numbers",
                "/0/tools/1/parameters/enum/0",
                &serde_json::json!(u64::MAX),
            ),
        ] {
            assert_text_growth([(label, base.clone(), with(pointer, value))]);
        }
        let estimate = |value: serde_json::Value| {
            estimate_input_tokens(&serde_json::from_value(value).expect("valid search output"))
        };
        for pointer in ["/0/tools/0/extra_context", "/0/tools/0/tools/0/description"] {
            assert_eq!(estimate(with(pointer, &long_text)), estimate(base.clone()), "{pointer}");
        }
    }

    #[test]
    fn token_estimate_counts_message_file_fields() {
        let base = serde_json::json!([{
            "role": "user", "content": [{"type": "input_file"}]
        }]);
        for field in ["file_data", "file_id", "file_url", "filename", "detail"] {
            let mut long = base.clone();
            long[0]["content"][0][field] = serde_json::json!("file context ".repeat(256));
            assert_text_growth([(field, base.clone(), long)]);
        }
    }

    #[test]
    fn token_estimate_counts_replayed_content() {
        let input = ResponsesInput::from(vec![
            user_message("hello context"),
            InputItem::FunctionCallOutput(FunctionToolResultMessage {
                id: None,
                status: None,
                call_id: "call_1".to_owned(),
                output: "substantial tool output".into(),
            }),
        ]);

        assert!(estimate_input_tokens(&input) > 0);
    }

    /// A hosted search as the model sees it, captured before the public pair became the stored
    /// record. Storing the public pair instead must not change when auto-compaction triggers.
    #[test]
    fn hosted_search_estimate_counts_what_the_model_sees() {
        let items: Vec<InputItem> = serde_json::from_str(include_str!(
            "../../tests/fixtures/multi_agent_private_search_history.json"
        ))
        .expect("fixture history");
        assert_eq!(estimate_history_tokens(&items), 137);

        let InputItem::FunctionCallOutput(private_output) = &items[2] else {
            panic!("the fixture's search output");
        };
        let ToolCallOutput::Text(text) = &private_output.output else {
            panic!("a text search output");
        };
        let loaded: serde_json::Value = serde_json::from_str(text).expect("search output JSON");
        let public: Vec<InputItem> = serde_json::from_value(serde_json::json!([
            items[0],
            {"type": "tool_search_call", "id": "tsc_s1", "call_id": null, "execution": "server",
                "arguments": {"paths": ["get_weather"]}, "status": "completed"},
            {"type": "tool_search_output", "id": "tso_s1", "call_id": null, "execution": "server",
                "status": "completed", "tools": loaded["tools"]},
            items[3]
        ]))
        .expect("public history");
        // The lowered call carries no item id (an upstream such as OpenAI would tie it to a call_id the public pair
        // does not keep), so the public pair counts as the stored private pair without that id.
        let mut model_view = items.clone();
        if let InputItem::FunctionCall(call) = &mut model_view[1] {
            call.id = None;
        }
        assert_eq!(
            estimate_history_tokens(&public),
            estimate_history_tokens(&model_view),
            "the public pair counts as the call and output the model reads"
        );
    }

    #[test]
    fn inline_image_payload_size_does_not_affect_user_message_estimate() {
        let small = estimate_input_tokens(&ResponsesInput::from(vec![image_message(100 * 1_024)]));
        let large = estimate_input_tokens(&ResponsesInput::from(vec![image_message(5 * 1_024 * 1_024)]));

        assert_eq!(small, large);
    }

    #[test]
    fn inline_image_payload_size_does_not_affect_structured_tool_output_estimate() {
        for make_output in [
            function_image_output as fn(usize) -> InputItem,
            custom_image_output as fn(usize) -> InputItem,
        ] {
            let small = estimate_input_tokens(&ResponsesInput::from(vec![make_output(100 * 1_024)]));
            let large = estimate_input_tokens(&ResponsesInput::from(vec![make_output(5 * 1_024 * 1_024)]));

            assert_eq!(small, large);
        }
    }

    #[test]
    fn an_image_referenced_by_file_id_is_meaningful_context() {
        let image_by = |content: InputImageContent| {
            InputItem::Message(InputMessage {
                role: "user".to_owned(),
                content: InputMessageContent::Parts(vec![InputContent::InputImage(content)]),
                ..Default::default()
            })
        };

        assert!(super::item_has_meaningful_context(&image_message(1)));
        assert!(super::item_has_meaningful_context(&image_by(InputImageContent {
            file_id: Some("file_diagram".to_owned()),
            ..InputImageContent::default()
        })));
        assert!(!super::item_has_meaningful_context(&image_by(InputImageContent {
            file_id: Some("  ".to_owned()),
            image_url: Some(String::new()),
            ..InputImageContent::default()
        })));
    }

    #[test]
    fn each_image_adds_the_fixed_image_budget() {
        let estimate_with_images = |count| {
            let parts = (0..count).map(|_| InputContent::InputImage(inline_image(1))).collect();
            estimate_input_tokens(&ResponsesInput::from(vec![InputItem::Message(InputMessage {
                role: "user".to_owned(),
                content: InputMessageContent::Parts(parts),
                ..Default::default()
            })]))
        };

        let without_image = estimate_with_images(0);
        let one_image = estimate_with_images(1);
        let two_images = estimate_with_images(2);

        assert_eq!(one_image - without_image, ESTIMATED_IMAGE_TOKENS);
        assert_eq!(two_images - one_image, ESTIMATED_IMAGE_TOKENS);
    }

    #[test]
    fn message_and_tool_textual_fields_increase_estimates() {
        let long_text = "substantial context ".repeat(256);
        assert_text_growth([
            (
                "message text",
                serde_json::json!([{"role": "user", "content": "x"}]),
                serde_json::json!([{"role": "user", "content": long_text}]),
            ),
            (
                "message content part",
                serde_json::json!([{
                    "role": "user",
                    "content": [{"type": "input_text", "text": "x"}]
                }]),
                serde_json::json!([{
                    "role": "user",
                    "content": [{"type": "input_text", "text": long_text}]
                }]),
            ),
            (
                "function arguments",
                serde_json::json!([{
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "lookup",
                    "arguments": "{}"
                }]),
                serde_json::json!([{
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "lookup",
                    "arguments": long_text
                }]),
            ),
            (
                "function call output",
                serde_json::json!([{
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "x"
                }]),
                serde_json::json!([{
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": long_text
                }]),
            ),
            (
                "custom tool input",
                serde_json::json!([{
                    "type": "custom_tool_call",
                    "call_id": "call_1",
                    "name": "shell",
                    "input": "x"
                }]),
                serde_json::json!([{
                    "type": "custom_tool_call",
                    "call_id": "call_1",
                    "name": "shell",
                    "input": long_text
                }]),
            ),
            (
                "structured custom tool output",
                serde_json::json!([{
                    "type": "custom_tool_call_output",
                    "call_id": "call_1",
                    "output": [{"type": "input_text", "text": "x"}]
                }]),
                serde_json::json!([{
                    "type": "custom_tool_call_output",
                    "call_id": "call_1",
                    "output": [{"type": "input_text", "text": long_text}]
                }]),
            ),
        ]);
    }

    #[test]
    fn shell_commands_and_outputs_increase_token_estimates() {
        let long_text = "shell context ".repeat(256);
        assert_text_growth([
            (
                "shell commands",
                serde_json::json!([{"type": "shell_call", "call_id": "c1", "action": {"commands": ["x"]}}]),
                serde_json::json!([{"type": "shell_call", "call_id": "c1", "action": {"commands": [long_text]}}]),
            ),
            (
                "shell output",
                serde_json::json!([{"type": "shell_call_output", "call_id": "c1", "output": [
                    {"stdout": "x", "stderr": "", "outcome": {"type": "exit", "exit_code": 0}}
                ]}]),
                serde_json::json!([{"type": "shell_call_output", "call_id": "c1", "output": [
                    {"stdout": long_text, "stderr": long_text, "outcome": {"type": "exit", "exit_code": 0}}
                ]}]),
            ),
        ]);
    }

    #[test]
    fn reasoning_textual_fields_increase_estimates() {
        let long_text = "substantial reasoning context ".repeat(256);
        assert_text_growth([
            (
                "assistant reasoning text",
                serde_json::json!([{
                    "type": "reasoning",
                    "id": "rs_1",
                    "content": [{"type": "reasoning_text", "text": "x"}],
                    "summary": []
                }]),
                serde_json::json!([{
                    "type": "reasoning",
                    "id": "rs_1",
                    "content": [{"type": "reasoning_text", "text": long_text}],
                    "summary": []
                }]),
            ),
            (
                "reasoning summary",
                serde_json::json!([{
                    "type": "reasoning",
                    "id": "rs_1",
                    "content": [],
                    "summary": [{"type": "summary_text", "text": "x"}]
                }]),
                serde_json::json!([{
                    "type": "reasoning",
                    "id": "rs_1",
                    "content": [],
                    "summary": [{"type": "summary_text", "text": long_text}]
                }]),
            ),
        ]);
    }

    #[test]
    fn request_and_checkpoint_estimates_agree_for_public_tool_items() {
        for wire in [
            serde_json::json!({"type":"custom_tool_call", "id":"ctc_1", "call_id":"call_1",
                "name":"notes", "input":"a quoted value: \"hello\""}),
            serde_json::json!({"type":"shell_call", "id":"sh_1", "call_id":"call_1",
                "action":{"commands":["printf hello"], "timeout_ms":1000}}),
            serde_json::json!({"type":"shell_call_output", "call_id":"call_1",
                "output":[{"stdout":"hello", "stderr":"", "outcome":{"type":"exit", "exit_code":0}}]}),
        ] {
            let checkpoint: Vec<InputItem> = vec![serde_json::from_value(wire).unwrap()];
            let request = ResponsesInput::from(checkpoint.clone());
            assert_eq!(estimate_input_tokens(&request), estimate_history_tokens(&checkpoint));
        }
    }

    #[test]
    fn text_only_estimate_remains_close_to_json_size_baseline() {
        let input: ResponsesInput = serde_json::from_value(serde_json::json!([
            {"role": "user", "content": "hello context"},
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": "{\"topic\":\"compaction\"}"
            },
            {"type": "function_call_output", "call_id": "call_1", "output": "tool context"}
        ]))
        .expect("valid text-only input");
        let serialized = serde_json::to_string(&input.model_input()).expect("model input serializes");
        let serialized_bytes = u64::try_from(serialized.len()).expect("serialized fixture length fits in u64");
        let json_size_estimate = serialized_bytes.div_ceil(4);
        let typed_estimate = estimate_input_tokens(&input);

        assert!(
            typed_estimate.abs_diff(json_size_estimate) <= json_size_estimate.div_ceil(4),
            "typed estimate {typed_estimate} should remain within 25% of JSON estimate {json_size_estimate}"
        );
    }

    #[test]
    fn estimate_uses_only_the_effective_compacted_window() {
        let input_with_stale_text = |stale_text: String| {
            ResponsesInput::from(vec![
                user_message(&stale_text),
                InputItem::Compaction(CompactionItem {
                    agent: None,
                    id: Some("cmp_1".to_owned()),
                    encrypted_content: "durable summary".to_owned(),
                }),
                user_message("continue"),
            ])
        };

        let short = input_with_stale_text("old".to_owned());
        let large = input_with_stale_text("old".repeat(100_000));

        assert_eq!(estimate_input_tokens(&short), estimate_input_tokens(&large));
    }

    #[tokio::test]
    async fn automatic_compaction_ignores_image_bytes_but_triggers_for_text_and_preserves_image() {
        let large_image_input = ResponsesInput::from(vec![image_message(5 * 1_024 * 1_024)]);
        let image_estimate = estimate_input_tokens(&large_image_input);
        let threshold = image_estimate.saturating_add(100);
        let (exec_ctx, requests, server) = mock_execution_context(ResponseStore::disabled()).await;
        let mut image_context = context_with_threshold(large_image_input, threshold);

        assert!(
            maybe_compact_context(&mut image_context, &exec_ctx, None)
                .await
                .expect("image-only threshold check succeeds")
                .is_none()
        );

        let retained_image = inline_image(100 * 1_024);
        let expected_url = retained_image.image_url.clone().expect("inline image URL");
        let text_input = ResponsesInput::from(vec![
            InputItem::Message(InputMessage {
                role: "user".to_owned(),
                content: InputMessageContent::Parts(vec![InputContent::InputImage(retained_image)]),
                ..Default::default()
            }),
            user_message(&"genuine textual context ".repeat(1_000)),
        ]);
        assert!(estimate_input_tokens(&text_input) > threshold);
        let mut text_context = context_with_threshold(text_input, threshold);
        text_context.enriched_request.prompt_cache_key = Some("workspace-a".to_owned());
        text_context.enriched_request.service_tier = Some("priority".to_owned());
        text_context.enriched_request.prompt_cache_retention =
            Some(crate::types::request_response::PromptCacheRetention::TwentyFourHours);

        assert!(
            maybe_compact_context(&mut text_context, &exec_ctx, None)
                .await
                .expect("long text compacts")
                .is_some()
        );
        let ResponsesInput::Items(compacted) = &text_context.enriched_request.input else {
            panic!("compaction should produce item input");
        };
        let retained_url = compacted.iter().find_map(|item| {
            let InOutItem::Input(InputItem::Message(message)) = item else {
                return None;
            };
            let InputMessageContent::Parts(parts) = &message.content else {
                return None;
            };
            parts.iter().find_map(|part| match part {
                InputContent::InputImage(image) => image.image_url.as_deref(),
                _ => None,
            })
        });
        assert_eq!(retained_url, Some(expected_url.as_str()));
        let requests = requests.lock().expect("request capture lock");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["prompt_cache_key"], "workspace-a");
        assert_eq!(requests[0]["service_tier"], "priority");
        assert_eq!(requests[0]["prompt_cache_retention"], "24h");
        server.abort();
    }

    #[tokio::test]
    async fn multi_agent_compaction_snapshot_preserves_prompt_cache_retention() {
        use crate::executor::multi_agent::CompactionPlan;
        use crate::types::agent::AgentIdentity;
        use crate::types::request_response::PromptCacheRetention;

        let (exec, requests, server) = mock_execution_context(ResponseStore::disabled()).await;
        let mut request = request_payload("test-model".into(), ResponsesInput::default(), None);
        request.prompt_cache_retention = Some(PromptCacheRetention::TwentyFourHours);
        let plan =
            CompactionPlan::prepare_explicit(&AgentIdentity::root(), 0, &[user_message("remember context")], &request)
                .unwrap()
                .unwrap();
        request.prompt_cache_retention = Some(PromptCacheRetention::InMemory);
        plan.execute(&exec, None).await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["prompt_cache_retention"], "24h");
        server.abort();
    }

    #[tokio::test]
    async fn prompt_cache_options_survive_automatic_and_snapshot_compaction() {
        use crate::executor::multi_agent::CompactionPlan;
        use crate::types::agent::AgentIdentity;
        let (mut exec, requests, server) = mock_execution_context(ResponseStore::disabled()).await;
        exec.responses_config.prompt_cache_options_supported = true;
        let options = serde_json::from_value(serde_json::json!({"mode":"explicit","ttl":"30m"})).unwrap();
        let mut ctx = context_with_threshold(ResponsesInput::from(vec![user_message(&"context ".repeat(100))]), 1);
        ctx.enriched_request.prompt_cache_options = Some(options);
        assert!(maybe_compact_context(&mut ctx, &exec, None).await.unwrap().is_some());
        assert_eq!(ctx.enriched_request.prompt_cache_options, Some(options));
        let plan = CompactionPlan::prepare_explicit(
            &AgentIdentity::root(),
            0,
            &[user_message("remember context")],
            &ctx.enriched_request,
        )
        .unwrap()
        .unwrap();
        ctx.enriched_request.prompt_cache_options =
            Some(serde_json::from_value(serde_json::json!({"mode":"implicit"})).unwrap());
        plan.execute(&exec, None).await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(
                request["prompt_cache_options"],
                serde_json::json!({"mode":"explicit","ttl":"30m"})
            );
        }
        server.abort();
    }

    #[test]
    fn partial_text_from_incomplete_or_failed_summary_is_rejected() {
        for (status, error, incomplete_details) in [
            (
                "incomplete",
                serde_json::Value::Null,
                serde_json::json!({"reason": "max_output_tokens"}),
            ),
            (
                "failed",
                serde_json::json!({"message": "model failed"}),
                serde_json::Value::Null,
            ),
        ] {
            let response: crate::ResponsePayload = serde_json::from_value(serde_json::json!({
                "id": "resp_upstream",
                "object": "response",
                "created_at": 0,
                "model": "test-model",
                "status": status,
                "output": [{
                    "id": "msg_partial",
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "partial summary", "annotations": []}]
                }],
                "usage": null,
                "incomplete_details": incomplete_details,
                "error": error,
                "previous_response_id": null,
                "conversation_id": null,
                "instructions": null
            }))
            .expect("valid upstream response");

            let error = completed_summary_text(&response).expect_err("partial summary must be rejected");
            assert!(matches!(error, crate::executor::ExecutorError::CompactionFailed { .. }));
        }
    }

    #[test]
    fn completed_response_without_summary_is_an_upstream_failure() {
        let response: crate::ResponsePayload = serde_json::from_value(serde_json::json!({
            "id": "resp_upstream",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [],
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        }))
        .expect("valid upstream response");

        let error = completed_summary_text(&response).expect_err("missing summary must be rejected");

        assert!(matches!(
            error,
            crate::executor::ExecutorError::CompactionFailed {
                ref status,
                ref details
            } if status == "completed" && details.contains("no summary text")
        ));
        assert_eq!(error.http_status(), http::StatusCode::BAD_GATEWAY);
        assert_eq!(error.error_code(), "upstream_error");
    }

    #[tokio::test]
    async fn disabled_storage_does_not_fail_compaction() {
        let (exec_ctx, _requests, server) = mock_execution_context(ResponseStore::disabled()).await;

        let response = compact_response(compact_request(), &exec_ctx, None)
            .await
            .expect("disabled persistence should be ignored");

        assert_eq!(response.object, "response.compaction");
        assert_eq!(response.usage.total_tokens, 15);
        assert!(response.id.starts_with("resp_"));
        assert!(
            matches!(response.output.last(), Some(InputItem::Compaction(item)) if item.encrypted_content == "durable summary")
        );
        server.abort();
    }

    #[tokio::test]
    async fn empty_context_is_rejected_before_summarization() {
        let (exec_ctx, _requests, server) = mock_execution_context(ResponseStore::disabled()).await;
        for input in [
            serde_json::json!(""),
            serde_json::json!([{"role": "user", "content": "   "}]),
            serde_json::json!([]),
        ] {
            let request = serde_json::from_value(serde_json::json!({
                "model": "test-model",
                "input": input
            }))
            .expect("structurally valid compact request");
            let error = compact_response(request, &exec_ctx, None)
                .await
                .expect_err("empty context must fail");
            assert!(matches!(error, crate::executor::ExecutorError::InvalidRequest(_)));
        }
        server.abort();
    }

    #[tokio::test]
    async fn compaction_prepares_tool_search_before_summarization() {
        let (exec_ctx, _requests, server) = mock_execution_context(ResponseStore::disabled()).await;
        let request = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "input": [{
                "type": "tool_search_call",
                "id": "tsc_1",
                "call_id": "call_search_1",
                "arguments": {"query": "weather"}
            }, {
                "type": "tool_search_output",
                "call_id": "call_search_1",
                "tools": []
            }]
        }))
        .expect("valid compact request");

        let compacted = compact_response(request, &exec_ctx, None)
            .await
            .expect("compaction prepares and summarizes public search history");

        assert!(matches!(compacted.output.last(), Some(InputItem::Compaction(_))));
        assert!(
            compacted
                .output
                .iter()
                .all(|item| !matches!(item, InputItem::ToolSearchCall(_) | InputItem::ToolSearchOutput(_)))
        );
        server.abort();
    }

    #[tokio::test]
    async fn compaction_persists_a_reusable_response_checkpoint() {
        let pool = create_pool_with_schema(Some("sqlite::memory:"))
            .await
            .expect("create response store");
        let response_store = ResponseStore::new(pool);
        let (exec_ctx, _requests, server) = mock_execution_context(response_store.clone()).await;

        let response = compact_response(compact_request(), &exec_ctx, None)
            .await
            .expect("compaction succeeds");
        let history = response_store
            .rehydrate(&response.id)
            .await
            .expect("compaction checkpoint can be rehydrated");

        assert_eq!(history.len(), 2);
        assert!(matches!(history[0], InOutItem::Input(InputItem::Message(_))));
        assert!(matches!(history[1], InOutItem::Input(InputItem::Compaction(_))));
        server.abort();
    }

    #[tokio::test]
    async fn compaction_resolves_and_replaces_previous_response_history() {
        let pool = create_pool_with_schema(Some("sqlite::memory:"))
            .await
            .expect("create response store");
        let response_store = ResponseStore::new(pool);
        response_store
            .persist(
                "resp_previous",
                None,
                vec![InOutItem::Input(user_message("remember banana"))],
                &ResponseMetadata {
                    multi_agent_tree: None,
                    model: "test-model".to_owned(),
                    previous_response_id: None,
                    effective_tools: None,
                    tool_search_loaded_tools: None,
                    response_snapshot: None,
                    effective_tool_choice: crate::ToolChoice::Auto,
                    effective_instructions: None,
                },
            )
            .await
            .expect("seed previous response");
        let (exec_ctx, _requests, server) = mock_execution_context(response_store.clone()).await;
        let request = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "previous_response_id": "resp_previous"
        }))
        .expect("valid previous-response compact request");

        let response = compact_response(request, &exec_ctx, None)
            .await
            .expect("previous response compacts");
        let history = response_store
            .rehydrate(&response.id)
            .await
            .expect("compacted continuation rehydrates");
        let model_input = ResponsesInput::from(InOutItem::into_input_items(history));
        let serialized = serde_json::to_value(model_input.model_input()).expect("model input serializes");

        assert_eq!(serialized.as_array().map(Vec::len), Some(2));
        assert_eq!(serialized[0]["content"][0]["text"], "remember banana");
        assert_eq!(serialized[1]["role"], "assistant");
        assert_eq!(serialized[1]["content"][0]["text"], "durable summary");
        server.abort();
    }
}
