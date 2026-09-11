//! Synchronous conversion of validated function-call events to public tool shapes.
//! Orchestration supplies owned classification facts; translators own only stream state.

mod client;
mod context;
mod custom;
mod dispatcher;
mod function;
mod namespace;
mod shell;
mod tool_search;

pub(super) use context::TranslationContext;
pub(super) use dispatcher::TranslationDispatcher;

use crate::events::EventFrame;
use crate::executor::accumulator::AccumulatedFunctionCall;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::event::ResponseStatus;
use crate::types::io::OutputItem;
use std::collections::HashSet;

const MAX_PENDING_FUNCTION_BYTES: usize = 256 * 1024;

#[derive(Debug, Default)]
pub(super) struct Translation {
    pub(super) frames: Vec<EventFrame>,
    pub(super) defer_from_output_index: Option<u32>,
}

#[derive(Debug)]
pub(super) struct TranslationOutcome {
    context: TranslationContext,
    pub(super) unfinished_tool_search_item_ids: HashSet<String>,
}

impl TranslationOutcome {
    pub(super) fn normalize_response_payload(
        &self,
        payload: &mut crate::types::request_response::ResponsePayload,
    ) -> ExecutorResult<()> {
        self.normalize_response_output(&mut payload.output, payload.status.parse().unwrap_or_default())?;
        self.context.restore_response_metadata(payload);
        Ok(())
    }

    pub(super) fn normalize_response_output(
        &self,
        output: &mut Vec<OutputItem>,
        status: ResponseStatus,
    ) -> ExecutorResult<()> {
        self.context
            .normalize_response_output(output, status, &self.unfinished_tool_search_item_ids)
    }
}

/// Events whose item identity and accumulated arguments have already been validated.
enum ToolEvent<'a> {
    Added {
        item_id: &'a str,
        name: &'a str,
        output_index: u32,
        original: Option<EventFrame>,
    },
    Delta(EventFrame),
    ArgumentsDone(EventFrame),
    Done(EventFrame),
}

/// One translator per active call. Implementations own public-shape conversion state.
trait ToolTranslator: std::fmt::Debug + Send {
    fn translate(
        &mut self,
        event: ToolEvent<'_>,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Vec<EventFrame>>;

    fn unfinished_tool_search_item_id(&self) -> Option<&str> {
        None
    }
}

/// Associates a client tool handler with fresh per-call translation state.
/// Defined here so tool handlers do not depend on executor or SSE types.
trait HasTranslator: crate::tool::ToolHandler {
    type Translator: ToolTranslator;

    fn new_translator() -> Self::Translator;
}

fn ensure_function_call_size(arguments: &str) -> ExecutorResult<()> {
    if arguments.len() > MAX_PENDING_FUNCTION_BYTES {
        return Err(ExecutorError::StreamError(format!(
            "function-call SSE exceeded {MAX_PENDING_FUNCTION_BYTES} buffered bytes"
        )));
    }
    Ok(())
}

fn resolved_output_index(index: Option<u32>) -> ExecutorResult<u32> {
    index.ok_or_else(|| ExecutorError::InvalidRequest("translation requires a resolved output_index".to_owned()))
}

#[cfg(test)]
mod tests;
