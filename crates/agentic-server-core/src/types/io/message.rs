//! Typed message data shared by input history and completed output.

use serde::{Deserialize, Serialize};

use super::input::{InputContent, InputMessageContent, InputTextContent};
use super::output::OutputTextContent;
use crate::events::EventPayload;
use crate::executor::error::ExecutorError;
use crate::types::event::MessageStatus;
use crate::utils::uuid7_str;

/// Provider-supplied assistant message phase. Never infer it from message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    /// An intermediate assistant update, including a preamble before tool calls.
    Commentary,
    /// The assistant's final answer, distinct from an intermediate update.
    FinalAnswer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InputMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<MessageStatus>,
    /// Preserved only when supplied for assistant history; absent for user messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<MessagePhase>,
    pub content: InputMessageContent,
}

impl InputMessage {
    /// Construct a new message without inventing upstream identity, status, or phase.
    #[must_use]
    pub fn new(role: impl Into<String>, content: InputMessageContent) -> Self {
        Self {
            id: None,
            role: role.into(),
            status: None,
            phase: None,
            content,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OutputMessage {
    pub id: String,
    pub role: String,
    pub status: MessageStatus,
    /// The authoritative completed item's phase, retained across continuation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<MessagePhase>,
    #[serde(default)]
    pub content: Vec<OutputTextContent>,
}

impl OutputMessage {
    pub fn new(id: impl Into<String>, status: MessageStatus) -> Self {
        Self {
            id: id.into(),
            role: "assistant".into(),
            status,
            phase: None,
            content: vec![],
        }
    }
}

impl TryFrom<&EventPayload> for OutputMessage {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        let EventPayload::OutputItemAdded { item_id, .. } = payload else {
            return Err(ExecutorError::ParseError("expected OutputItemAdded payload".into()));
        };
        let id = if item_id.is_empty() {
            uuid7_str("msg_")
        } else {
            item_id.clone()
        };
        Ok(Self::new(id, MessageStatus::InProgress))
    }
}

impl From<OutputMessage> for InputMessage {
    fn from(msg: OutputMessage) -> Self {
        let parts = msg
            .content
            .into_iter()
            .map(|c| InputContent::OutputText(InputTextContent::new(c.text)))
            .collect();
        Self {
            id: Some(msg.id),
            role: msg.role,
            status: Some(msg.status),
            phase: msg.phase,
            content: InputMessageContent::Parts(parts),
        }
    }
}
