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
use crate::executor::gateway_accumulator::synthetic_event_at;
use crate::tool::mcp::handler::started_list_tools_output_item;
use crate::types::event::MessageStatus;
use crate::types::io::output::{FunctionToolCall, McpListTools, OutputMessage, OutputTextContent};
use crate::types::io::{InputTextContent, OutputItem, OutputMessageContent};
use crate::utils::common::serialize_to_value;

/// The full public lifecycle of `item` at `output_index`. Each frame is built
/// only when the iterator reaches it.
pub(super) fn materialized_item_frames(
    item: &OutputItem,
    output_index: u64,
) -> impl Iterator<Item = ExecutorResult<EventFrame>> + '_ {
    let lifecycle = Lifecycle::of(item);
    lifecycle
        .steps()
        .map(move |step| step.frame(item, lifecycle, output_index))
}

/// `output_item.done` for `item` at `output_index`.
pub(super) fn item_done_frame(item: &OutputItem, output_index: u64) -> ExecutorResult<EventFrame> {
    synthetic_event_at(
        SSEEventType::OutputItemDone,
        output_index,
        [("item".to_owned(), to_value(item)?)],
    )
}

/// A status event in an MCP tool listing's public lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ListToolsStatus {
    InProgress,
    Completed,
    Failed,
}

impl ListToolsStatus {
    /// The status that ends `list`'s lifecycle: failed when the listing carries an error.
    pub(super) const fn finished(list: &McpListTools) -> Self {
        if list.error.is_some() {
            Self::Failed
        } else {
            Self::Completed
        }
    }

    pub(super) const fn event_type(self) -> SSEEventType {
        match self {
            Self::InProgress => SSEEventType::McpListToolsInProgress,
            Self::Completed => SSEEventType::McpListToolsCompleted,
            Self::Failed => SSEEventType::McpListToolsFailed,
        }
    }
}

/// `mcp_list_tools.in_progress`, `.completed` or `.failed` for the listing `list_id` at `output_index`.
pub(super) fn list_tools_status_frame(
    list_id: &str,
    output_index: u64,
    status: ListToolsStatus,
) -> ExecutorResult<EventFrame> {
    synthetic_event_at(status.event_type(), output_index, [string("item_id", list_id)])
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
            | OutputItem::ToolSearchOutput(_)
            | OutputItem::CustomToolCall(_)
            | OutputItem::ShellCall(_)
            | OutputItem::FileSearchCall(_)
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
                Some([Step::ListToolsInProgress(&list.id), Step::ListToolsFinished(list)]),
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
    /// Completed, or failed for a listing that carries an error.
    ListToolsFinished(&'a McpListTools),
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
    fn frame(self, item: &OutputItem, lifecycle: Lifecycle<'_>, output_index: u64) -> ExecutorResult<EventFrame> {
        let at_index = |event_type, fields| synthetic_event_at(event_type, output_index, fields);
        match self {
            Self::Added => at_index(
                SSEEventType::OutputItemAdded,
                vec![("item".to_owned(), lifecycle.started_item(item)?)],
            ),
            Self::ListToolsInProgress(item_id) => {
                list_tools_status_frame(item_id, output_index, ListToolsStatus::InProgress)
            }
            Self::ListToolsFinished(list) => {
                list_tools_status_frame(&list.id, output_index, ListToolsStatus::finished(list))
            }
            Self::PartAdded(at) => at_index(
                SSEEventType::ContentPartAdded,
                at.fields(("part".to_owned(), to_value(&started_part(at.part))?)),
            ),
            Self::TextDelta(at, text) => at_index(SSEEventType::OutputTextDelta, at.fields(string("delta", text))),
            Self::TextDone(at, text) => at_index(SSEEventType::OutputTextDone, at.fields(string("text", text))),
            Self::PartDone(at) => at_index(
                SSEEventType::ContentPartDone,
                at.fields(("part".to_owned(), to_value(at.part)?)),
            ),
            Self::ArgumentsDelta(call) => at_index(
                SSEEventType::FunctionCallArgumentsDelta,
                vec![string("item_id", &call.id), string("delta", &call.arguments)],
            ),
            Self::ArgumentsDone(call) => at_index(
                SSEEventType::FunctionCallArgumentsDone,
                vec![
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
    fn fields(self, value: (String, Value)) -> Vec<(String, Value)> {
        vec![
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

fn string(name: &str, value: &str) -> (String, Value) {
    (name.to_owned(), Value::String(value.to_owned()))
}

fn to_value<T: serde::Serialize>(value: &T) -> ExecutorResult<Value> {
    serialize_to_value(value).map_err(ExecutorError::JsonError)
}

#[cfg(test)]
#[path = "item_lifecycle_tests.rs"]
mod tests;
