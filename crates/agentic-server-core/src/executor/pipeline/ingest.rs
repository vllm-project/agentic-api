//! Synchronous semantic state for one inference round.

use crate::events::{ClassifiedSseLine, EventFrame, EventPayload, SSEItemType};
use crate::executor::accumulator::{ResponseAccumulator, Validation};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::executor::translate::{Translation, TranslationContext, TranslationDispatcher};
use crate::types::request_response::ResponsePayload;
#[derive(PartialEq, Eq)]
enum BodyKind {
    Empty,
    Stream,
    Json,
}

/// Owns validation, assembly, and translation for one body; performs no I/O.
pub(in crate::executor) struct RoundIngestion {
    accumulator: ResponseAccumulator,
    body_kind: BodyKind,
    translator: TranslationDispatcher,
}

impl RoundIngestion {
    pub(in crate::executor) fn new(
        response_id: String,
        conversation_id: Option<String>,
        validation: Validation,
        translation_context: TranslationContext,
        budget: Option<ExecutorResponseBudget>,
    ) -> Self {
        Self {
            body_kind: BodyKind::Empty,
            accumulator: ResponseAccumulator::with_validation_and_budget(
                response_id,
                conversation_id,
                validation,
                budget,
            ),
            translator: TranslationDispatcher::new(translation_context),
        }
    }

    /// Loads a complete response, preserving JSON status instead of applying SSE EOF policy.
    pub(in crate::executor) fn load_json_body(&mut self, body: &str) -> ExecutorResult<()> {
        if self.body_kind != BodyKind::Empty {
            return Err(ExecutorError::InvalidRequest(
                "cannot load a JSON body after starting stream ingestion".to_owned(),
            ));
        }
        self.translator.validate_json_body(body)?;
        self.accumulator.load_json_body(body)?;
        self.body_kind = BodyKind::Json;
        Ok(())
    }

    /// Ignored input emits no frames while retaining the current deferral boundary.
    pub(in crate::executor) fn push(&mut self, line: ClassifiedSseLine) -> ExecutorResult<Translation> {
        self.body_kind = BodyKind::Stream;
        match self.accumulator.normalize_line(line)? {
            Some(frame) => self.push_frame(frame),
            None => Ok(self.empty_translation()),
        }
    }

    /// Ingests an already normalized event without parsing an SSE data line.
    pub(in crate::executor) fn push_frame(&mut self, frame: EventFrame) -> ExecutorResult<Translation> {
        self.body_kind = BodyKind::Stream;
        Ok(
            Self::translate_frame(&mut self.accumulator, frame, &mut self.translator)?
                .unwrap_or_else(|| self.empty_translation()),
        )
    }

    pub(in crate::executor) fn translate_frame(
        accumulator: &mut ResponseAccumulator,
        frame: EventFrame,
        translator: &mut TranslationDispatcher,
    ) -> ExecutorResult<Option<Translation>> {
        let Some(frame) = accumulator.process_event_frame(frame)? else {
            return Ok(None);
        };
        let call = function_event_index(&frame.payload).and_then(|index| accumulator.accumulated_function_call(index));
        translator.translate(frame, call).map(Some)
    }

    #[cfg(test)]
    pub(in crate::executor) fn translate_line(
        accumulator: &mut ResponseAccumulator,
        line: ClassifiedSseLine,
        translator: &mut TranslationDispatcher,
    ) -> ExecutorResult<Option<Translation>> {
        let Some(frame) = accumulator.normalize_line(line)? else {
            return Ok(None);
        };
        Self::translate_frame(accumulator, frame, translator)
    }

    fn empty_translation(&self) -> Translation {
        Translation {
            frames: Vec::new(),
            defer_from_output_index: self.translator.defer_from_output_index(),
        }
    }

    /// Consumes the round, applying terminal policy and public tool-search projection once.
    pub(in crate::executor) fn finish(
        self,
        model: &str,
        previous_response_id: Option<&str>,
        instructions: Option<&str>,
    ) -> ExecutorResult<ResponsePayload> {
        let mut payload = match self.body_kind {
            BodyKind::Json => self.accumulator.finalize(model, previous_response_id, instructions),
            BodyKind::Empty | BodyKind::Stream => self.accumulator.finish(model, previous_response_id, instructions)?,
        };
        let outcome = self.translator.finish()?;
        outcome.normalize_response_payload(&mut payload)?;
        Ok(payload)
    }
}

fn function_event_index(payload: &EventPayload) -> Option<u32> {
    match payload {
        EventPayload::OutputItemAdded {
            item_type: SSEItemType::FunctionCall,
            output_index,
            ..
        }
        | EventPayload::OutputItemDone {
            item_type: SSEItemType::FunctionCall,
            output_index,
            ..
        }
        | EventPayload::FunctionCallArgsDelta { output_index, .. }
        | EventPayload::FunctionCallArgsDone { output_index, .. } => *output_index,
        _ => None,
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
