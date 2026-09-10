//! Ordered client delivery; no response assembly or tool-call translation lives here.
use crate::events::{EventFrame, SSEEventType, WireEvent};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway::{
    emit_gateway_completed_events, emit_gateway_start_events, mcp_list_tools_event_plans, public_output_items,
};
use crate::executor::gateway_accumulator::{GatewayStreamAccumulator, StreamEvent, emit_sse_frame};
use crate::executor::request::RequestContext;
use crate::executor::translate::Translation;
use crate::tool::ToolRegistry;
use crate::utils::common::serialize_to_string;
use serde_json::Value;
use tokio::sync::mpsc::Sender;

const MAX_DEFERRED_STREAM_BYTES: usize = 256 * 1024;
struct StreamEmitContext<'a> {
    request: &'a RequestContext,
    sender: &'a Sender<StreamEvent>,
    accumulator: &'a mut GatewayStreamAccumulator,
    output_offset: usize,
}

pub(super) struct StreamDelivery {
    pub(super) accumulator: GatewayStreamAccumulator,
    pub(super) sender: Option<Sender<StreamEvent>>,
    defer_from_output_index: Option<u64>,
    deferred_events: Vec<EventFrame>,
    deferred_bytes: usize,
}
impl StreamDelivery {
    pub(super) fn new(sender: Option<Sender<StreamEvent>>) -> Self {
        Self {
            accumulator: GatewayStreamAccumulator::new(),
            sender,
            defer_from_output_index: None,
            deferred_events: Vec::new(),
            deferred_bytes: 0,
        }
    }
    pub(super) fn take_deferred_events(&mut self) -> Vec<EventFrame> {
        self.defer_from_output_index = None;
        self.deferred_bytes = 0;
        std::mem::take(&mut self.deferred_events)
    }
    pub(super) async fn accept(
        &mut self,
        translation: Translation,
        ctx: &RequestContext,
        registry: &ToolRegistry,
        output_offset: usize,
    ) -> ExecutorResult<()> {
        let previous_defer_from_output_index = self.defer_from_output_index;
        self.defer_from_output_index = translation.defer_from_output_index.map(u64::from);
        for frame in &translation.frames {
            log_upstream_failure(frame, &ctx.response_id);
        }
        if let Some(sender) = self.sender.as_ref() {
            let mut emit_ctx = StreamEmitContext {
                request: ctx,
                sender,
                accumulator: &mut self.accumulator,
                output_offset,
            };
            for frame in translation.frames {
                if !is_terminal_response_event(frame.event_type) {
                    let event_type = frame.event_type;
                    let emitted = emit_or_defer_stream_frame(
                        frame,
                        &mut emit_ctx,
                        self.defer_from_output_index,
                        &mut self.deferred_events,
                        &mut self.deferred_bytes,
                    )
                    .await?;
                    if event_type == SSEEventType::ResponseInProgress && emitted {
                        emit_mcp_discovery_lifecycle(registry, emit_ctx.accumulator, emit_ctx.sender).await?;
                    }
                }
            }
            if self.defer_from_output_index != previous_defer_from_output_index {
                flush_released_stream_frames(
                    &mut emit_ctx,
                    self.defer_from_output_index,
                    &mut self.deferred_events,
                    &mut self.deferred_bytes,
                )
                .await?;
            }
        }
        Ok(())
    }
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

pub(in crate::executor) async fn emit_deferred_stream_events(
    deferred_events: Vec<EventFrame>,
    request: &RequestContext,
    accumulator: &mut GatewayStreamAccumulator,
    sender: &tokio::sync::mpsc::Sender<StreamEvent>,
    output_offset: usize,
) -> ExecutorResult<()> {
    let mut emit_ctx = StreamEmitContext {
        request,
        sender,
        accumulator,
        output_offset,
    };
    for mut frame in deferred_events {
        emit_stream_frame(&mut frame, &mut emit_ctx).await?;
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

async fn emit_stream_frame(frame: &mut EventFrame, emit_ctx: &mut StreamEmitContext<'_>) -> ExecutorResult<bool> {
    apply_context_response_ids(&mut frame.wire, emit_ctx.request);
    let emitted = emit_ctx.accumulator.process_event(frame, emit_ctx.output_offset);
    if emitted {
        emit_sse_frame(emit_ctx.sender, frame).await?;
    }
    Ok(emitted)
}

async fn emit_or_defer_stream_frame(
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
    emit_stream_frame(&mut frame, emit_ctx).await
}

async fn flush_released_stream_frames(
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
        )
        .await?;
    }
    Ok(())
}

async fn emit_mcp_discovery_lifecycle(
    registry: &ToolRegistry,
    stream_accumulator: &mut GatewayStreamAccumulator,
    stream_sender: &tokio::sync::mpsc::Sender<StreamEvent>,
) -> ExecutorResult<()> {
    let discovered_output = registry
        .mcp_list_tool_items()
        .map(crate::tool::mcp::handler::list_tools_output_item)
        .collect::<Vec<_>>();
    let public_output = public_output_items(&discovered_output, registry, &[])?;
    let event_plans = mcp_list_tools_event_plans(&public_output, 0);

    emit_gateway_start_events(&event_plans, stream_accumulator, stream_sender).await?;
    emit_gateway_completed_events(&public_output, &event_plans, stream_accumulator, stream_sender).await
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
    use crate::executor::upstream::tests::request_context;
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

    #[tokio::test]
    async fn released_frames_are_emitted_in_output_index_order() {
        let request = request_context();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let mut accumulator = GatewayStreamAccumulator::new();
        let mut emit_ctx = StreamEmitContext {
            request: &request,
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

        flush_released_stream_frames(&mut emit_ctx, None, &mut deferred, &mut deferred_bytes)
            .await
            .expect("flush succeeds");
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

    #[tokio::test]
    async fn deferred_frames_have_a_shared_byte_limit() {
        let request = request_context();
        let (sender, _receiver) = tokio::sync::mpsc::channel(4);
        let mut accumulator = GatewayStreamAccumulator::new();
        let mut emit_ctx = StreamEmitContext {
            request: &request,
            sender: &sender,
            accumulator: &mut accumulator,
            output_offset: 0,
        };
        let mut deferred = Vec::new();
        let mut deferred_bytes = 0;
        let oversized = frame(0, Value::String("x".repeat(256 * 1024 + 1)));

        let error = emit_or_defer_stream_frame(oversized, &mut emit_ctx, Some(0), &mut deferred, &mut deferred_bytes)
            .await
            .expect_err("oversized deferred stream must fail");
        assert!(error.to_string().contains("deferred stream exceeded"));
    }
}
