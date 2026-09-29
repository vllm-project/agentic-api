use super::{ResponseAccumulator, StreamLifecycle, Validation};
use crate::events::SSEEventType;
use crate::executor::error::ExecutorResult;
use crate::executor::response_budget::RETAINED_CONTAINER_OVERHEAD_BYTES;
use crate::types::event::ResponseStatus;
use crate::types::io::ResponseUsage;
use crate::types::request_response::{IncompleteDetails, ResponsePayload};

impl ResponseAccumulator {
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
        if let Some(service_tier) = &service_tier
            && let Some(budget) = &self.budget
        {
            budget.consume(RETAINED_CONTAINER_OVERHEAD_BYTES + service_tier.len())?;
        }
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
            conversation_id: self.conversation_id,
            instructions: instructions.map(str::to_string),
            service_tier: self.service_tier,
            tools: None,
            tool_choice: None,
        }
    }
}
