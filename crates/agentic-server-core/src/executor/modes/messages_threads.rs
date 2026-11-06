//! Messages-native thread orchestration over independent durable storage.

mod claude_code;
#[cfg(test)]
mod tests;
mod validation;

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;

use crate::executor::{ExecutorError, ExecutorResult, MessagesRequestContext, ParsedMessagesRequest};
pub(in crate::executor) use crate::storage::messages::MAX_CHECKPOINT_BYTES;
use crate::storage::messages::{MessageThreadLease, MessageThreadStore, MessageThreadStoreError};
use crate::storage::{DbPool, StorageError};
use crate::types::messages::parameters::RequestField;
use crate::types::messages::thread::{
    MessageThreadCheckpoint, MessageThreadErrorBody, MessageThreadErrorCode, MessageThreadErrorDetails,
    MessageThreadErrorInfo, MessageThreadErrorKind, MessageThreadErrorType, MessageThreadParam, MessagesThreadOwner,
    ThreadFingerprint,
};
use crate::types::messages::{MessageContent, MessageParam, MessagesRequest};
use validation::validate_request;

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

    #[must_use]
    pub fn owner(&self, components: &[&[u8]]) -> MessagesThreadOwner {
        MessagesThreadOwner::from_components(components)
    }

    /// Validate and rehydrate an already-decoded Messages thread request.
    ///
    /// # Errors
    /// Returns typed protocol, storage, or cross-instance reservation errors before inference.
    pub async fn prepare(
        &self,
        mut parsed: ParsedMessagesRequest,
        owner: MessagesThreadOwner,
        betas: Vec<String>,
    ) -> Result<PreparedMessagesThread, MessagesThreadError> {
        if !self.storage.is_configured() {
            return Err(MessageThreadStoreError::Storage(StorageError::NotConfigured).into());
        }
        let request = &mut parsed.typed;
        let claude_code = claude_code::enabled(&betas);
        if claude_code {
            claude_code::normalize_attribution(&mut request.system);
        }
        validate_request(request, &betas)?;
        let betas = betas.into_iter().collect::<BTreeSet<_>>();
        let fingerprint = ThreadFingerprint::from(&*request);
        let mut lease = None;
        let checkpoint = match request
            .thread
            .take()
            .ok_or_else(|| MessagesThreadError::invalid("missing thread"))?
        {
            MessageThreadParam::Create => MessageThreadCheckpoint {
                messages: Vec::new(),
                system: request.system.as_ref().cloned(),
                tools: request.tools.as_ref().cloned(),
                fingerprint,
                betas,
                paused: false,
            },
            MessageThreadParam::Continue { previous_message_id } => {
                let stored = self.storage.load(&owner, &previous_message_id).await?;
                if claude_code {
                    claude_code::validate_repeated_fields(request, &stored)?;
                }
                validate_continuation(request, &stored, &fingerprint, &betas)?;
                request.system = stored.system.clone().map_or(RequestField::Missing, RequestField::Value);
                request.tools = stored.tools.clone().map_or(RequestField::Missing, RequestField::Value);
                if validation::cache_breakpoints(request) > 4 {
                    return Err(MessagesThreadError::unsupported("cache_control"));
                }
                lease = Some(self.storage.reserve(owner.clone(), previous_message_id).await?);
                let MessageThreadCheckpoint {
                    mut messages,
                    system,
                    tools,
                    ..
                } = stored;
                messages.append(&mut request.messages);
                request.messages = messages;
                MessageThreadCheckpoint {
                    messages: Vec::new(),
                    system,
                    tools,
                    fingerprint,
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
        Ok(PreparedMessagesThread { parsed, turn })
    }
}

fn validate_continuation(
    request: &MessagesRequest,
    stored: &MessageThreadCheckpoint,
    fingerprint: &ThreadFingerprint,
    betas: &BTreeSet<String>,
) -> Result<(), MessagesThreadError> {
    let mut changed = fingerprint.differing_fields(&stored.fingerprint);
    if betas != &stored.betas {
        changed.push("anthropic-beta");
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
    } else if first.is_none_or(|message| message.role != "user") {
        return Err(MessagesThreadError::invalid(
            "thread continuation must start with a new user message",
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub struct PreparedMessagesThread {
    parsed: ParsedMessagesRequest,
    turn: MessagesThreadTurn,
}

impl PreparedMessagesThread {
    /// Enter the existing Messages loop without translating to Responses objects.
    ///
    /// # Errors
    /// Returns an existing Messages validation error for an invalid restored request.
    pub fn into_context(mut self) -> ExecutorResult<MessagesRequestContext> {
        self.parsed.project_thread_history()?;
        let mut context = MessagesRequestContext::new(self.parsed)?;
        context.thread = Some(self.turn);
        Ok(context)
    }
}

/// Whether a completed upstream round may seed another thread turn. Decide
/// from the original round before removing calls that the client cannot see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::executor) enum ThreadRetention {
    Discard,
    Ready,
    Paused,
}

impl ThreadRetention {
    pub(in crate::executor) fn for_response(stop_reason: Option<&str>, has_tool_call: bool) -> Self {
        match stop_reason {
            Some("pause_turn") => Self::Paused,
            Some("end_turn" | "stop_sequence" | "tool_use") => Self::Ready,
            Some("max_tokens") if !has_tool_call => Self::Ready,
            _ => Self::Discard,
        }
    }
}

#[derive(Debug)]
pub(in crate::executor) struct MessagesThreadTurn {
    storage: MessageThreadStore,
    lease: Option<MessageThreadLease>,
    owner: MessagesThreadOwner,
    checkpoint: MessageThreadCheckpoint,
    pub(in crate::executor) message_id: String,
}

impl MessagesThreadTurn {
    pub(in crate::executor) async fn complete(
        mut self,
        history: &Value,
        content: &Value,
        retention: ThreadRetention,
    ) -> ExecutorResult<()> {
        let checkpoint = match retention {
            ThreadRetention::Discard => None,
            ThreadRetention::Ready | ThreadRetention::Paused => {
                self.checkpoint.messages =
                    serde_json::from_value(history.clone()).map_err(ExecutorError::UpstreamProtocol)?;
                self.checkpoint.messages.push(MessageParam {
                    role: "assistant".to_owned(),
                    content: serde_json::from_value::<MessageContent>(content.clone())
                        .map_err(ExecutorError::UpstreamProtocol)?,
                });
                self.checkpoint.paused = retention == ThreadRetention::Paused;
                Some(&self.checkpoint)
            }
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
