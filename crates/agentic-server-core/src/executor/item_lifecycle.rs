//! The public SSE lifecycle of a completed output item.
//!
//! A multi-agent coordinator creates some items itself, so no upstream frames
//! ever presented them. [`materialized_item_frames`] synthesizes their whole
//! lifecycle, from `output_item.added` to `output_item.done`; an item that was
//! already streamed needs only [`item_done_frame`]. Frames are built one at a time
//! from borrowed parts of the item. The caller attributes and presents them.
use std::iter::once;

use serde_json::Value;

use crate::events::{EventFrame, SSEEventType};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::synthetic_event;
use crate::tool::mcp::handler::started_list_tools_output_item;
use crate::types::event::MessageStatus;
use crate::types::io::output::{FunctionToolCall, McpListTools, OutputMessage, OutputTextContent};
use crate::types::io::{InputTextContent, OutputItem, OutputMessageContent};
use crate::utils::common::serialize_to_value;

/// The full public lifecycle of `item` at `output_index`. Each frame is built
/// only when the iterator reaches it.
pub(super) fn materialized_item_frames(
    item: &OutputItem,
    output_index: usize,
) -> impl Iterator<Item = ExecutorResult<EventFrame>> + '_ {
    let lifecycle = Lifecycle::of(item);
    lifecycle
        .steps()
        .map(move |step| step.frame(item, lifecycle, output_index))
}

/// `output_item.done` for `item` at `output_index`.
pub(super) fn item_done_frame(item: &OutputItem, output_index: usize) -> ExecutorResult<EventFrame> {
    synthetic_event(
        SSEEventType::OutputItemDone,
        [index_field(output_index), ("item".to_owned(), to_value(item)?)],
    )
}

