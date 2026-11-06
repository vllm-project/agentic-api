//! Conversion of stored output to continuation input and history bookkeeping.

use super::{OutputItem, ToolSearchExecution};
use crate::types::io::input::{InputFunctionToolCall, InputToolSearchCall};
use crate::types::io::{InOutItem, InputItem};

impl OutputItem {
    /// Shapes a stored output item as continuation input.
    /// Tool items keep their public types; `ResponsesInput::normalized_model_input` lowers them
    /// to function calls for the model. MCP list metadata is retained here and removed by
    /// `ResponsesInput::model_input`.
    #[must_use]
    pub fn to_input_item(&self) -> Option<InputItem> {
        match self {
            Self::Message(message) => Some(InputItem::Message(message.clone().into())),
            Self::Reasoning(reasoning) => Some(InputItem::Reasoning(reasoning.clone())),
            Self::FunctionCall(call) => Some(InputItem::FunctionCall(InputFunctionToolCall::from(call.clone()))),
            Self::ToolSearchCall(call) => InputToolSearchCall::try_from(call).ok().map(InputItem::ToolSearchCall),
            Self::ToolSearchOutput(output) => Some(InputItem::ToolSearchOutput(output.clone())),
            Self::CustomToolCall(call) => Some(InputItem::CustomToolCall(call.clone())),
            Self::ShellCall(call) => Some(InputItem::ShellCall(call.clone())),
            Self::WebSearchCall(call) => Some(InputItem::WebSearchCall(call.clone())),
            Self::McpCall(call) => Some(InputItem::McpCall(call.clone())),
            Self::CodeInterpreterCall(call) => Some(InputItem::CodeInterpreterCall(call.clone())),
            Self::McpListTools(list_tools) => Some(InputItem::McpListTools(list_tools.clone())),
            Self::Compaction(item) => Some(InputItem::Compaction(item.clone())),
            Self::MultiAgentCall(item) => Some(InputItem::MultiAgentCall(item.clone().into())),
            Self::MultiAgentCallOutput(item) => Some(InputItem::MultiAgentCallOutput(item.clone().into())),
            Self::AgentMessage(item) => Some(InputItem::AgentMessage(item.clone().into())),
            Self::FileSearchCall(_) | Self::Unknown { .. } => None,
        }
    }

    /// Whether this public item is one the executor records as the turn's
    /// history in round order (a hosted tool-search item), rather than as output.
    #[must_use]
    pub(crate) fn is_recorded_as_history(&self) -> bool {
        match self {
            Self::ToolSearchCall(call) => call.execution == ToolSearchExecution::Server,
            Self::ToolSearchOutput(_) => true,
            Self::Message(_)
            | Self::FunctionCall(_)
            | Self::CustomToolCall(_)
            | Self::ShellCall(_)
            | Self::CodeInterpreterCall(_)
            | Self::FileSearchCall(_)
            | Self::WebSearchCall(_)
            | Self::McpCall(_)
            | Self::McpListTools(_)
            | Self::Reasoning(_)
            | Self::Compaction(_)
            | Self::MultiAgentCall(_)
            | Self::MultiAgentCallOutput(_)
            | Self::AgentMessage(_)
            | Self::Unknown { .. } => false,
        }
    }

    /// Whether persistence must leave this output out because the turn's
    /// history items already hold it under the same id. A turn that did not
    /// record it (multi-agent rounds keep their own history) stores it as output.
    #[must_use]
    pub(crate) fn is_already_in_history(&self, history: &[InOutItem]) -> bool {
        self.is_recorded_as_history()
            && self
                .id()
                .is_some_and(|id| history.iter().any(|item| item.id() == Some(id)))
    }
}
