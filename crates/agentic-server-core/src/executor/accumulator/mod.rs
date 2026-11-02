//! Response accumulation and parsing utilities.
//!
//! Handles both streaming (SSE) and non-streaming JSON response formats,
//! accumulating semantic events into a unified `ResponsePayload` structure.
//!
//! The executor processes SSE lines inline through `AgentPipeline` and its
//! round-scoped ingestion state. The separate [`ResponseAccumulator::from_stream`]
//! convenience method uses a channel and `spawn_blocking` worker; that worker is
//! not the executor's main streaming path.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::mpsc;

use futures::{Stream, StreamExt};

use crate::events::{
    ClassifiedSseLine, EventFrame, EventPayload, SSEEventType, SSEItemType, SseLine, ValidatedFrame,
    normalize_sse_data_checked, validate_frame,
};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::response_budget::{ExecutorResponseBudget, RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedAccount};
use crate::types::event::ResponseStatus;
use crate::types::io::{FunctionToolCall, OutputItem, ResponseUsage};
use crate::types::request_response::IncompleteDetails;
use crate::utils::uuid7_str;

mod active;
mod active_text;
mod identity;
use identity::{
    CallIdObservation, invalid_lifecycle, invalid_lifecycle_or_id, invalid_stream, item_identity, output_item_call_id,
};
mod completion;
mod details;
mod json;
mod slot;
mod terminal;

use active::ActiveItem;
use slot::{OutputIndex, SlotMap, SlotState};

/// Validation policy selected once for an accumulator's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Validation {
    Strict,
    Lenient,
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
    service_tier: Option<String>,
    service_tier_account: RetainedAccount,
    status: ResponseStatus,
    incomplete_details: Option<IncompleteDetails>,
    error: Option<serde_json::Value>,
    terminal_details_account: RetainedAccount,
    /// Per-round active and completed items, keyed by validated output index.
    slots: SlotMap,
    strict_call_ids: HashMap<u32, CallIdObservation>,
    stream_lifecycle: StreamLifecycle,
    pub(super) budget: Option<ExecutorResponseBudget>,
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
            service_tier: None,
            service_tier_account: RetainedAccount::default(),
            status: ResponseStatus::InProgress,
            incomplete_details: None,
            error: None,
            terminal_details_account: RetainedAccount::default(),
            slots: SlotMap::default(),
            strict_call_ids: HashMap::new(),
            stream_lifecycle: StreamLifecycle::AwaitingCreated,
            budget: None,
        }
    }

    pub(super) fn with_validation_and_budget(
        response_id: String,
        conversation_id: Option<String>,
        validation: Validation,
        budget: Option<ExecutorResponseBudget>,
    ) -> Self {
        let mut acc = Self::with_validation(response_id, conversation_id, validation);
        acc.budget = budget;
        acc
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
        acc.finish_stream()?;
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
        acc.finalize_all()?;
        Ok(acc)
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
        let Some(mut frame) = disposition.into_frame(frame) else {
            return Ok(None);
        };
        if let EventPayload::OutputItemDone {
            item_id,
            item_type: SSEItemType::CodeInterpreterCall,
            output_index: Some(index),
            ..
        } = &frame.payload
            && item_id.is_empty()
            && let Some(OutputItem::CodeInterpreterCall(call)) = self
                .slots
                .get(OutputIndex::new(*index))
                .and_then(|slot| slot.state.done_item())
        {
            frame.set_done_item_id(&call.id);
        }
        Ok(Some(frame))
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
        self.capture_terminal_details_if_needed(frame)?;
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

    fn observe_strict_call_id(&mut self, output_index: u32, call_id: Option<&str>) {
        self.strict_call_ids.entry(output_index).or_default().observe(call_id);
    }

    pub(super) fn accumulated_item_type(&self, output_index: u32) -> Option<SSEItemType> {
        self.slots.get(OutputIndex::new(output_index))?.state.item_type()
    }

    pub(super) fn accumulated_function_call(&self, output_index: u32) -> Option<AccumulatedFunctionCall<'_>> {
        let slot = self.slots.get(OutputIndex::new(output_index))?;
        match &slot.state {
            SlotState::Active(ActiveItem::FunctionCall(state)) => Some(AccumulatedFunctionCall {
                item: &state.item,
                output_index,
                arguments: &state.arguments,
            }),
            SlotState::Done(OutputItem::FunctionCall(item)) => Some(AccumulatedFunctionCall {
                item,
                output_index,
                arguments: &item.arguments,
            }),
            _ => None,
        }
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
                if let Some(budget) = &self.budget {
                    budget.consume(RETAINED_CONTAINER_OVERHEAD_BYTES + id.len())?;
                }
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
                EventPayload::Response {
                    usage, service_tier, ..
                },
            ) => self.finish_response_event(*event_type, *usage, service_tier.clone())?,
            _ => {
                let Some(identity) = item_identity(frame, validated) else {
                    return Ok(EventDisposition::Emit(None));
                };
                let index = match frame.event_type {
                    SSEEventType::OutputItemAdded => {
                        self.slots
                            .open(identity, &frame.payload, self.validation, self.budget.as_ref())?
                    }
                    SSEEventType::OutputItemDone => self.slots.complete(
                        identity,
                        &frame.payload,
                        validated
                            .and_then(|frame| frame.item.as_ref())
                            .and_then(|item| item.done_item.as_ref()),
                        self.validation,
                        self.budget.as_ref(),
                    )?,
                    _ => self
                        .slots
                        .apply(identity, &frame.payload, self.validation, self.budget.as_ref())?,
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
}

#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod identity_tests;

#[cfg(test)]
mod policy_tests;
