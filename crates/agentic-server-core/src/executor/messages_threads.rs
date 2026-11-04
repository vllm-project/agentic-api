//! Messages-native thread orchestration over independent durable storage.

mod claude_code;
#[cfg(test)]
mod tests;
mod validation;

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;

use super::{ExecutorError, ExecutorResult, MessagesRequestContext};
use crate::storage::messages::{MessageThreadLease, MessageThreadStore, MessageThreadStoreError};
use crate::storage::{DbPool, StorageError};
use crate::types::messages::thread::{
    MessageThreadCheckpoint, MessageThreadErrorBody, MessageThreadErrorCode, MessageThreadErrorDetails,
    MessageThreadErrorInfo, MessageThreadErrorKind, MessageThreadErrorType, MessageThreadParam, MessagesThreadOwner,
    ThreadMessage, ThreadMessageContent, ThreadMessageRole, ThreadMessagesRequest,
};
use validation::{fingerprint, validate_request};

pub(super) const MAX_CHECKPOINT_BYTES: usize = crate::storage::messages::MAX_CHECKPOINT_BYTES;

/// Thread protocol or persistence failure, separate from inference failures.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct MessagesThreadError {
    message: String,
    code: Option<MessageThreadErrorCode>,
    unsupported: Vec<String>,
    #[source]
    source: Option<MessageThreadStoreError>,
}

impl MessagesThreadError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            unsupported: Vec::new(),
            source: None,
        }
    }

    fn coded(code: MessageThreadErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            ..Self::invalid(message)
        }
    }

    pub(crate) fn unsupported(field: &str) -> Self {
        Self {
            unsupported: vec![field.to_owned()],
            ..Self::coded(
                MessageThreadErrorCode::ThreadUnsupportedRequest,
                format!("thread does not support {field}"),
            )
        }
    }

    #[must_use]
    pub fn http_status(&self) -> http::StatusCode {
        if self.source.is_some() {
            http::StatusCode::INTERNAL_SERVER_ERROR
        } else if matches!(self.code, Some(MessageThreadErrorCode::ThreadNotFound)) {
            http::StatusCode::NOT_FOUND
        } else {
            http::StatusCode::BAD_REQUEST
        }
    }

    #[must_use]
    pub fn into_body(self) -> MessageThreadErrorBody {
        let kind = match self.http_status() {
            http::StatusCode::NOT_FOUND => MessageThreadErrorType::NotFoundError,
            http::StatusCode::INTERNAL_SERVER_ERROR => MessageThreadErrorType::ApiError,
            _ => MessageThreadErrorType::InvalidRequestError,
        };
        MessageThreadErrorBody {
            kind: MessageThreadErrorKind::Error,
            error: MessageThreadErrorInfo {
                kind,
                message: self.message,
                details: self.code.map(|error_code| MessageThreadErrorDetails {
                    error_code,
                    unsupported: self.unsupported,
                }),
            },
        }
    }
}

impl From<MessageThreadStoreError> for MessagesThreadError {
    fn from(error: MessageThreadStoreError) -> Self {
        match error {
            MessageThreadStoreError::NotFound => Self::coded(
                MessageThreadErrorCode::ThreadNotFound,
                "message thread not found; replay the full conversation",
            ),
            MessageThreadStoreError::AlreadyContinued => Self::coded(
                MessageThreadErrorCode::ThreadAlreadyContinued,
                "message has already been continued",
            ),
            MessageThreadStoreError::InFlight => Self::coded(
                MessageThreadErrorCode::ThreadContinueInFlight,
                "message thread continuation is in flight",
            ),
            error => Self {
                source: Some(error),
                ..Self::invalid("message thread storage operation failed")
            },
        }
    }
}

/// Protocol service backed by SQLite/Postgres. Default is disabled for stateless callers.
#[derive(Clone, Debug, Default)]
pub struct MessagesThreadStore {
    storage: MessageThreadStore,
}

