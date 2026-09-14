//! Response accumulation and parsing utilities.
//!
//! Handles both streaming (SSE) and non-streaming JSON response formats,
//! accumulating semantic events into a unified `ResponsePayload` structure.
//!
//! The executor processes SSE lines inline through `AgentPipeline` and its
//! round-scoped ingestion state. The separate [`ResponseAccumulator::from_stream`]
//! convenience method uses a channel and `spawn_blocking` worker; that worker is
//! not the executor's main streaming path.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::mpsc;

use futures::{Stream, StreamExt};

use crate::events::{
    ClassifiedSseLine, EventFrame, EventPayload, SSEEventType, SSEItemType, SseLine, ValidatedFrame,
    expected_item_type, normalize_sse_data_checked, output_item_identity, validate_frame,
};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::event::ResponseStatus;
use crate::types::io::{FunctionToolCall, OutputItem, ResponseUsage};
use crate::types::request_response::{IncompleteDetails, ResponsePayload};
use crate::utils::common::{deserialize_from_str, deserialize_from_value_opt};
use crate::utils::uuid7_str;

mod completion;
mod json;
mod slot;

use slot::{ActiveItem, ItemIdentity, OutputIndex, SlotMap, SlotState};

/// Validation policy selected once for an accumulator's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Validation {
    Strict,
    Lenient,
}

#[derive(Debug, Default)]
struct CallIdObservation {
    first: Option<String>,
    changed: bool,
}

