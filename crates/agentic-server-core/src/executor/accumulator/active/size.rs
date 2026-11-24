//! Retained-size accounting for streaming output items.

use super::ActiveItem;
use crate::executor::response_budget::{RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize};

/// Bytes an in-flight item retains: its typed item plus its streaming buffers.
/// Measures the same fields as the completed item so reconciliation at
/// completion charges only what streaming could not know in advance.
impl RetainedSize for ActiveItem {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Message(state) => state.retained_bytes(),
            Self::Reasoning(state) => state.retained_bytes(),
            Self::FunctionCall(state) => state.retained_bytes(),
            Self::CustomToolCall(state) => state.retained_bytes(),
            Self::ShellCall(state) => state.retained_bytes(),
            Self::CodeInterpreterCall { item } => item
                .as_ref()
                .map_or(RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize::retained_bytes),
            Self::FileSearchCall { item } => item
                .as_ref()
                .map_or(RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize::retained_bytes),
            Self::ToolSearchCall { item } => item.retained_bytes(),
            Self::WebSearchCall { item } => item
                .as_ref()
                .map_or(RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize::retained_bytes),
            Self::McpCall { item } => item.retained_bytes(),
            Self::McpListTools { item } => item.retained_bytes(),
            Self::Compaction { item } => item.retained_bytes(),
            Self::MultiAgentCall { item } => item.retained_bytes(),
            Self::MultiAgentCallOutput { item } => item.retained_bytes(),
            Self::AgentMessage(state) => state.retained_bytes(),
        }
    }
}