impl MessagesThreadStore {
    #[must_use]
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self {
            storage: MessageThreadStore::new(pool),
        }
    }

    /// Renew active leases and expire old records, without keeping an in-memory history cache.
    ///
    /// # Errors
    /// Returns a storage failure; an expired lease cannot publish a stale completion.
    pub async fn maintain(&self) -> Result<(), MessagesThreadError> {
        Ok(self.storage.maintain().await?)
    }

    pub async fn shutdown(&self) {
        self.storage.shutdown().await;
    }

    /// Verify that the dedicated persistence schema is available.
    ///
    /// # Errors
    /// Returns a storage error when Messages persistence is unavailable.
    pub async fn verify_ready(&self) -> Result<(), MessagesThreadError> {
        Ok(self.storage.verify_ready().await?)
    }

    #[must_use]
    pub fn owner(&self, components: &[&[u8]]) -> MessagesThreadOwner {
        MessagesThreadOwner::from_components(components)
    }

    /// Validate and rehydrate a thread. Ordinary requests return `None`.
    ///
    /// # Errors
    /// Returns typed protocol, storage, or cross-instance reservation errors before inference.
    pub async fn prepare(
        &self,
        body: &[u8],
        owner: MessagesThreadOwner,
        mut betas: Vec<String>,
    ) -> Result<Option<PreparedMessagesThread>, MessagesThreadError> {
        #[derive(Deserialize)]
        struct Probe {
            thread: Option<Box<RawValue>>,
        }
        if !serde_json::from_slice::<Probe>(body).is_ok_and(|probe| probe.thread.is_some()) {
            return Ok(None);
        }
        if !self.storage.is_configured() {
            return Err(MessageThreadStoreError::Storage(StorageError::NotConfigured).into());
        }
        let mut request: ThreadMessagesRequest = serde_json::from_slice(body)
            .map_err(|error| MessagesThreadError::invalid(format!("invalid thread request: {error}")))?;
        let claude_code = claude_code::enabled(&betas);
        if claude_code {
            claude_code::normalize_attribution(&mut request.parameters)?;
        }
        validate_request(&request, &betas)?;
        betas.sort();
        betas.dedup();
        let fixed = fingerprint(&request)?;
        let mut lease = None;
        let checkpoint = match request.thread.clone() {
            MessageThreadParam::Create => MessageThreadCheckpoint {
                messages: Vec::new(),
                create_only: request
                    .parameters
                    .iter()
                    .filter(|(key, _)| matches!(key.as_str(), "system" | "tools"))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
                fixed,
                betas,
                paused: false,
            },
            MessageThreadParam::Continue { previous_message_id } => {
                let mut stored = self.storage.load(&owner, &previous_message_id).await?;
                if claude_code {
                    // Also support checkpoints written before attribution normalization.
                    claude_code::normalize_attribution(&mut stored.create_only)?;
                    claude_code::validate_repeated_fields(&request, &stored.create_only)?;
                }
                validate_continuation(&request, &stored, &fixed, &betas)?;
                for (field, value) in &stored.create_only {
                    request.parameters.insert(field.clone(), value.clone());
                }
                let current =
                    serde_json::to_value(&request).map_err(|e| MessagesThreadError::invalid(e.to_string()))?;
                if validation::cache_breakpoints(&current) > 4 {
                    return Err(MessagesThreadError::unsupported("cache_control"));
                }
                lease = Some(self.storage.reserve(owner.clone(), previous_message_id).await?);
                let MessageThreadCheckpoint {
                    mut messages,
                    create_only,
                    ..
                } = stored;
                messages.append(&mut request.messages);
                request.messages = messages;
                MessageThreadCheckpoint {
                    messages: Vec::new(),
                    create_only,
                    fixed,
                    betas,
                    paused: false,
                }
            }
        };
        let turn = MessagesThreadTurn {
            storage: self.storage.clone(),
            lease,
            owner,
            checkpoint,
            message_id: format!("msg_{}", uuid::Uuid::now_v7().simple()),
        };
        let mut upstream = serde_json::to_value(request).map_err(|e| MessagesThreadError::invalid(e.to_string()))?;
        if let Some(object) = upstream.as_object_mut() {
            object.remove("thread");
        }
        let body = serde_json::to_vec(&upstream).map_err(|e| MessagesThreadError::invalid(e.to_string()))?;
        Ok(Some(PreparedMessagesThread { body, turn }))
    }
}

