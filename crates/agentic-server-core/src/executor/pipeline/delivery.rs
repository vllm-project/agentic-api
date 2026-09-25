//! Ordered client delivery; no response assembly or tool-call translation lives here.
use crate::events::{EventFrame, SSEEventType, WireEvent};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway::{
    emit_gateway_completed_events, emit_gateway_start_events, mcp_list_tools_event_plans, public_output_items,
};
use crate::executor::gateway_accumulator::{GatewayStreamAccumulator, StreamEvent, emit_sse_frame_limited};
use crate::executor::request::RequestContext;
use crate::executor::translate::Translation;
use crate::tool::ToolRegistry;
use crate::utils::common::serialize_to_string;
use serde_json::Value;
use tokio::sync::mpsc::Sender;

const MAX_DEFERRED_STREAM_BYTES: usize = 256 * 1024;
const MAX_DEFERRED_STREAM_EVENTS: usize = 1024;
struct StreamEmitContext<'a> {
    request: &'a RequestContext,
    sender: &'a Sender<StreamEvent>,
    accumulator: &'a mut GatewayStreamAccumulator,
    output_offset: usize,
}

/// How much of a provider's `response.failed` details may enter gateway logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::executor) enum UpstreamFailureLog {
    /// Default adapters log the provider's error code and message.
    Detailed,
    /// Provider text can reflect request data, including opaque state: log only
    /// an identifier-shaped error code and the gateway response ID.
    CodeOnly,
}

pub(super) struct StreamDelivery {
    pub(super) accumulator: GatewayStreamAccumulator,
    pub(super) sender: Option<Sender<StreamEvent>>,
    pub(super) failure_log: UpstreamFailureLog,
    defer_from_output_index: Option<u64>,
    deferred_events: Vec<EventFrame>,
    deferred_bytes: usize,
}
impl StreamDelivery {
    pub(super) fn new(sender: Option<Sender<StreamEvent>>) -> Self {
        Self {
            accumulator: GatewayStreamAccumulator::new(),
            sender,
            failure_log: UpstreamFailureLog::Detailed,
            defer_from_output_index: None,
            deferred_events: Vec::new(),
            deferred_bytes: 0,
        }
    }
    pub(super) fn with_max_stream_event_bytes(
        sender: Option<Sender<StreamEvent>>,
        max_stream_event_bytes: usize,
    ) -> Self {
        Self {
            accumulator: GatewayStreamAccumulator::with_max_stream_event_bytes(max_stream_event_bytes),
            sender,
            failure_log: UpstreamFailureLog::Detailed,
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
            log_upstream_failure(frame, &ctx.response_id, self.failure_log);
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

fn log_upstream_failure(frame: &EventFrame, gateway_response_id: &str, detail: UpstreamFailureLog) {
    if frame.event_type != SSEEventType::ResponseFailed {
        return;
    }

    let response = frame.wire.rest.get("response").unwrap_or(&Value::Null);
    let error = &response["error"];
    let error_code = error.get("code").and_then(Value::as_str).unwrap_or_default();
    if detail == UpstreamFailureLog::CodeOnly {
        tracing::warn!(
            response_id = %gateway_response_id,
            error_code = identifier_or_withheld(error_code),
            "upstream response failed; provider error details withheld"
        );
        return;
    }
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

/// Provider error codes are short identifiers; any other text could be reflected input.
fn identifier_or_withheld(code: &str) -> &str {
    let identifier = code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'.' | b'-'));
    if identifier { code } else { "<withheld>" }
}

pub(in crate::executor) async fn emit_deferred_stream_events(
    mut deferred_events: Vec<EventFrame>,
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
    deferred_events.sort_by_key(deferred_event_order);
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
            .is_none_or(|output_index| output_index >= first_hidden_index)
    })
}

fn deferred_event_order(frame: &EventFrame) -> (bool, u64) {
    frame
        .wire
        .output_index
        .map_or((true, 0), |output_index| (false, output_index))
}