/// What streams between `output_item.added` and `output_item.done`.
#[derive(Clone, Copy)]
enum Lifecycle<'a> {
    /// Nothing: the complete item is both added and done.
    AddedDone,
    Message(&'a OutputMessage),
    FunctionCall(&'a FunctionToolCall),
    ListTools(&'a McpListTools),
}

impl<'a> Lifecycle<'a> {
    fn of(item: &'a OutputItem) -> Self {
        match item {
            OutputItem::Message(message) => Self::Message(message),
            OutputItem::FunctionCall(call) => Self::FunctionCall(call),
            OutputItem::McpListTools(list) => Self::ListTools(list),
            OutputItem::MultiAgentCall(_)
            | OutputItem::MultiAgentCallOutput(_)
            | OutputItem::AgentMessage(_)
            | OutputItem::CodeInterpreterCall(_)
            | OutputItem::ToolSearchCall(_)
            | OutputItem::CustomToolCall(_)
            | OutputItem::ShellCall(_)
            | OutputItem::WebSearchCall(_)
            | OutputItem::McpCall(_)
            | OutputItem::Reasoning(_)
            | OutputItem::Compaction(_)
            | OutputItem::Unknown => Self::AddedDone,
        }
    }

    fn steps(self) -> impl Iterator<Item = Step<'a>> {
        let (pair, message) = match self {
            Self::AddedDone => (None, None),
            Self::Message(message) => (None, Some(message)),
            Self::FunctionCall(call) => (Some([Step::ArgumentsDelta(call), Step::ArgumentsDone(call)]), None),
            Self::ListTools(list) => (
                Some([Step::ListToolsInProgress(&list.id), Step::ListToolsCompleted(&list.id)]),
                None,
            ),
        };
        once(Step::Added)
            .chain(pair.into_iter().flatten())
            .chain(message.into_iter().flat_map(message_steps))
            .chain(once(Step::Done))
    }

    /// The item as `output_item.added` presents it: streamed fields empty and,
    /// for messages and function calls, still in progress.
    fn started_item(self, item: &OutputItem) -> ExecutorResult<Value> {
        match self {
            Self::AddedDone => to_value(item),
            Self::Message(message) => to_value(&OutputItem::Message(OutputMessage {
                agent: message.agent.clone(),
                id: message.id.clone(),
                role: message.role.clone(),
                phase: message.phase,
                status: MessageStatus::InProgress,
                content: Vec::new(),
            })),
            Self::FunctionCall(call) => to_value(&OutputItem::FunctionCall(FunctionToolCall {
                async_execution: call.async_execution,
                agent: call.agent.clone(),
                id: call.id.clone(),
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                namespace: call.namespace.clone(),
                arguments: String::new(),
                status: MessageStatus::InProgress,
            })),
            Self::ListTools(list) => to_value(&started_list_tools_output_item(list)),
        }
    }
}

/// One lifecycle event, borrowing what it presents.
#[derive(Clone, Copy)]
enum Step<'a> {
    Added,
    ListToolsInProgress(&'a str),
    ListToolsCompleted(&'a str),
    PartAdded(MessagePart<'a>),
    TextDelta(MessagePart<'a>, &'a str),
    TextDone(MessagePart<'a>, &'a str),
    PartDone(MessagePart<'a>),
    ArgumentsDelta(&'a FunctionToolCall),
    ArgumentsDone(&'a FunctionToolCall),
    Done,
}

#[derive(Clone, Copy)]
struct MessagePart<'a> {
    item_id: &'a str,
    content_index: usize,
    part: &'a OutputMessageContent,
}

fn message_steps(message: &OutputMessage) -> impl Iterator<Item = Step<'_>> {
    message.content.iter().enumerate().flat_map(|(content_index, part)| {
        let at = MessagePart {
            item_id: &message.id,
            content_index,
            part,
        };
        let text = match part {
            OutputMessageContent::OutputText(text) => Some(text.text.as_str()),
            OutputMessageContent::InputText(_) => None,
        };
        once(Step::PartAdded(at))
            .chain(
                text.into_iter()
                    .flat_map(move |text| [Step::TextDelta(at, text), Step::TextDone(at, text)]),
            )
            .chain(once(Step::PartDone(at)))
    })
}

impl Step<'_> {
    fn frame(self, item: &OutputItem, lifecycle: Lifecycle<'_>, output_index: usize) -> ExecutorResult<EventFrame> {
        let index = index_field(output_index);
        match self {
            Self::Added => synthetic_event(
                SSEEventType::OutputItemAdded,
                [index, ("item".to_owned(), lifecycle.started_item(item)?)],
            ),
            Self::ListToolsInProgress(item_id) => synthetic_event(
                SSEEventType::McpListToolsInProgress,
                [index, string("item_id", item_id)],
            ),
            Self::ListToolsCompleted(item_id) => {
                synthetic_event(SSEEventType::McpListToolsCompleted, [index, string("item_id", item_id)])
            }
            Self::PartAdded(at) => synthetic_event(
                SSEEventType::ContentPartAdded,
                at.fields(index, ("part".to_owned(), to_value(&started_part(at.part))?)),
            ),
            Self::TextDelta(at, text) => {
                synthetic_event(SSEEventType::OutputTextDelta, at.fields(index, string("delta", text)))
            }
            Self::TextDone(at, text) => {
                synthetic_event(SSEEventType::OutputTextDone, at.fields(index, string("text", text)))
            }
            Self::PartDone(at) => synthetic_event(
                SSEEventType::ContentPartDone,
                at.fields(index, ("part".to_owned(), to_value(at.part)?)),
            ),
            Self::ArgumentsDelta(call) => synthetic_event(
                SSEEventType::FunctionCallArgumentsDelta,
                [index, string("item_id", &call.id), string("delta", &call.arguments)],
            ),
            Self::ArgumentsDone(call) => synthetic_event(
                SSEEventType::FunctionCallArgumentsDone,
                [
                    index,
                    string("item_id", &call.id),
                    string("name", &call.name),
                    string("arguments", &call.arguments),
                ],
            ),
            Self::Done => item_done_frame(item, output_index),
        }
    }
}

impl MessagePart<'_> {
    fn fields(self, index: (String, Value), value: (String, Value)) -> [(String, Value); 4] {
        [
            index,
            string("item_id", self.item_id),
            ("content_index".to_owned(), Value::from(self.content_index)),
            value,
        ]
    }
}

/// A content part as `content_part.added` presents it, with its text still empty.
fn started_part(part: &OutputMessageContent) -> OutputMessageContent {
    match part {
        OutputMessageContent::InputText(text) => OutputMessageContent::InputText(InputTextContent {
            text: String::new(),
            extra: text.extra.clone(),
        }),
        OutputMessageContent::OutputText(text) => OutputMessageContent::OutputText(OutputTextContent {
            text: String::new(),
            annotations: text.annotations.clone(),
            logprobs: text.logprobs.clone(),
        }),
    }
}

fn index_field(output_index: usize) -> (String, Value) {
    ("output_index".to_owned(), Value::from(output_index))
}

fn string(name: &str, value: &str) -> (String, Value) {
    (name.to_owned(), Value::String(value.to_owned()))
}

fn to_value<T: serde::Serialize>(value: &T) -> ExecutorResult<Value> {
    serialize_to_value(value).map_err(ExecutorError::JsonError)
}

#[cfg(test)]
#[path = "item_lifecycle_tests.rs"]
mod tests;
