//! Collaboration items replayed as input accept omitted IDs; response items require IDs.
//! Agent messages also preserve the broader output content union for lossless replay.
use serde::{Deserialize, Serialize};

use crate::types::io::InputItem;

use super::{
    AgentAttribution, AgentMessage, AgentMessageContent, MultiAgentAction, MultiAgentCall, MultiAgentCallOutput,
    MultiAgentCallOutputContent,
};

/// Input form of [`MultiAgentCall`], with an optional item ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InputMultiAgentCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub call_id: String,
    pub action: MultiAgentAction,
    pub arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
}

impl From<MultiAgentCall> for InputMultiAgentCall {
    fn from(item: MultiAgentCall) -> Self {
        Self {
            id: Some(item.id),
            call_id: item.call_id,
            action: item.action,
            arguments: item.arguments,
            agent: item.agent,
        }
    }
}

/// Input form of [`MultiAgentCallOutput`], with an optional item ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InputMultiAgentCallOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub call_id: String,
    pub action: MultiAgentAction,
    pub output: Vec<MultiAgentCallOutputContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
}

impl From<MultiAgentCallOutput> for InputMultiAgentCallOutput {
    fn from(item: MultiAgentCallOutput) -> Self {
        Self {
            id: Some(item.id),
            call_id: item.call_id,
            action: item.action,
            output: item.output,
            agent: item.agent,
        }
    }
}

/// Input form of [`AgentMessage`], with an optional item ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InputAgentMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub author: String,
    pub recipient: String,
    pub content: Vec<AgentMessageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
}

impl From<AgentMessage> for InputAgentMessage {
    fn from(item: AgentMessage) -> Self {
        Self {
            id: Some(item.id),
            author: item.author,
            recipient: item.recipient,
            content: item.content,
            agent: item.agent,
        }
    }
}

impl InputItem {
    /// Public ownership retained in a client-supplied history item.
    pub(crate) fn agent(&self) -> Option<&AgentAttribution> {
        match self {
            Self::MultiAgentCall(item) => item.agent.as_ref(),
            Self::MultiAgentCallOutput(item) => item.agent.as_ref(),
            Self::AgentMessage(item) => item.agent.as_ref(),
            Self::Message(item) => item.agent.as_ref(),
            Self::FunctionCall(item) => item.agent.as_ref(),
            Self::CodeInterpreterCall(item) => item.agent.as_ref(),
            Self::ToolSearchCall(item) => item.agent.as_ref(),
            Self::ToolSearchOutput(item) => item.agent.as_ref(),
            Self::CustomToolCall(item) => item.agent.as_ref(),
            Self::ShellCall(item) => item.agent.as_ref(),
            Self::WebSearchCall(item) => item.agent.as_ref(),
            Self::McpCall(item) => item.agent.as_ref(),
            Self::McpListTools(item) => item.agent.as_ref(),
            Self::Reasoning(item) => item.agent.as_ref(),
            Self::Compaction(item) => item.agent.as_ref(),
            Self::FunctionCallOutput(_)
            | Self::CustomToolCallOutput(_)
            | Self::ShellCallOutput(_)
            | Self::CompactionTrigger => None,
        }
    }

    /// Remove public attribution when an item is routed into its agent's model context.
    pub(crate) fn take_agent(&mut self) -> Option<AgentAttribution> {
        match self {
            Self::MultiAgentCall(item) => item.agent.take(),
            Self::MultiAgentCallOutput(item) => item.agent.take(),
            Self::AgentMessage(item) => item.agent.take(),
            Self::Message(item) => item.agent.take(),
            Self::FunctionCall(item) => item.agent.take(),
            Self::CodeInterpreterCall(item) => item.agent.take(),
            Self::ToolSearchCall(item) => item.agent.take(),
            Self::ToolSearchOutput(item) => item.agent.take(),
            Self::CustomToolCall(item) => item.agent.take(),
            Self::ShellCall(item) => item.agent.take(),
            Self::WebSearchCall(item) => item.agent.take(),
            Self::McpCall(item) => item.agent.take(),
            Self::McpListTools(item) => item.agent.take(),
            Self::Reasoning(item) => item.agent.take(),
            Self::Compaction(item) => item.agent.take(),
            Self::FunctionCallOutput(_)
            | Self::CustomToolCallOutput(_)
            | Self::ShellCallOutput(_)
            | Self::CompactionTrigger => None,
        }
    }
}