impl CallIdObservation {
    fn observe(&mut self, call_id: Option<&str>) {
        let Some(call_id) = call_id.filter(|call_id| !call_id.is_empty()) else {
            return;
        };
        match self.first.as_deref() {
            Some(first) if first != call_id => self.changed = true,
            None => self.first = Some(call_id.to_owned()),
            Some(_) => {}
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum StreamLifecycle {
    #[default]
    AwaitingCreated,
    Created,
    InProgress,
    Terminal,
}

enum EventDisposition {
    Emit(Option<OutputIndex>),
    Ignore,
}

impl EventDisposition {
    fn item(index: Option<OutputIndex>) -> Self {
        match index {
            Some(index) => Self::Emit(Some(index)),
            None => Self::Ignore,
        }
    }

    fn into_frame(self, mut frame: EventFrame) -> Option<EventFrame> {
        match self {
            Self::Emit(index) => {
                if let Some(index) = index {
                    frame.set_output_index(index.get());
                }
                Some(frame)
            }
            Self::Ignore => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct AccumulatedFunctionCall<'a> {
    pub(super) item: &'a FunctionToolCall,
    pub(super) output_index: u32,
    arguments: &'a str,
}

impl AccumulatedFunctionCall<'_> {
    pub(super) fn arguments(&self) -> &str {
        if self.item.arguments.is_empty() {
            self.arguments
        } else {
            &self.item.arguments
        }
    }
}

/// Accumulates LLM response chunks from streaming or non-streaming sources.
#[derive(Debug)]
pub struct ResponseAccumulator {
    validation: Validation,
    response_id: String,
    conversation_id: Option<String>,
    output: Vec<OutputItem>,
    usage: Option<ResponseUsage>,
    status: ResponseStatus,
    incomplete_details: Option<IncompleteDetails>,
    error: Option<serde_json::Value>,
    /// Per-round active and completed items, keyed by validated output index.
    slots: SlotMap,
    strict_call_ids: HashMap<u32, CallIdObservation>,
    stream_lifecycle: StreamLifecycle,
}

impl ResponseAccumulator {
    /// Creates a response accumulator using lenient streaming validation.
    #[must_use]
    pub fn new(response_id: String, conversation_id: Option<String>) -> Self {
        Self::with_validation(response_id, conversation_id, Validation::Lenient)
    }

    pub(super) fn with_validation(
        response_id: String,
        conversation_id: Option<String>,
        validation: Validation,
    ) -> Self {
        Self {
            validation,
            response_id,
            conversation_id,
            output: Vec::new(),
            usage: None,
            status: ResponseStatus::InProgress,
            incomplete_details: None,
            error: None,
            slots: SlotMap::default(),
            strict_call_ids: HashMap::new(),
            stream_lifecycle: StreamLifecycle::AwaitingCreated,
        }
    }

    /// Parses a non-streaming JSON response body.
    ///
    /// # Errors
    /// Returns `ExecutorError::ParseError` if JSON parsing fails or required fields are missing.
    pub fn from_json(body: &str, conversation_id: Option<&str>) -> ExecutorResult<Self> {
        Self::read_json(body, conversation_id.map(str::to_owned), Validation::Lenient)
    }

    pub(super) fn load_json_body(&mut self, body: &str) -> ExecutorResult<()> {
        *self = Self::read_json(body, self.conversation_id.clone(), self.validation)?;
        Ok(())
    }

    fn read_json(body: &str, conversation_id: Option<String>, validation: Validation) -> ExecutorResult<Self> {
        let mut json: serde_json::Value = deserialize_from_str(body).map_err(ExecutorError::JsonError)?;
        if validation == Validation::Strict {
            json::ensure_strict_response(&json)?;
        }

        let response_id = json["id"]
            .as_str()
            .ok_or_else(|| ExecutorError::ParseError("missing 'id' field in response".into()))?
            .to_string();

        let output = deserialize_from_value_opt::<Vec<serde_json::Value>>(json["output"].take())
            .map(|items| {
                let mut out = Vec::with_capacity(items.len());
                out.extend(items.into_iter().filter_map(deserialize_from_value_opt::<OutputItem>));
                out
            })
            .unwrap_or_default();

        let status = json["status"]
            .as_str()
            .map_or(ResponseStatus::Completed, |s| s.parse().unwrap_or_default());

        let usage = deserialize_from_value_opt::<ResponseUsage>(json["usage"].take());
        let incomplete_details = deserialize_from_value_opt::<IncompleteDetails>(json["incomplete_details"].take());
        let error = (!json["error"].is_null()).then(|| json["error"].take());

        Ok(Self {
            validation,
            response_id,
            conversation_id,
            output,
            usage,
            status,
            incomplete_details,
            error,
            slots: SlotMap::default(),
            strict_call_ids: HashMap::new(),
            stream_lifecycle: StreamLifecycle::Terminal,
        })
    }

    /// Accumulates an async stream of raw SSE lines with parallel processing.
    ///
    /// The async task feeds raw SSE lines through a channel while a `spawn_blocking`
    /// worker handles JSON parsing on a blocking thread — keeping the tokio executor
    /// free between chunk arrivals.
    ///
    /// # Errors
    /// Returns [`ExecutorError::InvalidRequest`] when repeated authoritative
    /// output-item content conflicts, or [`ExecutorError::StreamError`] when
    /// the input stream or worker encounters an error.
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
            .map_err(|_| ExecutorError::StreamError("Worker thread panicked".into()))?
    }

    /// Worker function that processes SSE lines from the channel (runs on blocking thread).
    fn process_stream_chunks(rx: mpsc::Receiver<String>, conversation_id: Option<String>) -> ExecutorResult<Self> {
        let mut acc = Self::new(uuid7_str("resp_"), conversation_id);
        for line in rx {
            let _ = acc.process_line(SseLine::parse(&line))?;
        }
        acc.finish_stream();
        Ok(acc)
    }

    /// Processes pre-collected raw SSE lines synchronously.
    ///
    /// Useful when lines have already been buffered (e.g. replaying a recorded stream).
    /// Prefer [`from_stream`](Self::from_stream) for live async streams.
    /// Malformed data frames are skipped for compatibility.
    ///
    /// # Errors
    /// Returns [`ExecutorError::InvalidRequest`] when repeated authoritative
    /// output-item content conflicts with the previously accumulated value.
    pub fn from_sse_lines(
        lines: impl IntoIterator<Item = String>,
        conversation_id: Option<&str>,
    ) -> ExecutorResult<Self> {
        let mut acc = Self::new(uuid7_str("resp_"), conversation_id.map(str::to_string));
        for line in lines {
            let _ = acc.process_line(SseLine::parse(&line))?;
        }
        acc.finalize_all();
        Ok(acc)
    }

    /// Finalizes all streaming items in upstream `output_index` order.
    pub(crate) fn finalize_all(&mut self) {
        self.output.extend(self.slots.drain_output());
    }

    /// Normalize classified data once, then validate and fold under the fixed policy.
    pub(super) fn process_line(&mut self, line: ClassifiedSseLine) -> ExecutorResult<Option<EventFrame>> {
        let ClassifiedSseLine::Data(data) = line else {
            return Ok(None);
        };
        let Some(frame) = normalize_sse_data_checked(&data).map_err(|error| invalid_stream(error.to_string()))? else {
            if self.validation == Validation::Strict {
                return Err(invalid_stream("upstream stream contains a malformed data frame"));
            }
            return Ok(None);
        };
        let disposition = self.process_normalized_event(&frame)?;
        Ok(disposition.into_frame(frame))
    }

    fn process_normalized_event(&mut self, frame: &EventFrame) -> ExecutorResult<EventDisposition> {
        let validated = match self.validation {
            Validation::Strict => {
                let validated = validate_frame(frame).map_err(|error| invalid_stream(error.to_string()))?;
                self.validate_strict_transition(frame)?;
                Some(validated)
            }
            Validation::Lenient => None,
        };
        self.capture_terminal_details_if_needed(frame);
        self.process_event_checked(frame, validated.as_ref())
    }

    fn validate_strict_transition(&mut self, frame: &EventFrame) -> ExecutorResult<()> {
        let event_name = frame.wire.event_type.as_deref().unwrap_or("streaming event");
        if self.stream_lifecycle == StreamLifecycle::Terminal {
            return Err(invalid_stream(
                "upstream stream contains an event after its terminal event",
            ));
        }

        match (&frame.event_type, &frame.payload) {
            (SSEEventType::ResponseCreated, EventPayload::Response { .. }) => {
                if self.stream_lifecycle != StreamLifecycle::AwaitingCreated {
                    return Err(invalid_lifecycle(event_name));
                }
            }
            (SSEEventType::ResponseInProgress, EventPayload::Response { id, .. }) => {
                if self.stream_lifecycle != StreamLifecycle::Created || id != &self.response_id {
                    return Err(invalid_lifecycle_or_id(event_name));
                }
            }
            (
                SSEEventType::ResponseCompleted | SSEEventType::ResponseFailed | SSEEventType::ResponseIncomplete,
                EventPayload::Response { id, .. },
            ) => {
                if self.stream_lifecycle != StreamLifecycle::InProgress || id != &self.response_id {
                    return Err(invalid_lifecycle_or_id(event_name));
                }
                if self.slots.has_active() {
                    return Err(invalid_stream("upstream stream ended with unfinished output items"));
                }
                self.validate_terminal_output(frame, frame.event_type != SSEEventType::ResponseFailed)?;
            }
            _ => self.require_in_progress(event_name)?,
        }
        Ok(())
    }

    fn require_in_progress(&self, event_name: &str) -> ExecutorResult<()> {
        if self.stream_lifecycle == StreamLifecycle::InProgress {
            return Ok(());
        }
        Err(invalid_lifecycle(event_name))
    }

    fn validate_terminal_output(&mut self, frame: &EventFrame, enforce_call_id_stability: bool) -> ExecutorResult<()> {
        if enforce_call_id_stability {
            self.ensure_stable_strict_call_ids()?;
        }
        let response = frame
            .wire
            .rest
            .get("response")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| invalid_stream("terminal upstream response has no valid 'response'"))?;
        let Some(output) = response.get("output") else {
            return Ok(());
        };
        if output.is_null() {
            return Ok(());
        }
        let output = output
            .as_array()
            .ok_or_else(|| invalid_stream("terminal upstream response has no valid 'output'"))?;
        if output.len() != self.slots.len() {
            return Err(invalid_stream(
                "terminal upstream response output does not match completed item events",
            ));
        }

        let mut terminal_item_ids = HashSet::with_capacity(output.len());
        for (output_index, item) in output.iter().enumerate() {
            let output_index = u32::try_from(output_index)
                .map_err(|_| invalid_stream("terminal upstream response has too many output items"))?;
            let item = item
                .as_object()
                .ok_or_else(|| invalid_stream("terminal upstream response contains an invalid output item"))?;
            let (item_id, item_type) = output_item_identity(item, "terminal output item")
                .map_err(|error| invalid_stream(error.to_string()))?;
            if !terminal_item_ids.insert(item_id) {
                return Err(invalid_stream(format!(
                    "terminal upstream response repeats output item '{item_id}'"
                )));
            }
            let Some(completed) = self
                .slots
                .get(OutputIndex::new(output_index))
                .and_then(|slot| slot.state.done_item())
            else {
                return Err(invalid_stream(
                    "terminal upstream response output does not match completed item events",
                ));
            };
            if SSEItemType::try_from(completed) != Ok(item_type) {
                return Err(invalid_stream(
                    "terminal upstream response output does not match completed item events",
                ));
            }
            let mut canonical = serde_json::Value::Object(item.clone());
            if canonical
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_none_or(str::is_empty)
            {
                canonical["id"] = serde_json::Value::String(item_id.to_owned());
            }
            let terminal_item = serde_json::from_value(canonical).map_err(|error| {
                invalid_stream(format!("terminal upstream response output item is invalid: {error}"))
            })?;
            self.observe_strict_call_id(output_index, output_item_call_id(&terminal_item));
        }
        if enforce_call_id_stability {
            self.ensure_stable_strict_call_ids()?;
        }
        Ok(())
    }

    fn ensure_stable_strict_call_ids(&self) -> ExecutorResult<()> {
        if let Some(output_index) = self
            .strict_call_ids
            .iter()
            .filter_map(|(output_index, observation)| observation.changed.then_some(*output_index))
            .min()
        {
            return Err(invalid_stream(format!(
                "upstream stream changes 'call_id' for output[{output_index}]"
            )));
        }
        Ok(())
    }

    fn observe_strict_call_id(&mut self, output_index: u32, call_id: Option<&str>) {
        self.strict_call_ids.entry(output_index).or_default().observe(call_id);
    }

    pub(super) fn accumulated_function_call(&self, output_index: u32) -> Option<AccumulatedFunctionCall<'_>> {
        let slot = self.slots.get(OutputIndex::new(output_index))?;
        match &slot.state {
            SlotState::Active(ActiveItem::FunctionCall { item, arguments }) => Some(AccumulatedFunctionCall {
                item,
                output_index,
                arguments,
            }),
            SlotState::Done(OutputItem::FunctionCall(item)) => Some(AccumulatedFunctionCall {
                item,
                output_index,
                arguments: &item.arguments,
            }),
            _ => None,
        }
    }

    fn capture_terminal_details(&mut self, frame: &EventFrame) {
        let Some(response) = frame.wire.rest.get("response") else {
            return;
        };

        self.incomplete_details = response
            .get("incomplete_details")
            .cloned()
            .and_then(deserialize_from_value_opt::<IncompleteDetails>);
        self.error = response.get("error").filter(|error| !error.is_null()).cloned();
    }

    fn capture_terminal_details_if_needed(&mut self, frame: &EventFrame) {
        if matches!(
            frame.event_type,
            SSEEventType::ResponseCompleted | SSEEventType::ResponseFailed | SSEEventType::ResponseIncomplete
        ) {
            self.capture_terminal_details(frame);
        }
    }

    pub(crate) fn finish_strict_stream(&mut self) -> ExecutorResult<()> {
        if self.stream_lifecycle != StreamLifecycle::Terminal {
            return Err(ExecutorError::InvalidRequest(
                "upstream stream ended without a terminal event".to_owned(),
            ));
        }
        self.finish_stream();
        Ok(())
    }

    pub(crate) fn finish_stream(&mut self) {
        self.finalize_all();
        if self.status == ResponseStatus::InProgress {
            self.status = ResponseStatus::Completed;
        }
        self.stream_lifecycle = StreamLifecycle::Terminal;
    }

    /// Feeds typed test fixtures through the same policy-controlled transitions as ingestion.
    #[cfg(test)]
    fn process_event(&mut self, frame: &EventFrame) {
        let _ = self.process_normalized_event(frame);
    }

    fn process_event_checked(
        &mut self,
        frame: &EventFrame,
        validated: Option<&ValidatedFrame<'_>>,
    ) -> ExecutorResult<EventDisposition> {
        match (&frame.event_type, &frame.payload) {
            (SSEEventType::ResponseCreated, EventPayload::Response { id, .. }) if !id.is_empty() => {
                self.response_id.clone_from(id);
                self.stream_lifecycle = StreamLifecycle::Created;
            }
            (SSEEventType::ResponseInProgress, EventPayload::Response { .. }) => {
                self.stream_lifecycle = StreamLifecycle::InProgress;
            }
            (
                event_type @ (SSEEventType::ResponseCompleted
                | SSEEventType::ResponseFailed
                | SSEEventType::ResponseIncomplete),
                EventPayload::Response { usage, .. },
            ) => self.finish_response_event(*event_type, *usage),
            _ => {
                let Some(identity) = item_identity(frame, validated) else {
                    return Ok(EventDisposition::Emit(None));
                };
                let index = match frame.event_type {
                    SSEEventType::OutputItemAdded => self.slots.open(identity, &frame.payload, self.validation)?,
                    SSEEventType::OutputItemDone => self.slots.complete(
                        identity,
                        &frame.payload,
                        validated
                            .and_then(|frame| frame.item.as_ref())
                            .and_then(|item| item.done_item.as_ref()),
                        self.validation,
                    )?,
                    _ => self.slots.apply(identity, &frame.payload, self.validation)?,
                };
                if self.validation == Validation::Strict
                    && let Some(index) = index
                {
                    match &frame.payload {
                        EventPayload::OutputItemAdded { call_id, .. }
                        | EventPayload::FunctionCallArgsDelta { call_id, .. }
                        | EventPayload::FunctionCallArgsDone { call_id, .. } => {
                            self.observe_strict_call_id(index.get(), call_id.as_deref());
                        }
                        EventPayload::OutputItemDone { .. } => {
                            let call_id = self
                                .slots
                                .get(index)
                                .and_then(|slot| slot.state.done_item())
                                .and_then(output_item_call_id);
                            self.strict_call_ids.entry(index.get()).or_default().observe(call_id);
                        }
                        _ => {}
                    }
                }
                return Ok(EventDisposition::item(index));
            }
        }
        Ok(EventDisposition::Emit(None))
    }

    fn finish_response_event(&mut self, event_type: SSEEventType, usage: Option<ResponseUsage>) {
        let status = match event_type {
            SSEEventType::ResponseCompleted => ResponseStatus::Completed,
            SSEEventType::ResponseFailed => ResponseStatus::Error,
            SSEEventType::ResponseIncomplete => ResponseStatus::Incomplete,
            _ => return,
        };
        self.finish_response(status, usage);
    }

    fn finish_response(&mut self, status: ResponseStatus, usage: Option<ResponseUsage>) {
        self.finalize_all();
        self.status = status;
        self.usage = usage;
        self.stream_lifecycle = StreamLifecycle::Terminal;
    }

    /// Marks the response as incomplete due to an error or interruption.
    pub fn mark_incomplete(&mut self, reason: impl Into<String>) {
        self.status = ResponseStatus::Incomplete;
        self.incomplete_details = Some(IncompleteDetails {
            reason: Some(reason.into()),
        });
    }

    /// Applies the selected stream policy and consumes the assembled response.
    pub(super) fn finish(
        mut self,
        model: &str,
        previous_response_id: Option<&str>,
        instructions: Option<&str>,
    ) -> ExecutorResult<ResponsePayload> {
        match self.validation {
            Validation::Strict => self.finish_strict_stream()?,
            Validation::Lenient => self.finish_stream(),
        }
        Ok(self.finalize(model, previous_response_id, instructions))
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
            error: self.error,
            previous_response_id: previous_response_id.map(str::to_string),
            conversation_id: self.conversation_id,
            instructions: instructions.map(str::to_string),
            tools: None,
            tool_choice: None,
        }
    }
}

