use std::sync::Arc;

use futures::StreamExt;
use serde_json::Value;

use crate::events::{EventFrame, SSEEventType, WireEvent};
use crate::executor::accumulator::ResponseAccumulator;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::function_sse::FunctionSseTranslator;
use crate::executor::gateway::{
    emit_gateway_completed_events, emit_gateway_start_events, mcp_list_tools_event_plans, public_output_items,
};
use crate::executor::gateway_accumulator::{GatewayStreamAccumulator, StreamEvent, emit_sse_frame};
use crate::executor::inference::{call_inference, fetch_response_json};
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::tool::ToolRegistry;
use crate::types::request_response::ResponsePayload;
use crate::utils::common::serialize_to_string;

const MAX_DEFERRED_STREAM_BYTES: usize = 256 * 1024;

struct StreamEmitContext<'a> {
    request: &'a RequestContext,
    registry: &'a ToolRegistry,
    sender: &'a tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    accumulator: &'a mut GatewayStreamAccumulator,
    output_offset: usize,
}

pub(super) struct StreamPayload {
    pub(super) payload: ResponsePayload,
    pub(super) deferred_events: Vec<EventFrame>,
}

pub(super) async fn fetch_blocking_payload(
    ctx: &RequestContext,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
    registry: &ToolRegistry,
) -> ExecutorResult<ResponsePayload> {
    let url = exec_ctx.responses_url();
    // Non-streaming request: stream=false -> full JSON body -> from_json.
    registry.ensure_request_prepared(&ctx.enriched_request)?;
    let upstream_request = ctx.enriched_request.to_upstream_request(false)?;
    let upstream_json = serialize_to_string(&upstream_request).map_err(ExecutorError::JsonError)?;

    let body = fetch_response_json(upstream_json, &url, &exec_ctx.client, auth).await?;
    registry.validate_blocking_response(&body)?;
    let acc = ResponseAccumulator::from_json(&body, ctx.conversation_id.as_deref())?;
    let mut payload = acc.finalize(
        &ctx.enriched_request.model,
        ctx.original_request.previous_response_id.as_deref(),
        ctx.original_request.instructions.as_deref(),
    );
    let status = payload.status.parse().unwrap_or_default();
    registry.normalize_response_output(&mut payload.output, status, &std::collections::HashSet::new())?;
    ctx.inject_ids(&mut payload);

    Ok(payload)
}

pub(super) async fn fetch_stream_payload(
    ctx: &RequestContext,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
    registry: &mut ToolRegistry,
    mut stream: Option<(
        &mut GatewayStreamAccumulator,
        &tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    )>,
    output_offset: usize,
) -> ExecutorResult<StreamPayload> {
    let url = exec_ctx.responses_url();
    registry.ensure_request_prepared(&ctx.enriched_request)?;
    let upstream_request = ctx.enriched_request.to_upstream_request(true)?;
    let upstream_json = serialize_to_string(&upstream_request).map_err(ExecutorError::JsonError)?;
    let mut line_stream = Box::pin(call_inference(
        upstream_json,
        url,
        Arc::clone(&exec_ctx.client),
        auth.map(str::to_owned),
        exec_ctx.streaming_timeout,
    ));
    let mut acc = ResponseAccumulator::new(ctx.response_id.clone(), ctx.conversation_id.clone());
    let mut function_sse = FunctionSseTranslator::new(registry);
    let mut defer_from_output_index = None;
    let mut deferred_events = Vec::new();
    let mut deferred_bytes = 0;
    while let Some(line_result) = line_stream.next().await {
        let line = line_result?;
        if let Some(translation) = acc.process_sse_line_with_translator(&line, &mut function_sse)? {
            let previous_defer_from_output_index = defer_from_output_index;
            defer_from_output_index = translation.defer_from_output_index.map(u64::from);
            for frame in &translation.frames {
                log_upstream_failure(frame, &ctx.response_id);
            }
            if let Some((accumulator, sender)) = stream.as_mut() {
                let mut emit_ctx = StreamEmitContext {
                    request: ctx,
                    registry,
                    sender,
                    accumulator,
                    output_offset,
                };
                for frame in translation.frames {
                    if !is_terminal_response_event(frame.event_type) {
                        let event_type = frame.event_type;
                        let emitted = emit_or_defer_stream_frame(
                            frame,
                            &mut emit_ctx,
                            defer_from_output_index,
                            &mut deferred_events,
                            &mut deferred_bytes,
                        )?;
                        if event_type == SSEEventType::ResponseInProgress && emitted {
                            emit_mcp_discovery_lifecycle(registry, emit_ctx.accumulator, emit_ctx.sender)?;
                        }
                    }
                }
                if defer_from_output_index != previous_defer_from_output_index {
                    flush_released_stream_frames(
                        &mut emit_ctx,
                        defer_from_output_index,
                        &mut deferred_events,
                        &mut deferred_bytes,
                    )?;
                }
            }
        }
    }
    let function_sse_outcome = function_sse.finish()?;
    acc.finish_stream();
    let mut payload = acc.finalize(
        &ctx.enriched_request.model,
        ctx.original_request.previous_response_id.as_deref(),
        ctx.original_request.instructions.as_deref(),
    );
    let status = payload.status.parse().unwrap_or_default();
    registry.normalize_response_output(
        &mut payload.output,
        status,
        &function_sse_outcome.unfinished_tool_search_item_ids,
    )?;
    ctx.inject_ids(&mut payload);
    Ok(StreamPayload {
        payload,
        deferred_events,
    })
}