async fn emit_stream_frame(frame: &mut EventFrame, emit_ctx: &mut StreamEmitContext<'_>) -> ExecutorResult<bool> {
    apply_context_response_ids(&mut frame.wire, emit_ctx.request);
    emit_client_frame(frame, emit_ctx.accumulator, emit_ctx.sender, emit_ctx.output_offset).await
}

pub(in crate::executor) async fn emit_gateway_event(
    frame: &mut EventFrame,
    accumulator: &mut GatewayStreamAccumulator,
    sender: &Sender<StreamEvent>,
) -> ExecutorResult<()> {
    // Gateway-synthesized events already use public IDs and absolute indexes.
    emit_client_frame(frame, accumulator, sender, 0).await?;
    Ok(())
}

async fn emit_client_frame(
    frame: &mut EventFrame,
    accumulator: &mut GatewayStreamAccumulator,
    sender: &Sender<StreamEvent>,
    output_offset: usize,
) -> ExecutorResult<bool> {
    // Only three scalar fields are cloned. Commit presentation state after
    // enqueueing, not on a cancelled wait, closed receiver, or oversized event.
    let mut published = accumulator.clone();
    if !published.process_event(frame, output_offset) {
        return Ok(false);
    }
    emit_sse_frame_limited(sender, frame, accumulator.max_stream_event_bytes()).await?;
    *accumulator = published;
    Ok(true)
}

