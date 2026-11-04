//! Message-thread contracts, independent of the Responses object model.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::Value;
use serde_json::value::RawValue;
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageThreadCheckpoint {
    pub messages: Vec<ThreadMessage>,
    pub create_only: BTreeMap<String, Box<RawValue>>,
    pub fixed: BTreeMap<String, Value>,
    pub betas: Vec<String>,
    pub paused: bool,
}

/// Start a conversation or append to the latest completed message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MessageThreadParam {
    Create,
    Continue { previous_message_id: String },
}

/// Lossless message retained by a thread. Content blocks are provider-owned;
/// their payloads must survive replay, including images and thinking signatures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadMessage {
    pub role: ThreadMessageRole,
    pub content: ThreadMessageContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadMessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThreadMessageContent {
    Text(String),
    Blocks(Vec<ThreadContentBlock>),
}

/// Typed block discriminator with opaque, lossless provider extension fields.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub fields: BTreeMap<String, Box<RawValue>>,
}

/// Thread request envelope. Inference parameters preserve provider fields
/// rather than round-tripping through the narrower tool view.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadMessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<ThreadMessage>,
    pub thread: MessageThreadParam,
    #[serde(flatten)]
    pub parameters: BTreeMap<String, Box<RawValue>>,
}

// serde's flatten/untagged buffering cannot deserialize RawValue directly.
// Capture extensions through a JSON tree internally, then keep them opaque in
// the public object model. This also works with from_value during checkpointing.
impl<'de> Deserialize<'de> for ThreadContentBlock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut fields = BTreeMap::<String, Value>::deserialize(deserializer)?;
        let kind = take_field::<_, D::Error>(&mut fields, "type")?;
        Ok(Self {
            kind,
            fields: opaque_fields::<D::Error>(fields)?,
        })
    }
}

impl<'de> Deserialize<'de> for ThreadMessagesRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut fields = BTreeMap::<String, Value>::deserialize(deserializer)?;
        Ok(Self {
            model: take_field::<_, D::Error>(&mut fields, "model")?,
            max_tokens: take_field::<_, D::Error>(&mut fields, "max_tokens")?,
            messages: take_field::<_, D::Error>(&mut fields, "messages")?,
            thread: take_field::<_, D::Error>(&mut fields, "thread")?,
            parameters: opaque_fields::<D::Error>(fields)?,
        })
    }
}

fn take_field<T: DeserializeOwned, E: serde::de::Error>(
    fields: &mut BTreeMap<String, Value>,
    key: &str,
) -> Result<T, E> {
    let value = fields
        .remove(key)
        .ok_or_else(|| E::custom(format!("missing field {key}")))?;
    serde_json::from_value(value).map_err(E::custom)
}

fn opaque_fields<E: serde::de::Error>(fields: BTreeMap<String, Value>) -> Result<BTreeMap<String, Box<RawValue>>, E> {
    fields
        .into_iter()
        .map(|(key, value)| {
            serde_json::value::to_raw_value(&value)
                .map(|value| (key, value))
                .map_err(E::custom)
        })
        .collect()
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