fn validate_continuation(
    request: &ThreadMessagesRequest,
    stored: &MessageThreadCheckpoint,
    fixed: &BTreeMap<String, Value>,
    betas: &[String],
) -> Result<(), MessagesThreadError> {
    let mut changed: Vec<_> = fixed
        .keys()
        .chain(stored.fixed.keys())
        .filter(|key| fixed.get(*key) != stored.fixed.get(*key))
        .cloned()
        .collect();
    changed.sort();
    changed.dedup();
    if betas != stored.betas {
        changed.push("anthropic-beta".to_owned());
    }
    if !changed.is_empty() {
        return Err(MessagesThreadError::coded(
            MessageThreadErrorCode::ThreadFingerprintMismatch,
            format!("thread fixed fields differ: {}", changed.join(", ")),
        ));
    }
    let first = request.messages.first();
    if stored.paused {
        if first.is_some() {
            return Err(MessagesThreadError::invalid(
                "a paused thread requires an empty messages list",
            ));
        }
    } else if first.is_none_or(|message| message.role != ThreadMessageRole::User) {
        return Err(MessagesThreadError::invalid(
            "thread continuation must start with a new user message",
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub struct PreparedMessagesThread {
    body: Vec<u8>,
    turn: MessagesThreadTurn,
}

impl PreparedMessagesThread {
    /// Enter the existing Messages loop without translating to Responses objects.
    ///
    /// # Errors
    /// Returns an existing Messages validation error for an invalid restored request.
    pub fn into_context(self) -> ExecutorResult<MessagesRequestContext> {
        let parsed = super::ParsedMessagesRequest::parse(&self.body)?;
        let mut context = MessagesRequestContext::new(parsed)?;
        context.thread = Some(self.turn);
        Ok(context)
    }
}

#[derive(Debug)]
pub(super) struct MessagesThreadTurn {
    storage: MessageThreadStore,
    lease: Option<MessageThreadLease>,
    owner: MessagesThreadOwner,
    checkpoint: MessageThreadCheckpoint,
    pub(super) message_id: String,
}

impl MessagesThreadTurn {
    pub(super) async fn complete(
        mut self,
        history: &Value,
        content: &Value,
        stop_reason: Option<&str>,
    ) -> ExecutorResult<()> {
        let retain = matches!(
            stop_reason,
            Some("end_turn" | "stop_sequence" | "tool_use" | "pause_turn" | "max_tokens")
        ) && !(stop_reason == Some("max_tokens")
            && content
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use")));
        let checkpoint = if retain {
            self.checkpoint.messages = serde_json::from_value(history.clone())?;
            self.checkpoint.messages.push(ThreadMessage {
                role: ThreadMessageRole::Assistant,
                content: serde_json::from_value::<ThreadMessageContent>(content.clone())?,
            });
            self.checkpoint.paused = stop_reason == Some("pause_turn");
            Some(&self.checkpoint)
        } else {
            None
        };
        self.storage
            .publish(&self.owner, &self.message_id, checkpoint, self.lease)
            .await
            .map_err(|error| match error {
                MessageThreadStoreError::TooLarge => ExecutorError::PayloadTooLarge(error.to_string()),
                MessageThreadStoreError::Storage(source) => {
                    ExecutorError::Persistence(Box::new(ExecutorError::Storage(source)))
                }
                error => ExecutorError::Conflict(error.to_string()),
            })
    }
}
