use super::context::mark_async_call;
use super::custom::custom_frame;
use super::{CallExecution, ToolEvent, ToolTranslator};
use crate::events::{EventFrame, SSEEventType};
use crate::executor::accumulator::AccumulatedFunctionCall;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::event::MessageStatus;
use crate::types::io::{FunctionToolCall, OutputItem};
use serde_json::Value;

/// Function calls keep their upstream events with public item identities.
/// An async call's item events are built from the typed accumulated call with the shared async marker,
/// so the streamed item carries
/// `async: true` in the same form as the final output.
#[derive(Debug)]
pub(super) struct FunctionTranslator {
    execution: CallExecution,
}

impl FunctionTranslator {
    pub(super) const fn new(execution: CallExecution) -> Self {
        Self { execution }
    }
}

impl ToolTranslator for FunctionTranslator {
    fn translate(
        &mut self,
        event: ToolEvent<'_>,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Vec<EventFrame>> {
        let is_async = self.execution == CallExecution::Async;
        let mut frames = match event {
            ToolEvent::Added { .. } if is_async => vec![async_item_frame(SSEEventType::OutputItemAdded, call)?],
            ToolEvent::Done(_) if is_async => vec![async_item_frame(SSEEventType::OutputItemDone, call)?],
            ToolEvent::Added { original, .. } => original.into_iter().collect(),
            ToolEvent::Delta(frame) | ToolEvent::ArgumentsDone(frame) | ToolEvent::Done(frame) => vec![frame],
        };
        // This translator emits only public function calls (including namespace
        // members). Other tool translators own their projected item identities.
        for frame in &mut frames {
            if let Some(Value::String(id)) = frame.wire.rest.get_mut("item_id") {
                FunctionToolCall::normalize_public_id(id);
            }
            if let Some(Value::String(id)) = frame.wire.rest.get_mut("item").and_then(|item| item.get_mut("id")) {
                FunctionToolCall::normalize_public_id(id);
            }
        }
        Ok(frames)
    }
}

/// An async call's `output_item.added` (in progress, no arguments yet) or `output_item.done` event,
/// from the accumulated call, which carries the event's item ID and the completed arguments.
fn async_item_frame(event_type: SSEEventType, call: Option<AccumulatedFunctionCall<'_>>) -> ExecutorResult<EventFrame> {
    let call = call
        .ok_or_else(|| ExecutorError::StreamError("an async function call event has no accumulated call".to_owned()))?;
    let mut function = call.item.clone();
    if event_type == SSEEventType::OutputItemAdded {
        function.arguments.clear();
        function.status = MessageStatus::InProgress;
    } else if function.arguments.is_empty() {
        call.arguments().clone_into(&mut function.arguments);
    }
    let mut item = OutputItem::FunctionCall(function);
    mark_async_call(&mut item)?;
    custom_frame(
        event_type,
        call.output_index,
        [("item".to_owned(), serde_json::to_value(item)?)],
    )
}
