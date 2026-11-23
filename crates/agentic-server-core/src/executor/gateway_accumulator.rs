use crate::config::DEFAULT_MAX_STREAM_EVENT_BYTES;
use crate::events::{EventFrame, EventPayload, SSEEventType, WireEvent};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::request_response::ResponsePayload;
use crate::utils::common::{serialize_to_string, serialize_to_value};
use serde_json::Value;

/// Cross-round presentation state: sequence numbers and response-start
/// deduplication. `StreamRelay` owns the instance for a response.
#[derive(Clone)]
pub struct GatewayStreamAccumulator {
    next_sequence_number: u64,
    emitted_created: bool,
    emitted_in_progress: bool,
    max_stream_event_bytes: usize,
}

/// Ordered delivery messages. Flush barriers carry no wire data or sequence.
pub(super) enum StreamEvent {
    Frame(StreamFrame),
    Flush(tokio::sync::oneshot::Sender<()>),
}

pub(super) struct StreamFrame {
    pub(super) content: String,
    pub(super) sequence_number: u64,
    /// An output-text delta, for the relay's first-text timing.
    pub(super) text: bool,
}

#[cfg(test)]
impl StreamEvent {
    pub(super) fn into_frame(self) -> StreamFrame {
        match self {
            Self::Frame(frame) => frame,
            Self::Flush(_) => panic!("expected a wire frame, received a flush barrier"),
        }
    }
}

/// Executor streams keep only a small number of bounded-size events ahead of
/// their consumer so downstream backpressure also pauses upstream reads.
pub(super) const STREAM_EVENT_BUFFER: usize = 1;
#[cfg(test)]
pub(super) const MAX_STREAM_EVENT_BYTES: usize = DEFAULT_MAX_STREAM_EVENT_BYTES;

impl GatewayStreamAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_stream_event_bytes(DEFAULT_MAX_STREAM_EVENT_BYTES)
    }

    #[must_use]
    pub fn with_max_stream_event_bytes(max_stream_event_bytes: usize) -> Self {
        Self {
            next_sequence_number: 0,
            emitted_created: false,
            emitted_in_progress: false,
            max_stream_event_bytes,
        }
    }

    /// Sequence number that the next successfully enqueued event will receive.
    ///
    /// Typed gateway event construction may use this value; presenting a frame
    /// stamps it, and the sink's commit of [`Self::present`]'s state advances it.
    pub(crate) const fn upcoming_sequence_number(&self) -> u64 {
        self.next_sequence_number
    }

    /// Stamps `frame` and commits the state at once, skipping a response start
    /// event already presented. Executor sinks use [`Self::present`] instead,
    /// which checks the event size and commits only after delivery.
    #[must_use]
    pub fn process_event(&mut self, frame: &mut EventFrame) -> bool {
        if !self.should_emit_lifecycle(frame.event_type) {
            return false;
        }
        self.stamp_event(frame);
        true
    }

    fn stamp_event(&mut self, frame: &mut EventFrame) {
        frame.wire.sequence_number = Some(self.take_sequence_number());
    }

    /// The client frame for `frame`, stamped and serialized within the event size
    /// limit, with the presentation state that follows it. `None` is a response
    /// start event this stream already presented. The state is a copy: a sink
    /// commits it only after accepting the frame, so a frame that is never
    /// delivered consumes no sequence number or response start.
    pub(super) fn present(&self, frame: &mut EventFrame) -> ExecutorResult<Option<(StreamFrame, Self)>> {
        let mut next = self.clone();
        if !next.should_emit_lifecycle(frame.event_type) {
            return Ok(None);
        }
        let sequence_number = next.take_sequence_number();
        frame.wire.sequence_number = Some(sequence_number);
        let content = checked_stream_event(frame, self.max_stream_event_bytes)?;
        Ok(Some((
            StreamFrame {
                content,
                sequence_number,
                text: frame.event_type == SSEEventType::OutputTextDelta,
            },
            next,
        )))
    }

    /// Stamps and serializes gateway-built frames in order. Response lifecycle
    /// events already presented by this stream are skipped.
    pub(crate) fn frames_chunk(&mut self, frames: Vec<EventFrame>) -> ExecutorResult<String> {
        let mut chunk = String::new();
        for mut frame in frames {
            if let Some((presented, next)) = self.present(&mut frame)? {
                chunk.push_str(&presented.content);
                *self = next;
            }
        }
        Ok(chunk)
    }

    pub(crate) fn terminal_response_chunk(&mut self, payload: &ResponsePayload) -> ExecutorResult<String> {
        let mut frame = terminal_response_frame(payload)?;
        self.stamp_event(&mut frame);
        checked_stream_event(&frame, self.max_stream_event_bytes)
    }

    pub(crate) fn executor_error_chunk_at(error: &ExecutorError, sequence_number: u64) -> String {
        let mut frame = executor_error_frame(error);
        frame.wire.sequence_number = Some(sequence_number);
        checked_stream_event(&frame, DEFAULT_MAX_STREAM_EVENT_BYTES).unwrap_or_else(|_| {
            error_sse_chunk("executor error event exceeded the response size limit", sequence_number)
        })
    }

    fn should_emit_lifecycle(&mut self, event_type: SSEEventType) -> bool {
        match event_type {
            SSEEventType::ResponseCreated => take_once(&mut self.emitted_created),
            SSEEventType::ResponseInProgress => take_once(&mut self.emitted_in_progress),
            _ => true,
        }
    }

    fn take_sequence_number(&mut self) -> u64 {
        let sequence_number = self.next_sequence_number;
        self.next_sequence_number = self.next_sequence_number.saturating_add(1);
        sequence_number
    }
}