fn output_item_call_id(item: &OutputItem) -> Option<&str> {
    match item {
        OutputItem::FunctionCall(call) => Some(&call.call_id),
        OutputItem::ToolSearchCall(call) => Some(&call.call_id),
        OutputItem::CustomToolCall(call) => Some(&call.call_id),
        OutputItem::ShellCall(call) => Some(&call.call_id),
        _ => None,
    }
}

fn invalid_lifecycle(event_name: &str) -> ExecutorError {
    invalid_stream(format!(
        "upstream stream event '{event_name}' is out of lifecycle order"
    ))
}

fn invalid_lifecycle_or_id(event_name: &str) -> ExecutorError {
    invalid_stream(format!(
        "upstream stream event '{event_name}' is out of lifecycle order or changes the response id"
    ))
}

fn invalid_stream(message: impl Into<String>) -> ExecutorError {
    ExecutorError::InvalidRequest(message.into())
}

fn item_identity<'a>(frame: &'a EventFrame, validated: Option<&ValidatedFrame<'a>>) -> Option<ItemIdentity<'a>> {
    if let Some(item) = validated.and_then(|frame| frame.item.as_ref()) {
        return Some(ItemIdentity {
            index: Some(OutputIndex::new(item.output_index)),
            item_id: (!item.item_id.is_empty()).then_some(item.item_id),
            item_type: item.item_type,
        });
    }
    let (item_id, item_type) = match &frame.payload {
        EventPayload::OutputItemAdded { item_id, item_type, .. }
        | EventPayload::OutputItemDone { item_id, item_type, .. } => (item_id.as_str(), *item_type),
        payload => {
            let item_type = expected_item_type(frame.event_type)?;
            let item_id = match payload {
                EventPayload::TextDelta { item_id, .. }
                | EventPayload::TextDone { item_id, .. }
                | EventPayload::FunctionCallArgsDelta { item_id, .. }
                | EventPayload::FunctionCallArgsDone { item_id, .. }
                | EventPayload::CustomToolCallInputDelta { item_id, .. }
                | EventPayload::CustomToolCallInputDone { item_id, .. }
                | EventPayload::ReasoningTextDelta { item_id, .. }
                | EventPayload::ReasoningTextDone { item_id, .. }
                | EventPayload::ReasoningSummaryTextDelta { item_id, .. }
                | EventPayload::ReasoningSummaryTextDone { item_id, .. } => item_id.as_str(),
                _ => frame
                    .wire
                    .rest
                    .get("item_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
            };
            (item_id, item_type)
        }
    };
    Some(ItemIdentity {
        index: frame.output_index().map(OutputIndex::new),
        item_id: (!item_id.is_empty()).then_some(item_id),
        item_type,
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod identity_tests;

#[cfg(test)]
mod policy_tests;