fn log_upstream_failure(frame: &EventFrame, gateway_response_id: &str) {
    if frame.event_type != SSEEventType::ResponseFailed {
        return;
    }

    let response = frame.wire.rest.get("response").unwrap_or(&Value::Null);
    let error = &response["error"];
    let error_code = error.get("code").and_then(Value::as_str).unwrap_or_default();
    let error_message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or_default();
    let incomplete_reason = response["incomplete_details"]
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();

    tracing::warn!(
        response_id = %gateway_response_id,
        upstream_response_id = response["id"].as_str().unwrap_or_default(),
        error_code,
        error_message,
        incomplete_reason,
        "upstream response failed"
    );
}

pub(super) fn emit_deferred_stream_events(
    deferred_events: Vec<EventFrame>,
    request: &RequestContext,
    registry: &ToolRegistry,
    accumulator: &mut GatewayStreamAccumulator,
    sender: &tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    output_offset: usize,
) -> ExecutorResult<()> {
    let mut emit_ctx = StreamEmitContext {
        request,
        registry,
        sender,
        accumulator,
        output_offset,
    };
    for mut frame in deferred_events {
        emit_stream_frame(&mut frame, &mut emit_ctx)?;
    }
    Ok(())
}

fn should_defer_stream_event(frame: &EventFrame, defer_from_output_index: Option<u64>) -> bool {
    defer_from_output_index.is_some_and(|first_hidden_index| {
        frame
            .wire
            .output_index
            .is_some_and(|output_index| output_index >= first_hidden_index)
    })
}

fn emit_stream_frame(frame: &mut EventFrame, emit_ctx: &mut StreamEmitContext<'_>) -> ExecutorResult<bool> {
    apply_context_response_ids(&mut frame.wire, emit_ctx.request);
    emit_ctx.registry.restore_tool_search_response_tools(&mut frame.wire)?;
    emit_ctx.registry.restore_stream_event_wire(&mut frame.wire);
    let emitted = emit_ctx.accumulator.process_event(frame, emit_ctx.output_offset);
    if emitted {
        emit_sse_frame(emit_ctx.sender, frame)?;
    }
    Ok(emitted)
}

fn emit_or_defer_stream_frame(
    mut frame: EventFrame,
    emit_ctx: &mut StreamEmitContext<'_>,
    defer_from_output_index: Option<u64>,
    deferred_events: &mut Vec<EventFrame>,
    deferred_bytes: &mut usize,
) -> ExecutorResult<bool> {
    if should_defer_stream_event(&frame, defer_from_output_index) {
        let frame_bytes = serialize_to_string(&frame.wire)
            .map_err(ExecutorError::JsonError)?
            .len();
        let next_bytes = deferred_bytes.saturating_add(frame_bytes);
        if next_bytes > MAX_DEFERRED_STREAM_BYTES {
            return Err(ExecutorError::StreamError(format!(
                "deferred stream exceeded {MAX_DEFERRED_STREAM_BYTES} buffered bytes"
            )));
        }
        deferred_events.push(frame);
        *deferred_bytes = next_bytes;
        return Ok(false);
    }
    emit_stream_frame(&mut frame, emit_ctx)
}

fn flush_released_stream_frames(
    emit_ctx: &mut StreamEmitContext<'_>,
    defer_from_output_index: Option<u64>,
    deferred_events: &mut Vec<EventFrame>,
    deferred_bytes: &mut usize,
) -> ExecutorResult<()> {
    let mut pending = std::mem::take(deferred_events);
    *deferred_bytes = 0;
    pending.sort_by_key(|frame| frame.wire.output_index);
    for frame in pending {
        emit_or_defer_stream_frame(
            frame,
            emit_ctx,
            defer_from_output_index,
            deferred_events,
            deferred_bytes,
        )?;
    }
    Ok(())
}