impl Default for GatewayStreamAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

fn take_once(already_taken: &mut bool) -> bool {
    if *already_taken {
        false
    } else {
        *already_taken = true;
        true
    }
}

pub(super) fn terminal_response_frame(payload: &ResponsePayload) -> ExecutorResult<EventFrame> {
    let event_type = match payload.terminal_event_type() {
        "response.incomplete" => SSEEventType::ResponseIncomplete,
        "response.failed" => SSEEventType::ResponseFailed,
        "response.in_progress" => SSEEventType::ResponseInProgress,
        _ => SSEEventType::ResponseCompleted,
    };
    let mut rest = serde_json::Map::new();
    rest.insert(
        "response".to_owned(),
        serialize_to_value(payload).map_err(ExecutorError::JsonError)?,
    );
    EventFrame::synthetic(event_type, rest)
        .ok_or_else(|| ExecutorError::StreamError("terminal response event has no wire representation".to_owned()))
}

pub(super) fn executor_error_frame(error: &ExecutorError) -> EventFrame {
    let mut wire = WireEvent::new("error");
    wire.rest
        .insert("status".to_owned(), serde_json::json!(error.http_status().as_u16()));
    wire.rest.insert("error".to_owned(), error.response_error());
    EventFrame {
        event_type: SSEEventType::Other,
        payload: EventPayload::None,
        wire,
    }
}

pub(super) fn error_sse_chunk(message: &str, sequence_number: u64) -> String {
    let code = "server_error";
    let mut wire = WireEvent::new("error");
    wire.sequence_number = Some(sequence_number);
    wire.rest.insert("status".to_owned(), serde_json::json!(500));
    wire.rest.insert(
        "error".to_owned(),
        serde_json::json!({
            "message": message,
            "type": code,
            "code": code,
        }),
    );
    let frame = EventFrame {
        event_type: SSEEventType::Other,
        payload: EventPayload::None,
        wire,
    };
    serialize_sse_frame(&frame).unwrap_or_else(|_| {
        format!("event: error\ndata: {{\"type\":\"error\",\"status\":500,\"sequence_number\":{sequence_number}}}\n\n")
    })
}

pub(super) fn synthetic_event(
    event_type: SSEEventType,
    rest: impl IntoIterator<Item = (String, Value)>,
) -> ExecutorResult<EventFrame> {
    EventFrame::synthetic(event_type, rest.into_iter().collect())
        .ok_or_else(|| ExecutorError::StreamError("synthetic event has no wire representation".to_owned()))
}

/// A gateway-built event for the item at `output_index`.
pub(super) fn synthetic_event_at(
    event_type: SSEEventType,
    output_index: u64,
    rest: impl IntoIterator<Item = (String, Value)>,
) -> ExecutorResult<EventFrame> {
    let mut frame = synthetic_event(event_type, rest)?;
    frame.wire.output_index = Some(output_index);
    Ok(frame)
}

pub(super) fn checked_stream_event(frame: &EventFrame, max_bytes: usize) -> ExecutorResult<String> {
    let content = serialize_sse_frame(frame)?;
    if content.len() > max_bytes {
        return Err(ExecutorError::ResourceLimitExceeded {
            limit: crate::executor::error::ResourceLimit::StreamEvent,
            max_bytes,
        });
    }
    Ok(content)
}

