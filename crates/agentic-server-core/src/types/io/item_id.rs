//! Typed public item identities and their known type-specific prefixes.

use super::{InputItem, OutputItem};

impl InputItem {
    /// Returns the public ID carried by this input item, when present.
    #[must_use]
    pub(crate) fn id(&self) -> Option<&str> {
        match self {
            Self::Message(item) => item.id.as_deref(),
            Self::FunctionCall(item) => item.id.as_deref(),
            Self::ToolSearchCall(item) => Some(&item.id),
            Self::CustomToolCall(item) => Some(&item.id),
            Self::ShellCall(item) => item.id.as_deref(),
            Self::ShellCallOutput(item) => item.id.as_deref(),
            Self::Reasoning(item) => Some(&item.id),
            Self::McpListTools(item) => Some(&item.id),
            Self::Compaction(item) => item.id.as_deref(),
            Self::FunctionCallOutput(_)
            | Self::ToolSearchOutput(_)
            | Self::CustomToolCallOutput(_)
            | Self::CompactionTrigger
            | Self::Unknown => None,
        }
    }

    /// Returns the known public ID prefix, including its underscore separator.
    /// A missing rule does not impose a prefix on unverified or ID-less variants.
    #[must_use]
    pub(crate) fn id_prefix(&self) -> Option<&'static str> {
        match self {
            Self::Message(_) => Some("msg_"),
            Self::FunctionCall(_) => Some("fc_"),
            Self::ToolSearchCall(_) => Some("tsc_"),
            Self::CustomToolCall(_) => Some("ctc_"),
            Self::ShellCall(_) => Some("sh_"),
            Self::Reasoning(_) => Some("rs_"),
            Self::McpListTools(_) => Some("mcpl_"),
            Self::Compaction(_) => Some("cmp_"),
            Self::ShellCallOutput(_)
            | Self::FunctionCallOutput(_)
            | Self::ToolSearchOutput(_)
            | Self::CustomToolCallOutput(_)
            | Self::CompactionTrigger
            | Self::Unknown => None,
        }
    }
}

impl OutputItem {
    /// Returns the output item's wire ID, if the item has a known type.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Message(item) => Some(&item.id),
            Self::FunctionCall(item) => Some(&item.id),
            Self::ToolSearchCall(item) => Some(&item.id),
            Self::CustomToolCall(item) => Some(&item.id),
            Self::ShellCall(item) => item.id.as_deref(),
            Self::WebSearchCall(item) => Some(&item.id),
            Self::McpCall(item) => Some(&item.id),
            Self::McpListTools(item) => Some(&item.id),
            Self::Reasoning(item) => Some(&item.id),
            Self::Compaction(item) => item.id.as_deref(),
            Self::Unknown => None,
        }
    }

    /// Returns the known public ID prefix, including its underscore separator.
    /// A missing rule does not impose a prefix on unverified or ID-less variants.
    #[must_use]
    pub(crate) fn id_prefix(&self) -> Option<&'static str> {
        match self {
            Self::Message(_) => Some("msg_"),
            Self::FunctionCall(_) => Some("fc_"),
            Self::ToolSearchCall(_) => Some("tsc_"),
            Self::CustomToolCall(_) => Some("ctc_"),
            Self::ShellCall(_) => Some("sh_"),
            Self::WebSearchCall(_) => Some("ws_"),
            Self::McpCall(_) => Some("mcp_"),
            Self::McpListTools(_) => Some("mcpl_"),
            Self::Reasoning(_) => Some("rs_"),
            Self::Compaction(_) => Some("cmp_"),
            Self::Unknown => None,
        }
    }
}