fn emit_mcp_discovery_lifecycle(
    registry: &ToolRegistry,
    stream_accumulator: &mut GatewayStreamAccumulator,
    stream_sender: &tokio::sync::mpsc::UnboundedSender<StreamEvent>,
) -> ExecutorResult<()> {
    let discovered_output = registry
        .mcp_list_tools_items()
        .iter()
        .map(crate::tool::mcp::handler::list_tools_output_item)
        .collect::<Vec<_>>();
    let public_output = public_output_items(&discovered_output, registry, &[]);
    let event_plans = mcp_list_tools_event_plans(&public_output, 0);

    emit_gateway_start_events(&event_plans, stream_accumulator, stream_sender)?;
    emit_gateway_completed_events(&public_output, &event_plans, stream_accumulator, stream_sender)
}

fn is_terminal_response_event(event_type: SSEEventType) -> bool {
    matches!(
        event_type,
        SSEEventType::ResponseCompleted | SSEEventType::ResponseFailed | SSEEventType::ResponseIncomplete
    )
}

fn apply_context_response_ids(wire: &mut WireEvent, ctx: &RequestContext) {
    let Some(response) = wire.rest.get_mut("response").and_then(Value::as_object_mut) else {
        return;
    };
    response.insert("id".to_owned(), Value::String(ctx.response_id.clone()));
    if let Some(previous_response_id) = &ctx.original_request.previous_response_id {
        response.insert(
            "previous_response_id".to_owned(),
            Value::String(previous_response_id.clone()),
        );
    }
    if let Some(conversation_id) = &ctx.conversation_id {
        response.insert("conversation_id".to_owned(), Value::String(conversation_id.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventPayload;
    use crate::types::io::ResponsesInput;
    use crate::types::request_response::RequestPayload;

    fn request_context() -> RequestContext {
        let request = RequestPayload {
            model: "test".to_owned(),
            input: ResponsesInput::Text("hi".to_owned()),
            instructions: None,
            previous_response_id: None,
            conversation_id: None,
            tools: None,
            tool_choice: None,
            stream: true,
            store: false,
            include: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            metadata: None,
            parallel_tool_calls: None,
            cache_salt: None,
            context_management: None,
        };
        RequestContext {
            original_request: request.clone(),
            enriched_request: request,
            new_input_items: Vec::new(),
            response_id: "resp_test".to_owned(),
            conversation_id: None,
            conversation_version: None,
        }
    }

    fn frame(output_index: u64, payload: Value) -> EventFrame {
        let mut wire = WireEvent::new("response.output_item.added");
        wire.output_index = Some(output_index);
        wire.rest.insert("item".to_owned(), payload);
        EventFrame {
            event_type: SSEEventType::OutputItemAdded,
            payload: EventPayload::None,
            wire,
        }
    }

    #[test]
    fn released_frames_are_emitted_in_output_index_order() {
        let request = request_context();
        let registry = ToolRegistry::default();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut accumulator = GatewayStreamAccumulator::new();
        let mut emit_ctx = StreamEmitContext {
            request: &request,
            registry: &registry,
            sender: &sender,
            accumulator: &mut accumulator,
            output_offset: 0,
        };
        let mut deferred = vec![
            frame(3, serde_json::json!({"id": "msg_3"})),
            frame(2, serde_json::json!({"id": "msg_2"})),
        ];
        let mut deferred_bytes = deferred
            .iter()
            .map(|frame| serialize_to_string(&frame.wire).unwrap().len())
            .sum();

        flush_released_stream_frames(&mut emit_ctx, None, &mut deferred, &mut deferred_bytes).expect("flush succeeds");
        assert_eq!(deferred_bytes, 0);

        let indices = [receiver.try_recv().unwrap(), receiver.try_recv().unwrap()].map(|event| {
            let data_line = event
                .content
                .lines()
                .find(|line| line.starts_with("data: "))
                .expect("SSE data line");
            crate::events::normalize_sse_line(data_line)
                .and_then(|frame| frame.wire.output_index)
                .expect("output index")
        });
        assert_eq!(indices, [2, 3]);
    }

    #[test]
    fn deferred_frames_have_a_shared_byte_limit() {
        let request = request_context();
        let registry = ToolRegistry::default();
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut accumulator = GatewayStreamAccumulator::new();
        let mut emit_ctx = StreamEmitContext {
            request: &request,
            registry: &registry,
            sender: &sender,
            accumulator: &mut accumulator,
            output_offset: 0,
        };
        let mut deferred = Vec::new();
        let mut deferred_bytes = 0;
        let oversized = frame(0, Value::String("x".repeat(256 * 1024 + 1)));

        let error = emit_or_defer_stream_frame(oversized, &mut emit_ctx, Some(0), &mut deferred, &mut deferred_bytes)
            .expect_err("oversized deferred stream must fail");
        assert!(error.to_string().contains("deferred stream exceeded"));
    }
}