fn serialize_sse_frame(frame: &EventFrame) -> ExecutorResult<String> {
    let event_json = serialize_to_string(&frame.wire).map_err(ExecutorError::JsonError)?;
    let event_name =
        frame.wire.event_type.as_deref().filter(|event_name| {
            !event_name.is_empty() && !event_name.bytes().any(|byte| matches!(byte, b'\r' | b'\n'))
        });
    let event_name_len = event_name.map_or(0, str::len);
    let mut chunk = String::with_capacity(event_name_len + event_json.len() + 16);
    if let Some(event_name) = event_name {
        chunk.push_str("event: ");
        chunk.push_str(event_name);
        chunk.push('\n');
    }
    chunk.push_str("data: ");
    chunk.push_str(&event_json);
    chunk.push_str("\n\n");
    Ok(chunk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StorageError;
    use crate::events::normalize_sse_line;
    use crate::tool::ToolError;

    fn parse_named_sse_event(chunk: &str) -> (&str, serde_json::Value) {
        let body = chunk.strip_suffix("\n\n").expect("SSE event terminator");
        let (event_line, data_line) = body.split_once('\n').expect("named SSE event and data lines");
        let event_name = event_line.strip_prefix("event: ").expect("SSE event prefix");
        let data = data_line.strip_prefix("data: ").expect("SSE data prefix");
        let event = serde_json::from_str(data).expect("valid event JSON");
        (event_name, event)
    }

    #[test]
    fn process_event_numbers_without_rebasing() {
        let mut accumulator = GatewayStreamAccumulator::new();
        let mut frame =
            normalize_sse_line(r#"data: {"type":"response.output_text.delta","output_index":2,"delta":"hi"}"#)
                .expect("line should normalize");
        assert!(accumulator.process_event(&mut frame));

        assert_eq!(frame.sequence_number(), Some(0));
        assert_eq!(frame.wire.sequence_number, Some(0));
        // The relay rebases upstream frames onto their round (`emit_upstream`); presentation keeps the index.
        assert_eq!(frame.wire.output_index, Some(2));
        assert_eq!(frame.wire.rest["delta"], "hi");
    }

    #[test]
    fn error_sse_chunk_escapes_error_messages() {
        let chunk = error_sse_chunk("task failed: \"unexpected\"\nretry", 7);
        let (event_name, event) = parse_named_sse_event(&chunk);

        assert_eq!(event_name, "error");
        assert_eq!(event["type"], "error");
        assert_eq!(event["sequence_number"], 7);
        assert_eq!(event["error"]["message"], "task failed: \"unexpected\"\nretry");
    }

    #[test]
    fn executor_error_chunk_uses_the_consumers_next_sequence_number() {
        let error = ExecutorError::StreamError("buffer limit".to_owned());
        let chunk = GatewayStreamAccumulator::executor_error_chunk_at(&error, 4);
        let (event_name, event) = parse_named_sse_event(&chunk);

        assert_eq!(event_name, "error");
        assert_eq!(event["sequence_number"], 4);
    }

    #[test]
    fn executor_conflict_sse_chunk_uses_client_conflict_contract() {
        let error = ExecutorError::Persistence(Box::new(ExecutorError::ConversationLocked {
            source: StorageError::ConversationConflict {
                conversation_id: "conv_test".to_owned(),
            },
        }));
        let chunk = GatewayStreamAccumulator::executor_error_chunk_at(&error, 0);
        let (event_name, event) = parse_named_sse_event(&chunk);

        assert_eq!(event_name, "error");
        assert_eq!(event["type"], "error");
        assert_eq!(event["status"], 400);
        assert_eq!(
            event["error"],
            serde_json::json!({
                "message": "conversation changed while the response was being generated; retry the request",
                "type": "invalid_request_error",
                "code": "conversation_locked",
                "param": "conversation"
            })
        );
    }

    #[test]
    fn missing_tool_output_sse_chunk_matches_openai_error_contract() {
        let error = ExecutorError::Tool(ToolError::MissingOutput {
            call_id: "call_test".to_owned(),
        });
        let chunk = GatewayStreamAccumulator::executor_error_chunk_at(&error, 0);
        let (event_name, event) = parse_named_sse_event(&chunk);

        assert_eq!(event_name, "error");
        assert_eq!(event["status"], 400);
        assert_eq!(
            event["error"],
            serde_json::json!({
                "message": "No tool output found for function call call_test.",
                "type": "invalid_request_error",
                "param": "input",
                "code": null
            })
        );
    }

    #[test]
    fn emits_in_progress_terminal_event_after_lifecycle_event() {
        let mut accumulator = GatewayStreamAccumulator::new();
        let mut frame = normalize_sse_line(r#"data: {"type":"response.in_progress"}"#).expect("line should normalize");
        assert!(
            accumulator.process_event(&mut frame),
            "first lifecycle event should be emitted"
        );

        let payload: ResponsePayload = serde_json::from_value(serde_json::json!({
            "id": "resp_1",
            "object": "response",
            "created_at": 0,
            "model": "test",
            "status": "in_progress",
            "output": [],
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        }))
        .expect("valid response payload");

        let chunk = accumulator
            .terminal_response_chunk(&payload)
            .expect("terminal event serializes");
        assert!(chunk.starts_with("event: response.in_progress\n"));
        assert!(chunk.contains("\"type\":\"response.in_progress\""));
        assert!(chunk.contains("\"sequence_number\":1"));
    }

    #[test]
    fn serialize_sse_frame_uses_wire_event_type_as_event_name() {
        let frame = synthetic_event(SSEEventType::ResponseCreated, []).expect("synthetic event");
        let chunk = serialize_sse_frame(&frame).expect("event serializes");
        let (event_name, event) = parse_named_sse_event(&chunk);

        assert_eq!(event_name, "response.created");
        assert_eq!(event["type"], event_name);
    }

    #[test]
    fn serialize_sse_frame_omits_invalid_event_name() {
        let frame = EventFrame {
            event_type: SSEEventType::Other,
            payload: EventPayload::None,
            wire: WireEvent::new("error\nevent: injected"),
        };
        let chunk = serialize_sse_frame(&frame).expect("event serializes");

        assert!(chunk.starts_with("data: "));
        assert!(!chunk.contains("\nevent: injected\n"));
    }
}
