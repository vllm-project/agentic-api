//! Conversion of stored output to continuation input and history bookkeeping.

use super::OutputItem;
use crate::types::io::InputItem;
use crate::types::io::input::{InputFunctionToolCall, InputToolSearchCall};

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
}
