//! Collaboration items replayed as input accept omitted IDs; response items require IDs.
//! Agent messages also preserve the broader output content union for lossless replay.
use serde::{Deserialize, Serialize};

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
