use super::{ToolEvent, ToolTranslator};
use crate::events::EventFrame;
use crate::executor::accumulator::AccumulatedFunctionCall;
use crate::executor::error::ExecutorResult;

#[derive(Debug)]
pub(super) struct FunctionTranslator;

impl ToolTranslator for FunctionTranslator {
    fn translate(
        &mut self,
        event: ToolEvent<'_>,
        _call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Vec<EventFrame>> {
        Ok(match event {
            ToolEvent::Added { original, .. } => original.into_iter().collect(),
            ToolEvent::Delta(frame) | ToolEvent::ArgumentsDone(frame) | ToolEvent::Done(frame) => vec![frame],
        })
    }
}
