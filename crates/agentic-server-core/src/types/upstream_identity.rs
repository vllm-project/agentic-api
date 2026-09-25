//! Bounded upstream model evidence, distinct from a client's requested model.

use std::fmt;

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use thiserror::Error;

use super::request_response::ResponsePayload;

/// Maximum decoded UTF-8 bytes retained for one upstream-reported model identifier.
pub const MAX_UPSTREAM_MODEL_BYTES: usize = 1024;

/// An exact, nonempty model identifier observed in an upstream response.
///
/// This is evidence of what the provider reported, not proof of backend identity
/// or permission to replay opaque reasoning. Debug output deliberately omits it.
#[derive(Clone, PartialEq, Eq)]
pub struct UpstreamModelId(String);

impl UpstreamModelId {
    /// Validate without normalizing, trimming, or resolving an alias.
    ///
    /// # Errors
    /// Rejects empty/whitespace-only identifiers or more than 1024 UTF-8 bytes.
    pub fn new(model: String) -> Result<Self, UpstreamModelError> {
        if model.len() > MAX_UPSTREAM_MODEL_BYTES || model.trim().is_empty() {
            return Err(UpstreamModelError::Invalid);
        }
        Ok(Self(model))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for UpstreamModelId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("UpstreamModelId(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for UpstreamModelId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Safe diagnostics: never retain a rejected model string or upstream payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum UpstreamModelError {
    #[error("upstream response has an invalid 'model'; expected a nonempty string of at most 1024 bytes")]
    Invalid,
    #[error("upstream response changes its reported model during an inference round")]
    Changed,
}

/// Shared typed projection of JSON bodies and SSE response lifecycle objects.
pub(crate) struct UpstreamResponseIdentity {
    pub(crate) model: Option<UpstreamModelId>,
    pub(crate) invalid_model: bool,
}

impl UpstreamResponseIdentity {
    pub(crate) fn observe(response: &Value) -> Self {
        match response.get("model") {
            None | Some(Value::Null) => Self {
                model: None,
                invalid_model: false,
            },
            Some(Value::String(model)) => match UpstreamModelId::new(model.clone()) {
                Ok(model) => Self {
                    model: Some(model),
                    invalid_model: false,
                },
                Err(_) => Self {
                    model: None,
                    invalid_model: true,
                },
            },
            Some(_) => Self {
                model: None,
                invalid_model: true,
            },
        }
    }
}

/// The consuming ingestion result; evidence is never serialized into public JSON.
#[derive(Debug)]
pub(crate) struct IngestedResponse {
    pub(crate) payload: ResponsePayload,
    /// Present only when an explicit terminal response reports a consistent model.
    /// Missing/null metadata and lenient EOF completion must remain unknown.
    pub(crate) upstream_model: Option<UpstreamModelId>,
}
