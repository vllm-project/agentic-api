//! Message-thread contracts, independent of the Responses object model.

use std::collections::BTreeSet;

use super::parameters::{
    CacheControl, Container, ContainerSkill, ContextManagement, McpServerFingerprint, ThinkingConfig,
};
use super::request::{MessageParam, MessagesRequest, OutputConfig, SystemPrompt, ToolParam};
use super::tool_choice::MessagesToolChoice;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Required beta for Messages thread requests.
pub const MESSAGE_THREADS_BETA: &str = "message-threads-2026-08-12";

/// Claude Code requests opt into the gateway's thread field compatibility rules.
pub const CLAUDE_CODE_BETA: &str = "claude-code-20250219";

/// Stable, opaque ownership scope shared by gateway processes and restarts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessagesThreadOwner(String);

impl MessagesThreadOwner {
    #[must_use]
    pub fn from_components(components: &[&[u8]]) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"agentic-messages-thread-owner-v1");
        for component in components {
            hash.update(component.len().to_string());
            hash.update([0]);
            hash.update(component);
        }
        Self(format!("{:x}", hash.finalize()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Persisted Messages-native conversation, separate from Responses items.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageThreadCheckpoint {
    pub messages: Vec<MessageParam>,
    pub system: Option<SystemPrompt>,
    pub tools: Option<Vec<ToolParam>>,
    pub fingerprint: ThreadFingerprint,
    pub betas: BTreeSet<String>,
    pub paused: bool,
}

/// Start a conversation or append to the latest completed message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MessageThreadParam {
    Create,
    Continue { previous_message_id: String },
}

/// Fixed thread parameters, excluding per-request credentials and container identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadFingerprint {
    pub model: String,
    pub tool_choice: Option<MessagesToolChoice>,
    pub thinking: Option<ThinkingConfig>,
    pub mcp_servers: Option<Vec<McpServerFingerprint>>,
    pub context_management: Option<ContextManagement>,
    pub output_config: Option<OutputConfig>,
    pub container_skills: Option<Vec<ContainerSkill>>,
    pub cache_control: Option<CacheControl>,
    pub fallbacks: bool,
}

impl From<&MessagesRequest> for ThreadFingerprint {
    fn from(request: &MessagesRequest) -> Self {
        Self {
            model: request.model.clone(),
            tool_choice: request.tool_choice.clone(),
            thinking: request.thinking.clone(),
            mcp_servers: request
                .mcp_servers
                .as_ref()
                .map(|servers| servers.iter().map(|server| server.configuration.clone()).collect()),
            context_management: request.context_management.clone(),
            output_config: request.output_config.clone(),
            container_skills: match &request.container {
                Some(Container::Configuration(container)) => container.skills.clone(),
                Some(Container::Id(_)) | None => None,
            },
            cache_control: request.cache_control.clone(),
            fallbacks: !request.fallbacks.is_missing(),
        }
    }
}

impl ThreadFingerprint {
    /// Field names remain protocol spellings for actionable mismatch errors.
    pub(crate) fn differing_fields(&self, previous: &Self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.model != previous.model {
            fields.push("model");
        }
        if self.tool_choice != previous.tool_choice {
            fields.push("tool_choice");
        }
        if self.thinking != previous.thinking {
            fields.push("thinking");
        }
        if self.mcp_servers != previous.mcp_servers {
            fields.push("mcp_servers");
        }
        if self.context_management != previous.context_management {
            fields.push("context_management");
        }
        if self.output_config != previous.output_config {
            fields.push("output_config");
        }
        if self.container_skills != previous.container_skills {
            fields.push("container.skills");
        }
        if self.cache_control != previous.cache_control {
            fields.push("cache_control");
        }
        if self.fallbacks != previous.fallbacks {
            fields.push("fallbacks");
        }
        fields
    }
}

/// Anthropic-compatible thread error details, used by SDK replay logic.
#[derive(Debug, Serialize)]
pub struct MessageThreadErrorBody {
    #[serde(rename = "type")]
    pub kind: MessageThreadErrorKind,
    pub error: MessageThreadErrorInfo,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageThreadErrorKind {
    Error,
}

#[derive(Debug, Serialize)]
pub struct MessageThreadErrorInfo {
    #[serde(rename = "type")]
    pub kind: MessageThreadErrorType,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<MessageThreadErrorDetails>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageThreadErrorType {
    InvalidRequestError,
    NotFoundError,
    ApiError,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageThreadErrorCode {
    ThreadNotFound,
    ThreadFingerprintMismatch,
    ThreadAlreadyContinued,
    ThreadContinueInFlight,
    ThreadUnsupportedRequest,
}

#[derive(Debug, Serialize)]
pub struct MessageThreadErrorDetails {
    pub error_code: MessageThreadErrorCode,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unsupported: Vec<String>,
}
