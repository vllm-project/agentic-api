use serde_json::Value;
use tokio::sync::mpsc;

use crate::events::{EventFrame, EventPayload, SSEEventType, WireEvent, normalize_sse_line};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::request_response::ResponsePayload;
use crate::utils::common::serialize_to_string;

pub struct GatewayStreamAccumulator {
    next_sequence_number: u64,
    emitted_created: bool,
    emitted_in_progress: bool,
}

pub(crate) struct GatewayStreamContext<'a> {
    sender: &'a mpsc::UnboundedSender<String>,
    accumulator: &'a mut GatewayStreamAccumulator,
}

impl<'a> GatewayStreamContext<'a> {
    pub(crate) fn new(
        sender: &'a mpsc::UnboundedSender<String>,
        accumulator: &'a mut GatewayStreamAccumulator,
    ) -> Self {
        Self { sender, accumulator }
    }

    pub(crate) fn process_event(&mut self, frame: &mut EventFrame, output_offset: usize) -> ExecutorResult<()> {
        if self.accumulator.process_event(frame, output_offset) {
            emit_sse_frame(self.sender, frame)?;
        }
        Ok(())
    }
}

impl GatewayStreamAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_sequence_number: 0,
            emitted_created: false,
            emitted_in_progress: false,
        }
    }

    pub fn process_sse_line(&mut self, line: &str, output_offset: usize) -> Option<EventFrame> {
        let mut frame = normalize_sse_line(line)?;
        self.process_event(&mut frame, output_offset).then_some(frame)
    }

    pub fn process_event(&mut self, frame: &mut EventFrame, output_offset: usize) -> bool {
        if !self.should_emit_lifecycle(frame.event_type) {
            return false;
        }
        let sequence_number = Some(self.take_sequence_number());
        frame.sequence_number = sequence_number;
        frame.wire.sequence_number = sequence_number;
        rebase_output_index(&mut frame.wire, output_offset);
        true
    }

    pub(crate) fn terminal_response_chunk(&mut self, payload: &ResponsePayload) -> ExecutorResult<String> {
        let mut frame = terminal_response_frame(payload)?;
        self.process_event(&mut frame, 0);
        serialize_sse_frame(&frame)
    }

    pub(crate) fn error_chunk(&mut self, message: &str) -> String {
        let mut frame = error_frame(message);
        self.process_event(&mut frame, 0);
        serialize_sse_frame(&frame).unwrap_or_else(|_| "data: {\"type\":\"error\"}\n\n".to_owned())
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

fn rebase_output_index(wire: &mut WireEvent, output_offset: usize) {
    let Some(offset) = u64::try_from(output_offset).ok().filter(|offset| *offset > 0) else {
        return;
    };
    if let Some(index) = wire.output_index {
        wire.output_index = Some(index.saturating_add(offset));
    }
}

fn terminal_response_frame(payload: &ResponsePayload) -> ExecutorResult<EventFrame> {
    let event_type = match payload.terminal_event_type() {
        "response.incomplete" => SSEEventType::ResponseIncomplete,
        "response.failed" => SSEEventType::ResponseFailed,
        "response.in_progress" => SSEEventType::ResponseInProgress,
        _ => SSEEventType::ResponseCompleted,
    };
    let mut wire = WireEvent::new(event_type.as_str());
    wire.rest.insert(
        "response".to_owned(),
        serde_json::to_value(payload).map_err(ExecutorError::JsonError)?,
    );
    Ok(EventFrame::synthetic(event_type, wire))
}

fn error_frame(message: &str) -> EventFrame {
    let mut wire = WireEvent::new("error");
    wire.rest.insert(
        "error".to_owned(),
        serde_json::json!({
            "message": message,
        }),
    );
    EventFrame {
        event_type: SSEEventType::Other,
        payload: EventPayload::Raw(wire.to_value()),
        sequence_number: wire.sequence_number,
        wire,
    }
}

pub(super) fn synthetic_event(event_type: SSEEventType, rest: impl IntoIterator<Item = (String, Value)>) -> EventFrame {
    let mut wire = WireEvent::new(event_type.as_str());
    wire.rest.extend(rest);
    EventFrame::synthetic(event_type, wire)
}

fn emit_sse_frame(sender: &mpsc::UnboundedSender<String>, frame: &EventFrame) -> ExecutorResult<()> {
    sender
        .send(serialize_sse_frame(frame)?)
        .map_err(|_| ExecutorError::StreamError("stream receiver closed while emitting gateway event".to_owned()))
}

fn serialize_sse_frame(frame: &EventFrame) -> ExecutorResult<String> {
    let event_json = serialize_to_string(&frame.wire).map_err(ExecutorError::JsonError)?;
    Ok(format!("data: {event_json}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_sse_line_numbers_and_rebases_output_index() {
        let mut accumulator = GatewayStreamAccumulator::new();
        let frame = accumulator
            .process_sse_line(
                r#"data: {"type":"response.output_text.delta","output_index":2,"delta":"hi"}"#,
                3,
            )
            .expect("line should normalize");

        assert_eq!(frame.sequence_number, Some(0));
        assert_eq!(frame.wire.sequence_number, Some(0));
        assert_eq!(frame.wire.output_index, Some(5));
        assert_eq!(frame.wire.rest["delta"], "hi");
    }
}
