//! Response accumulation and parsing utilities.
//!
//! Handles both streaming (SSE) and non-streaming JSON response formats,
//! accumulating chunks into a unified `ResponsePayload` structure.
//!
//! Streaming path uses a channel + `spawn_blocking` so that SSE JSON parsing
//! runs on a blocking thread while the async task continues reading from the
//! network — keeping the tokio executor thread free between chunk arrivals.

use std::pin::Pin;
use std::sync::mpsc;

use futures::{Stream, StreamExt};

use crate::events::{EventFrame, EventPayload, SSEEventType, normalize_sse_line};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::event::{MessageStatus, ResponseStatus};
use crate::types::io::{OutputItem, OutputMessage, OutputTextContent, ResponseUsage};
use crate::types::request_response::{IncompleteDetails, ResponsePayload};
use crate::utils::common::{deserialize_from_str, deserialize_from_value_opt};
use crate::utils::uuid7_str;

/// Accumulates LLM response chunks from streaming or non-streaming sources.
#[derive(Debug)]
pub struct ResponseAccumulator {
    response_id: String,
    conversation_id: Option<String>,
    output: Vec<OutputItem>,
    usage: Option<ResponseUsage>,
    status: ResponseStatus,
    incomplete_details: Option<IncompleteDetails>,
    // In-flight message state — owned here so process_sse_line takes only &mut self.
    current_message: Option<OutputMessage>,
    accumulated_text: String,
}

impl ResponseAccumulator {
    /// Creates a new response accumulator.
    #[must_use]
    pub fn new(response_id: String, conversation_id: Option<String>) -> Self {
        Self {
            response_id,
            conversation_id,
            output: Vec::new(),
            usage: None,
            status: ResponseStatus::InProgress,
            incomplete_details: None,
            current_message: None,
            accumulated_text: String::new(),
        }
    }

    /// Parses a non-streaming JSON response body.
    ///
    /// # Errors
    /// Returns `ExecutorError::ParseError` if JSON parsing fails or required fields are missing.
    pub fn from_json(body: &str, conversation_id: Option<&str>) -> ExecutorResult<Self> {
        let json: serde_json::Value = deserialize_from_str(body).map_err(ExecutorError::JsonError)?;

        let response_id = json["id"]
            .as_str()
            .ok_or_else(|| ExecutorError::ParseError("missing 'id' field in response".into()))?
            .to_string();

        let output = json["output"]
            .as_array()
            .map(|items| {
                let mut out = Vec::with_capacity(items.len());
                out.extend(
                    items
                        .iter()
                        .filter_map(|item| deserialize_from_value_opt::<OutputItem>(item.clone())),
                );
                out
            })
            .unwrap_or_default();

        let status = json["status"]
            .as_str()
            .map_or(ResponseStatus::Completed, |s| s.parse().unwrap_or_default());

        let usage = deserialize_from_value_opt::<ResponseUsage>(json["usage"].clone());

        Ok(Self {
            response_id,
            conversation_id: conversation_id.map(str::to_string),
            output,
            usage,
            status,
            incomplete_details: None,
            current_message: None,
            accumulated_text: String::new(),
        })
    }

