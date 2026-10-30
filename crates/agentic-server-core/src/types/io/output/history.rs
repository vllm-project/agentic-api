//! Hosted search output items that the executor stores in round history.

use super::{OutputItem, ToolSearchExecution};
use crate::types::io::InOutItem;

impl OutputItem {
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
