//! Client tool call identity on input items: which call an item is, and which call it answers.

use super::InputItem;

impl InputItem {
    /// The `call_id` of a replayed client call item; `None` for every other item, including the
    /// outputs that answer calls (see [`Self::output_call_id`]).
    #[must_use]
    pub(crate) fn call_id(&self) -> Option<&str> {
        match self {
            Self::FunctionCall(call) => Some(&call.call_id),
            Self::ToolSearchCall(call) => Some(&call.call_id),
            Self::CustomToolCall(call) => Some(&call.call_id),
            Self::ShellCall(call) => Some(&call.call_id),
            Self::MultiAgentCall(_)
            | Self::MultiAgentCallOutput(_)
            | Self::AgentMessage(_)
            | Self::Message(_)
            | Self::CodeInterpreterCall(_)
            | Self::FunctionCallOutput(_)
            | Self::ToolSearchOutput(_)
            | Self::CustomToolCallOutput(_)
            | Self::ShellCallOutput(_)
            | Self::Reasoning(_)
            | Self::McpListTools(_)
            | Self::Compaction(_)
            | Self::CompactionTrigger
            | Self::Unknown => None,
        }
    }

    /// The `call_id` that a client tool output answers; `None` for every other item, including
    /// the calls themselves (see [`Self::call_id`]).
    #[must_use]
    pub(crate) fn output_call_id(&self) -> Option<&str> {
        match self {
            Self::FunctionCallOutput(output) => Some(&output.call_id),
            Self::ToolSearchOutput(output) => Some(&output.call_id),
            Self::CustomToolCallOutput(output) => Some(&output.call_id),
            Self::ShellCallOutput(output) => Some(&output.call_id),
            // Multi-agent outputs belong to the coordinator, not to a client call.
            Self::MultiAgentCall(_)
            | Self::MultiAgentCallOutput(_)
            | Self::AgentMessage(_)
            | Self::Message(_)
            | Self::FunctionCall(_)
            | Self::CodeInterpreterCall(_)
            | Self::ToolSearchCall(_)
            | Self::CustomToolCall(_)
            | Self::ShellCall(_)
            | Self::Reasoning(_)
            | Self::McpListTools(_)
            | Self::Compaction(_)
            | Self::CompactionTrigger
            | Self::Unknown => None,
        }
    }

    /// Whether the item is a call to an async tool. Only function and custom tools accept `async`.
    #[must_use]
    pub(crate) fn is_async_call(&self) -> bool {
        match self {
            Self::FunctionCall(call) => call.async_execution,
            Self::CustomToolCall(call) => call.async_execution,
            Self::MultiAgentCall(_)
            | Self::MultiAgentCallOutput(_)
            | Self::AgentMessage(_)
            | Self::Message(_)
            | Self::CodeInterpreterCall(_)
            | Self::FunctionCallOutput(_)
            | Self::ToolSearchCall(_)
            | Self::ToolSearchOutput(_)
            | Self::CustomToolCallOutput(_)
            | Self::ShellCall(_)
            | Self::ShellCallOutput(_)
            | Self::Reasoning(_)
            | Self::McpListTools(_)
            | Self::Compaction(_)
            | Self::CompactionTrigger
            | Self::Unknown => false,
        }
    }
}