    /// Accumulates an async stream of raw SSE lines with parallel processing.
    ///
    /// The async task feeds raw SSE lines through a channel while a `spawn_blocking`
    /// worker handles JSON parsing on a blocking thread — keeping the tokio executor
    /// free between chunk arrivals.
    ///
    /// # Errors
    /// Returns `ExecutorError::ParseError` if chunk parsing fails, or
    /// `ExecutorError::StreamError` if the stream or worker encounters an error.
    pub async fn from_stream(
        mut stream: Pin<Box<dyn Stream<Item = Result<String, ExecutorError>> + Send>>,
        conversation_id: Option<&str>,
    ) -> ExecutorResult<Self> {
        let (tx, rx) = mpsc::channel::<String>();
        // Convert to owned here — spawn_blocking closure must be 'static.
        let conv_id_owned = conversation_id.map(str::to_string);

        // Spawn blocking task: JSON parsing is CPU-bound, runs off the async executor.
        let worker_handle = tokio::task::spawn_blocking(move || Self::process_stream_chunks(rx, conv_id_owned));

        // Feed raw SSE lines from the async stream to the blocking worker.
        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    if tx.send(chunk).is_err() {
                        // Worker exited early (e.g. saw ResponseDone).
                        break;
                    }
                }
                Err(e) => return Err(e),
            }
        }

        // Signal EOF to worker.
        drop(tx);

        // Properly async join — does not block the tokio executor thread.
        worker_handle
            .await
            .map_err(|_| ExecutorError::StreamError("Worker thread panicked".into()))
    }

    /// Worker function that processes SSE lines from the channel (runs on blocking thread).
    fn process_stream_chunks(rx: mpsc::Receiver<String>, conversation_id: Option<String>) -> Self {
        let mut acc = Self::new(uuid7_str("resp_"), conversation_id);
        for line in rx {
            acc.process_sse_line(&line);
        }
        acc.finalize_current_message();
        if acc.status == ResponseStatus::InProgress {
            acc.status = ResponseStatus::Completed;
        }
        acc
    }

    /// Processes pre-collected raw SSE lines synchronously.
    ///
    /// Useful when lines have already been buffered (e.g. replaying a recorded stream).
    /// Prefer [`from_stream`](Self::from_stream) for live async streams.
    /// Line parse errors are silently skipped — this function is infallible.
    #[must_use]
    pub fn from_sse_lines(lines: impl IntoIterator<Item = String>, conversation_id: Option<&str>) -> Self {
        let mut acc = Self::new(uuid7_str("resp_"), conversation_id.map(str::to_string));
        for line in lines {
            acc.process_sse_line(&line);
        }
        acc.finalize_current_message();
        acc
    }

    /// Closes the in-flight message, pushing it to `output` with accumulated text.
    fn finalize_current_message(&mut self) {
        if let Some(mut msg) = self.current_message.take() {
            if !self.accumulated_text.is_empty() {
                msg.content.push(OutputTextContent::new(&self.accumulated_text));
            }
            msg.status = MessageStatus::Completed.as_str().to_string();
            self.output.push(OutputItem::Message(msg));
        }
        self.accumulated_text.clear();
    }

    /// Processes a single raw SSE line, updating accumulator state.
    ///
    /// Non-`data:` lines, `[DONE]`, and malformed JSON are silently skipped.
    fn process_sse_line(&mut self, line: &str) {
        if let Some(frame) = normalize_sse_line(line) {
            self.process_event(&frame);
        }
    }

    /// Processes a typed [`EventFrame`], updating accumulator state.
    ///
    /// This is the core state machine — callers that already have a normalized
    /// frame (e.g. [`StreamTee`](future)) can call this directly without
    /// re-parsing from a raw line.
    pub fn process_event(&mut self, frame: &EventFrame) {
        match (&frame.event_type, &frame.payload) {
            (SSEEventType::ResponseCreated, EventPayload::Response { id, .. }) if !id.is_empty() => {
                self.response_id.clone_from(id);
            }
            (_, EventPayload::OutputItemAdded { item_id, .. }) => {
                self.finalize_current_message();
                let id = if item_id.is_empty() {
                    uuid7_str("msg_")
                } else {
                    item_id.clone()
                };
                self.current_message = Some(OutputMessage::new(&id, MessageStatus::InProgress.as_str()));
            }
            (_, EventPayload::TextDelta { delta, .. }) => {
                self.accumulated_text.push_str(delta);
            }
            (SSEEventType::ResponseCompleted, EventPayload::Response { usage, .. }) => {
                self.finalize_current_message();
                self.status = ResponseStatus::Completed;
                if let Some(u) = usage {
                    if let Ok(parsed) = serde_json::from_value::<ResponseUsage>(u.clone()) {
                        self.usage = Some(parsed);
                    }
                }
            }
            _ => {}
        }
    }

    /// Marks the response as incomplete due to an error or interruption.
    pub fn mark_incomplete(&mut self, reason: impl Into<String>) {
        self.status = ResponseStatus::Incomplete;
        self.incomplete_details = Some(IncompleteDetails {
            reason: Some(reason.into()),
        });
    }

    /// Finalizes the accumulator into a `ResponsePayload`.
    ///
    /// The caller supplies fields that come from the original request, not from
    /// the LLM response stream.
    #[must_use]
    pub fn finalize(
        self,
        model: &str,
        previous_response_id: Option<&str>,
        instructions: Option<&str>,
    ) -> ResponsePayload {
        ResponsePayload {
            id: self.response_id,
            object: "response".to_string(),
            created_at: chrono::Utc::now().timestamp(),
            model: model.to_string(),
            status: self.status.as_str().to_string(),
            output: self.output,
            usage: self.usage,
            incomplete_details: self.incomplete_details,
            error: None,
            previous_response_id: previous_response_id.map(str::to_string),
            conversation_id: self.conversation_id,
            instructions: instructions.map(str::to_string),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_accumulator_new() {
        let acc = ResponseAccumulator::new("resp_123".into(), Some("conv_456".into()));
        assert_eq!(acc.response_id, "resp_123");
        assert_eq!(acc.conversation_id, Some("conv_456".into()));
        assert_eq!(acc.status, ResponseStatus::InProgress);
    }

    #[test]
    fn test_accumulator_mark_incomplete() {
        let mut acc = ResponseAccumulator::new("resp_123".into(), None);
        acc.mark_incomplete("Stream interrupted");
        assert_eq!(acc.status, ResponseStatus::Incomplete);
        assert!(acc.incomplete_details.is_some());
    }

    #[test]
    fn test_accumulator_finalize() {
        let acc = ResponseAccumulator::new("resp_123".into(), Some("conv_456".into()));
        let payload = acc.finalize("gpt-4o", Some("resp_prev"), Some("be helpful"));
        assert_eq!(payload.id, "resp_123");
        assert_eq!(payload.model, "gpt-4o");
        assert_eq!(payload.conversation_id, Some("conv_456".into()));
        assert_eq!(payload.previous_response_id, Some("resp_prev".into()));
        assert_eq!(payload.instructions, Some("be helpful".into()));
        assert_eq!(payload.status, ResponseStatus::InProgress.as_str());
    }

    #[test]
    fn test_accumulator_from_sse_lines_empty() {
        let acc = ResponseAccumulator::from_sse_lines(vec![], None);
        assert_eq!(acc.status, ResponseStatus::InProgress);
        assert!(acc.output.is_empty());
    }

    #[test]
    fn test_accumulator_text_delta_assigned_to_message() {
        let lines = vec![
            r#"data: {"type":"response.created","response":{"id":"resp_abc"}}"#.to_string(),
            r#"data: {"type":"response.output_item.added","item":{"id":"msg_1"}}"#.to_string(),
            r#"data: {"type":"response.output_text.delta","delta":"Hello"}"#.to_string(),
            r#"data: {"type":"response.output_text.delta","delta":" world"}"#.to_string(),
            r#"data: {"type":"response.done","response":{"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}"#.to_string(),
        ];

        let acc = ResponseAccumulator::from_sse_lines(lines, None);
        assert_eq!(acc.status, ResponseStatus::Completed);
        assert_eq!(acc.output.len(), 1);

        if let OutputItem::Message(msg) = &acc.output[0] {
            assert_eq!(msg.content.len(), 1);
            assert_eq!(msg.content[0].text, "Hello world");
        } else {
            panic!("expected OutputItem::Message");
        }

        assert!(acc.usage.is_some());
        let usage = acc.usage.unwrap();
        assert_eq!(usage.total_tokens, 7);
    }

    #[test]
    fn test_message_status_enum() {
        assert_eq!(MessageStatus::Completed.as_str(), "completed");
        assert_eq!(MessageStatus::InProgress.as_str(), "in_progress");
    }
}