async fn emit_or_defer_stream_frame(
    mut frame: EventFrame,
    emit_ctx: &mut StreamEmitContext<'_>,
    defer_from_output_index: Option<u64>,
    deferred_events: &mut Vec<EventFrame>,
    deferred_bytes: &mut usize,
) -> ExecutorResult<bool> {
    if should_defer_stream_event(&frame, defer_from_output_index) {
        if deferred_events.len() >= MAX_DEFERRED_STREAM_EVENTS {
            return Err(ExecutorError::StreamError(format!(
                "deferred stream exceeded {MAX_DEFERRED_STREAM_EVENTS} buffered events"
            )));
        }
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
    deferred_events.sort_by_key(deferred_event_order);
    while let Some(index) = deferred_events
        .iter()
        .position(|frame| !should_defer_stream_event(frame, defer_from_output_index))
    {
        let mut frame = deferred_events[index].clone();
        emit_stream_frame(&mut frame, emit_ctx).await?;
        let released = deferred_events.remove(index);
        let released_bytes = serialize_to_string(&released.wire)
            .map_err(ExecutorError::JsonError)?
            .len();
        *deferred_bytes = deferred_bytes.saturating_sub(released_bytes);
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

    #[tokio::test]
    async fn cancelled_send_does_not_consume_lifecycle_or_sequence() {
        let request = request_context();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut accumulator = GatewayStreamAccumulator::new();
        let mut emit_ctx = StreamEmitContext {
            request: &request,
            sender: &sender,
            accumulator: &mut accumulator,
            output_offset: 0,
        };
        emit_stream_frame(&mut frame(0, serde_json::json!({"id":"msg_0"})), &mut emit_ctx)
            .await
            .unwrap();
        let created = || {
            crate::events::normalize_sse_line(
                r#"data: {"type":"response.created","response":{"id":"upstream","status":"in_progress"}}"#,
            )
            .unwrap()
        };
        let mut pending_frame = created();
        let mut pending = Box::pin(emit_stream_frame(&mut pending_frame, &mut emit_ctx));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        drop(pending);
        assert_eq!(receiver.try_recv().unwrap().sequence_number, 0);

        assert!(emit_stream_frame(&mut created(), &mut emit_ctx).await.unwrap());
        let delivered = receiver.try_recv().expect("cancelled creation was not delivered");
        assert_eq!(delivered.sequence_number, 1);
        assert!(delivered.content.contains("resp_test"));
    }

    #[tokio::test]
    async fn rejected_send_does_not_advance_sequence() {
        let request = request_context();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut accumulator = GatewayStreamAccumulator::with_max_stream_event_bytes(500 * 1024);
        let mut emit_ctx = StreamEmitContext {
            request: &request,
            sender: &sender,
            accumulator: &mut accumulator,
            output_offset: 0,
        };
        let error = emit_stream_frame(&mut frame(0, Value::String("x".repeat(1024 * 1024))), &mut emit_ctx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("stream event exceeded"));
        assert!(receiver.try_recv().is_err());
        emit_stream_frame(&mut frame(0, serde_json::json!({"id":"msg_0"})), &mut emit_ctx)
            .await
            .unwrap();
        assert_eq!(receiver.try_recv().unwrap().sequence_number, 0);
    }

    #[tokio::test]
    async fn local_and_upstream_events_share_numbering_but_not_id_rewriting() {
        let request = request_context();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let mut accumulator = GatewayStreamAccumulator::new();
        let mut created = crate::events::normalize_sse_line(
            r#"data: {"type":"response.created","response":{"id":"local","status":"in_progress"}}"#,
        )
        .unwrap();
        emit_gateway_event(&mut created, &mut accumulator, &sender)
            .await
            .unwrap();
        let mut progress = crate::events::normalize_sse_line(
            r#"data: {"type":"response.in_progress","response":{"id":"upstream","status":"in_progress"}}"#,
        )
        .unwrap();
        emit_stream_frame(
            &mut progress,
            &mut StreamEmitContext {
                request: &request,
                sender: &sender,
                accumulator: &mut accumulator,
                output_offset: 7,
            },
        )
        .await
        .unwrap();
        emit_gateway_event(
            &mut frame(7, serde_json::json!({"id":"local_item"})),
            &mut accumulator,
            &sender,
        )
        .await
        .unwrap();
        emit_stream_frame(
            &mut frame(1, serde_json::json!({"id":"upstream_item"})),
            &mut StreamEmitContext {
                request: &request,
                sender: &sender,
                accumulator: &mut accumulator,
                output_offset: 7,
            },
        )
        .await
        .unwrap();

        // A duplicate lifecycle event must not wait for a full client queue.
        let mut duplicate = Box::pin(emit_gateway_event(&mut created, &mut accumulator, &sender));
        assert!(matches!(
            futures::poll!(duplicate.as_mut()),
            std::task::Poll::Ready(Ok(()))
        ));
        drop(duplicate);

        let frames = std::iter::from_fn(|| receiver.try_recv().ok())
            .map(|event| {
                event
                    .content
                    .lines()
                    .find_map(crate::events::normalize_sse_line)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            frames.iter().map(EventFrame::sequence_number).collect::<Vec<_>>(),
            [Some(0), Some(1), Some(2), Some(3)]
        );
        assert_eq!(frames[0].wire.rest["response"]["id"], "local");
        assert_eq!(frames[1].wire.rest["response"]["id"], "resp_test");
        assert_eq!(frames[2].wire.output_index, Some(7));
        assert_eq!(frames[3].wire.output_index, Some(8));
    }

    #[tokio::test]
    async fn closed_local_sender_does_not_consume_presentation_state() {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);
        let mut accumulator = GatewayStreamAccumulator::new();
        let created = || {
            crate::events::normalize_sse_line(
                r#"data: {"type":"response.created","response":{"id":"local","status":"in_progress"}}"#,
            )
            .unwrap()
        };
        let error = emit_gateway_event(&mut created(), &mut accumulator, &sender)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("stream receiver closed"));
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        emit_gateway_event(&mut created(), &mut accumulator, &sender)
            .await
            .unwrap();
        assert_eq!(receiver.try_recv().unwrap().sequence_number, 0);
    }

    #[tokio::test]
    async fn completed_partial_flush_recharges_retained_frames() {
        let request = request_context();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut accumulator = GatewayStreamAccumulator::new();
        let mut emit_ctx = StreamEmitContext {
            request: &request,
            sender: &sender,
            accumulator: &mut accumulator,
            output_offset: 5,
        };
        let mut deferred = [2, 1, 3]
            .map(|index| frame(index, serde_json::json!({"id":format!("msg_{index}")})))
            .to_vec();
        let retained_bytes = serialize_to_string(&deferred[2].wire).unwrap().len();
        let mut bytes = deferred
            .iter()
            .map(|frame| serialize_to_string(&frame.wire).unwrap().len())
            .sum();
        let mut flush = Box::pin(flush_released_stream_frames(
            &mut emit_ctx,
            Some(3),
            &mut deferred,
            &mut bytes,
        ));
        assert!(futures::poll!(flush.as_mut()).is_pending());
        let first = receiver.try_recv().unwrap();
        assert_eq!(first.sequence_number, 0);
        assert!(matches!(futures::poll!(flush.as_mut()), std::task::Poll::Ready(Ok(()))));
        drop(flush);
        assert_eq!(receiver.try_recv().unwrap().sequence_number, 1);
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].wire.output_index, Some(3));
        assert_eq!(bytes, retained_bytes);
        flush_released_stream_frames(&mut emit_ctx, None, &mut deferred, &mut bytes)
            .await
            .unwrap();
        let last = receiver.try_recv().unwrap();
        let last = last
            .content
            .lines()
            .find_map(crate::events::normalize_sse_line)
            .unwrap();
        assert_eq!(last.wire.output_index, Some(8));
        assert_eq!(last.sequence_number(), Some(2));
        assert!(deferred.is_empty());
        assert_eq!(bytes, 0);
    }

    #[test]
    fn opaque_failure_logs_withhold_provider_text_while_default_logs_are_unchanged() {
        let failed = |code: &str| {
            crate::events::normalize_sse_line(&format!(
                r#"data: {{"type":"response.failed","response":{{"id":"resp_upstream","status":"failed","error":{{"code":"{code}","message":"reflected-sentinel-state"}},"incomplete_details":{{"reason":"reflected-sentinel-reason"}}}}}}"#
            ))
            .unwrap()
        };
        let logs = |detail: UpstreamFailureLog, code: &str| {
            let capture = crate::executor::log_capture::LogCapture::default();
            let guard = capture.install();
            log_upstream_failure(&failed(code), "resp_gateway", detail);
            drop(guard);
            capture.text()
        };

        let detailed = logs(UpstreamFailureLog::Detailed, "invalid_encrypted_content");
        assert!(detailed.contains("reflected-sentinel-state"), "{detailed}");
        assert!(detailed.contains("resp_upstream"), "{detailed}");

        let code_only = logs(UpstreamFailureLog::CodeOnly, "invalid_encrypted_content");
        assert!(code_only.contains("resp_gateway"), "{code_only}");
        assert!(code_only.contains("invalid_encrypted_content"), "{code_only}");
        assert!(!code_only.contains("reflected-sentinel"), "{code_only}");
        assert!(!code_only.contains("resp_upstream"), "{code_only}");

        let reflected_code = logs(UpstreamFailureLog::CodeOnly, "Reflected Sentinel");
        assert!(reflected_code.contains("<withheld>"), "{reflected_code}");
        assert!(!reflected_code.contains("Reflected"), "{reflected_code}");
    }

    #[tokio::test]
    async fn deferred_frames_have_an_entry_limit() {
        let request = request_context();
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let mut delivery = StreamDelivery::new(Some(sender));
        let registry = ToolRegistry::default();
        let delta = || {
            crate::events::normalize_sse_line(
                r#"data: {"type":"response.output_text.delta","output_index":1,"item_id":"msg_1","content_index":0,"delta":"x"}"#,
            )
            .unwrap()
        };
        for _ in 0..MAX_DEFERRED_STREAM_EVENTS {
            delivery
                .accept(
                    Translation {
                        frames: vec![delta()],
                        defer_from_output_index: Some(0),
                    },
                    &request,
                    &registry,
                    0,
                )
                .await
                .unwrap();
        }
        let bytes = delivery.deferred_bytes;
        let error = delivery
            .accept(
                Translation {
                    frames: vec![delta()],
                    defer_from_output_index: Some(0),
                },
                &request,
                &registry,
                0,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("1024 buffered events"));
        assert_eq!(delivery.deferred_events.len(), MAX_DEFERRED_STREAM_EVENTS);
        assert_eq!(delivery.deferred_bytes, bytes);
    }

    #[tokio::test]
    async fn indexless_frames_wait_for_the_defer_window_and_emit_last() {
        let request = request_context();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let mut accumulator = GatewayStreamAccumulator::new();
        let mut emit_ctx = StreamEmitContext {
            request: &request,
            sender: &sender,
            accumulator: &mut accumulator,
            output_offset: 0,
        };
        let mut indexless = EventFrame {
            event_type: SSEEventType::Other,
            payload: EventPayload::None,
            wire: WireEvent::new("response.custom"),
        };
        indexless
            .wire
            .rest
            .insert("marker".to_owned(), serde_json::json!("indexless"));
        let mut deferred = vec![indexless, frame(2, serde_json::json!({"id":"msg_2"}))];
        let mut bytes = deferred
            .iter()
            .map(|frame| serialize_to_string(&frame.wire).unwrap().len())
            .sum();

        flush_released_stream_frames(&mut emit_ctx, Some(2), &mut deferred, &mut bytes)
            .await
            .unwrap();
        assert_eq!(deferred.len(), 2);
        assert!(receiver.try_recv().is_err());

        flush_released_stream_frames(&mut emit_ctx, None, &mut deferred, &mut bytes)
            .await
            .unwrap();
        let first = receiver.try_recv().unwrap();
        let second = receiver.try_recv().unwrap();
        assert!(first.content.contains("msg_2"));
        assert!(second.content.contains("indexless"));
        assert!(deferred.is_empty());
        assert_eq!(bytes, 0);
    }

    #[tokio::test]
    async fn cancelling_a_flush_preserves_unsent_frames_and_byte_accounting() {
        let request = request_context();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut accumulator = GatewayStreamAccumulator::new();
        let mut emit_ctx = StreamEmitContext {
            request: &request,
            sender: &sender,
            accumulator: &mut accumulator,
            output_offset: 0,
        };
        let mut deferred = vec![
            frame(1, serde_json::json!({"id":"msg_1"})),
            frame(2, serde_json::json!({"id":"msg_2"})),
        ];
        let second_bytes = serialize_to_string(&deferred[1].wire).unwrap().len();
        let mut bytes = deferred
            .iter()
            .map(|frame| serialize_to_string(&frame.wire).unwrap().len())
            .sum();
        let mut flush = Box::pin(flush_released_stream_frames(
            &mut emit_ctx,
            None,
            &mut deferred,
            &mut bytes,
        ));
        assert!(futures::poll!(flush.as_mut()).is_pending());
        assert_eq!(receiver.try_recv().unwrap().sequence_number, 0);
        drop(flush);
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].wire.output_index, Some(2));
        assert_eq!(bytes, second_bytes);
    }

    #[tokio::test]
    async fn final_deferred_delivery_emits_indexless_frames_last() {
        let request = request_context();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let mut accumulator = GatewayStreamAccumulator::new();
        let mut indexless = EventFrame {
            event_type: SSEEventType::Other,
            payload: EventPayload::None,
            wire: WireEvent::new("response.custom"),
        };
        indexless
            .wire
            .rest
            .insert("marker".to_owned(), serde_json::json!("indexless"));

        emit_deferred_stream_events(
            vec![
                indexless,
                frame(3, serde_json::json!({"id":"msg_3"})),
                frame(1, serde_json::json!({"id":"msg_1"})),
            ],
            &request,
            &mut accumulator,
            &sender,
            0,
        )
        .await
        .unwrap();

        let events = [
            receiver.try_recv().unwrap(),
            receiver.try_recv().unwrap(),
            receiver.try_recv().unwrap(),
        ];
        assert!(events[0].content.contains("msg_1"));
        assert!(events[1].content.contains("msg_3"));
        assert!(events[2].content.contains("indexless"));
        assert_eq!(events.map(|event| event.sequence_number), [0, 1, 2]);
    }
}
