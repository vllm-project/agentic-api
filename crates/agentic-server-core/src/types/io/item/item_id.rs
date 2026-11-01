//! Typed public item identities and their known type-specific prefixes.

use crate::types::io::{FunctionToolCall, InOutItem, InputItem, OutputItem, OutputMessage};

impl OutputMessage {
    /// Make a nonempty provider message ID valid for public history reuse.
    /// Empty-ID allocation remains owned by ingestion; upstream lookup keys
    /// are retained separately from the public response projection.
    pub(crate) fn normalize_public_id(id: &mut String) {
        if !id.is_empty() && !id.starts_with("msg_") {
            id.insert_str(0, "msg_");
        }
    }
}

impl FunctionToolCall {
    /// Normalize the public item identity without changing its execution call ID.
    /// Private calls lowered to another public tool shape keep their original identity.
    pub(crate) fn normalize_public_id(id: &mut String) {
        if !id.is_empty() && !id.starts_with("fc_") {
            id.insert_str(0, "fc_");
        }
    }
}

impl InputItem {
    /// Returns the public ID carried by this input item, when present.
    #[must_use]
    pub(crate) fn id(&self) -> Option<&str> {
        match self {
            Self::MultiAgentCall(item) => item.id.as_deref(),
            Self::MultiAgentCallOutput(item) => item.id.as_deref(),
            Self::AgentMessage(item) => item.id.as_deref(),
            Self::Message(item) => item.id.as_deref(),
            Self::FunctionCall(item) => item.id.as_deref(),
            Self::CodeInterpreterCall(item) => Some(&item.id),
            Self::ToolSearchCall(item) => Some(&item.id),
            Self::ToolSearchOutput(item) => item.id.as_deref(),
            Self::CustomToolCall(item) => Some(&item.id),
            Self::ShellCall(item) => item.id.as_deref(),
            Self::ShellCallOutput(item) => item.id.as_deref(),
            Self::Reasoning(item) => Some(&item.id),
            Self::McpListTools(item) => Some(&item.id),
            Self::Compaction(item) => item.id.as_deref(),
            Self::WebSearchCall(item) => Some(&item.id),
            Self::McpCall(item) => Some(&item.id),
            Self::CustomToolCallOutput(item) => item.id.as_deref(),
            Self::FunctionCallOutput(item) => item.id.as_deref(),
            Self::CompactionTrigger => None,
        }
    }

    /// Returns the known public ID prefix, including its underscore separator.
    /// A missing rule does not impose a prefix on unverified or ID-less variants.
    #[must_use]
    pub(crate) fn id_prefix(&self) -> Option<&'static str> {
        match self {
            Self::MultiAgentCall(_) => Some("mac_"),
            Self::MultiAgentCallOutput(_) => Some("maco_"),
            Self::AgentMessage(_) => Some("amsg_"),
            Self::Message(_) => Some("msg_"),
            Self::FunctionCall(_) | Self::FunctionCallOutput(_) => Some("fc_"),
            Self::CodeInterpreterCall(_) => Some("ci_"),
            Self::ToolSearchCall(_) => Some("tsc_"),
            Self::ToolSearchOutput(_) => Some("tso_"),
            Self::CustomToolCall(_) => Some("ctc_"),
            Self::ShellCall(_) => Some("sh_"),
            Self::Reasoning(_) => Some("rs_"),
            Self::McpListTools(_) => Some("mcpl_"),
            Self::Compaction(_) => Some("cmp_"),
            Self::WebSearchCall(_) => Some("ws_"),
            Self::McpCall(_) => Some("mcp_"),
            Self::ShellCallOutput(_) => Some("sho_"),
            Self::CustomToolCallOutput(_) => Some("ctco_"),
            Self::CompactionTrigger => None,
        }
    }
}

impl OutputItem {
    /// Returns the output item's wire ID, if the item has a known type.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::MultiAgentCall(item) => Some(&item.id),
            Self::MultiAgentCallOutput(item) => Some(&item.id),
            Self::AgentMessage(item) => Some(&item.id),
            Self::Message(item) => Some(&item.id),
            Self::FunctionCall(item) => Some(&item.id),
            Self::CodeInterpreterCall(item) => Some(&item.id),
            Self::ToolSearchCall(item) => Some(&item.id),
            Self::ToolSearchOutput(item) => item.id.as_deref(),
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
            Self::MultiAgentCall(_) => Some("mac_"),
            Self::MultiAgentCallOutput(_) => Some("maco_"),
            Self::AgentMessage(_) => Some("amsg_"),
            Self::Message(_) => Some("msg_"),
            Self::FunctionCall(_) => Some("fc_"),
            Self::CodeInterpreterCall(_) => Some("ci_"),
            Self::ToolSearchCall(_) => Some("tsc_"),
            Self::ToolSearchOutput(_) => Some("tso_"),
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

impl InOutItem {
    /// The item's public identity, when supplied or assigned by storage.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Input(item) => item.id(),
            Self::Output(item) => item.id(),
        }
    }

    /// Assign the public resource identity without changing the item kind or payload.
    pub(crate) fn set_id(&mut self, id: String) {
        match self {
            Self::Input(item) => match item {
                InputItem::MultiAgentCall(item) => item.id = Some(id),
                InputItem::MultiAgentCallOutput(item) => item.id = Some(id),
                InputItem::AgentMessage(item) => item.id = Some(id),
                InputItem::Message(item) => item.id = Some(id),
                InputItem::FunctionCall(item) => item.id = Some(id),
                InputItem::ShellCall(item) => item.id = Some(id),
                InputItem::ShellCallOutput(item) => item.id = Some(id),
                InputItem::Compaction(item) => item.id = Some(id),
                InputItem::ToolSearchOutput(item) => item.id = Some(id),
                InputItem::CustomToolCallOutput(item) => item.id = Some(id),
                InputItem::CodeInterpreterCall(item) => item.id = id,
                InputItem::ToolSearchCall(item) => item.id = id,
                InputItem::CustomToolCall(item) => item.id = id,
                InputItem::Reasoning(item) => item.id = id,
                InputItem::McpListTools(item) => item.id = id,
                InputItem::WebSearchCall(item) => item.id = id,
                InputItem::McpCall(item) => item.id = id,
                InputItem::FunctionCallOutput(item) => item.id = Some(id),
                InputItem::CompactionTrigger => {}
            },
            Self::Output(item) => match item {
                OutputItem::MultiAgentCall(item) => item.id = id,
                OutputItem::MultiAgentCallOutput(item) => item.id = id,
                OutputItem::AgentMessage(item) => item.id = id,
                OutputItem::Message(item) => item.id = id,
                OutputItem::FunctionCall(item) => item.id = id,
                OutputItem::CodeInterpreterCall(item) => item.id = id,
                OutputItem::ToolSearchCall(item) => item.id = id,
                OutputItem::ToolSearchOutput(item) => item.id = Some(id),
                OutputItem::CustomToolCall(item) => item.id = id,
                OutputItem::WebSearchCall(item) => item.id = id,
                OutputItem::McpCall(item) => item.id = id,
                OutputItem::McpListTools(item) => item.id = id,
                OutputItem::Reasoning(item) => item.id = id,
                OutputItem::ShellCall(item) => item.id = Some(id),
                OutputItem::Compaction(item) => item.id = Some(id),
                OutputItem::Unknown => {}
            },
        }
    }
}
