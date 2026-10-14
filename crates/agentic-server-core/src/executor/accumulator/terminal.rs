//! Terminal response validation, finalization, and safe steering cuts.

use std::collections::HashSet;

use super::identity::{invalid_stream, output_item_call_id};
use super::slot::OutputIndex;
use super::{ResponseAccumulator, StreamLifecycle, Validation};
use crate::events::{EventFrame, SSEEventType, SSEItemType, output_item_identity};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::response_budget::{RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize};
use crate::types::event::ResponseStatus;
use crate::types::io::ResponseUsage;
use crate::types::request_response::{IncompleteDetails, ResponsePayload};

impl ResponseAccumulator {
    /// Finalizes all streaming items in upstream `output_index` order.
    pub(crate) fn finalize_all(&mut self) -> ExecutorResult<()> {
        self.output
            .extend(self.slots.drain_output_with_budget(self.budget.as_ref())?);
        Ok(())
    }

    pub(super) fn validate_terminal_output(
        &mut self,
        frame: &EventFrame,
        enforce_call_id_stability: bool,
    ) -> ExecutorResult<()> {
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

    pub(crate) fn finish_strict_stream(&mut self) -> ExecutorResult<()> {
        if self.stream_lifecycle != StreamLifecycle::Terminal {
            return Err(ExecutorError::InvalidRequest(
                "upstream stream ended without a terminal event".to_owned(),
            ));
        }
        self.finish_stream()
    }

    pub(in crate::executor) fn at_steering_boundary(&self) -> bool {
        self.stream_lifecycle != StreamLifecycle::Terminal && self.slots.at_steering_boundary()
    }

    pub(in crate::executor) fn ensure_steering_settled(&self) -> ExecutorResult<()> {
        if self.stream_lifecycle != StreamLifecycle::Terminal
            && self.slots.len() != 0
            && !self.slots.at_steering_boundary()
        {
            return Err(invalid_stream("upstream stream ended before a safe steering boundary"));
        }
        Ok(())
    }

    /// Finish an intentional steering cut only after all observed items have
    /// authoritative completions. An ordinary terminal event wins the race.
    /// Unlike lenient EOF, this never promotes an unfinished item.
    pub(in crate::executor) fn finish_for_steering(&mut self) -> ExecutorResult<bool> {
        if !self.at_steering_boundary() {
            return Ok(false);
        }
        if self.validation == Validation::Strict {
            self.ensure_stable_strict_call_ids()?;
        }
        let details = IncompleteDetails {
            reason: Some("steered".to_owned()),
        };
        self.terminal_details_account
            .reconcile(self.budget.as_ref(), details.retained_bytes())?;
        self.status = ResponseStatus::Incomplete;
        self.incomplete_details = Some(details);
        self.stream_lifecycle = StreamLifecycle::Terminal;
        Ok(true)
    }

    pub(crate) fn finish_stream(&mut self) -> ExecutorResult<()> {
        self.finalize_all()?;
        if self.status == ResponseStatus::InProgress {
            self.status = ResponseStatus::Completed;
        }
        self.stream_lifecycle = StreamLifecycle::Terminal;
        Ok(())
    }

    pub(super) fn finish_response_event(
        &mut self,
        event_type: SSEEventType,
        usage: Option<ResponseUsage>,
        service_tier: Option<String>,
    ) -> ExecutorResult<()> {
        let status = match event_type {
            SSEEventType::ResponseCompleted => ResponseStatus::Completed,
            SSEEventType::ResponseFailed => ResponseStatus::Error,
            SSEEventType::ResponseIncomplete => ResponseStatus::Incomplete,
            _ => return Ok(()),
        };
        self.finish_response(status, usage, service_tier)
    }

    fn finish_response(
        &mut self,
        status: ResponseStatus,
        usage: Option<ResponseUsage>,
        service_tier: Option<String>,
    ) -> ExecutorResult<()> {
        self.finalize_all()?;
        self.service_tier_account.reconcile(
            self.budget.as_ref(),
            service_tier
                .as_ref()
                .map_or(0, |tier| RETAINED_CONTAINER_OVERHEAD_BYTES + tier.len()),
        )?;
        self.status = status;
        self.usage = usage;
        self.service_tier = service_tier;
        self.stream_lifecycle = StreamLifecycle::Terminal;
        Ok(())
    }

    /// Marks the response as incomplete due to an error or interruption.
    pub fn mark_incomplete(&mut self, reason: impl Into<String>) {
        self.status = ResponseStatus::Incomplete;
        self.incomplete_details = Some(IncompleteDetails {
            reason: Some(reason.into()),
        });
    }

    /// Applies the selected stream policy and consumes the assembled response.
    pub(in crate::executor) fn finish(
        mut self,
        model: &str,
        previous_response_id: Option<&str>,
        instructions: Option<&str>,
    ) -> ExecutorResult<ResponsePayload> {
        match self.validation {
            Validation::Strict => self.finish_strict_stream()?,
            Validation::Lenient => self.finish_stream()?,
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
            conversation: self.conversation_id,
            instructions: instructions.map(str::to_string),
            max_tool_calls: None,
            service_tier: self.service_tier,
            tools: None,
            tool_choice: None,
            standard_fields: crate::types::request_response::StandardResponseFields::default(),
        }
    }
}
