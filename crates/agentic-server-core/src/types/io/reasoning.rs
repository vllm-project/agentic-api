//! Typed reasoning content shared by public items, continuation history, and storage.
//!
//! Opaque state is a provider-owned string, not arbitrary JSON. Keeping it typed
//! does not authorize replay to another provider or model family.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, de};
use thiserror::Error;

/// Absolute decoded-byte ceiling for one opaque reasoning value (16 MiB).
///
/// This is a gateway safety limit, not a provider limit. Request admission,
/// upstream wire limits, and the shared retained-response budget may be smaller.
pub const MAX_OPAQUE_REASONING_BYTES: usize = 16 * 1024 * 1024;

/// Invalid opaque reasoning state. Errors never include the supplied state.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum OpaqueReasoningError {
    /// The decoded UTF-8 string exceeds the gateway's absolute ceiling.
    #[error("encrypted reasoning exceeds the {MAX_OPAQUE_REASONING_BYTES}-byte limit")]
    TooLarge,
}

/// Provider-owned reasoning state, preserved without decoding or normalization.
///
/// Debug output is deliberately redacted. Serialization is the exact string,
/// including whitespace; callers must not trim, decrypt, or summarize it.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "openapi", schema(value_type = String))]
pub struct OpaqueReasoning(String);

impl OpaqueReasoning {
    /// Borrow the exact provider-owned string for serialization or accounting.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for OpaqueReasoning {
    type Error = OpaqueReasoningError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() > MAX_OPAQUE_REASONING_BYTES {
            return Err(OpaqueReasoningError::TooLarge);
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for OpaqueReasoning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpaqueReasoning")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl<'de> Deserialize<'de> for OpaqueReasoning {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OpaqueVisitor;

        impl de::Visitor<'_> for OpaqueVisitor {
            type Value = OpaqueReasoning;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded opaque reasoning string")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value.len() > MAX_OPAQUE_REASONING_BYTES {
                    return Err(E::custom(OpaqueReasoningError::TooLarge));
                }
                Ok(OpaqueReasoning(value.to_owned()))
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                OpaqueReasoning::try_from(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_string(OpaqueVisitor)
    }
}

/// Wire discriminator for plaintext reasoning content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReasoningTextKind {
    ReasoningText,
}

/// Plaintext reasoning emitted by a compatible upstream reasoning parser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ReasoningTextContent {
    #[serde(rename = "type")]
    pub type_: ReasoningTextKind,
    pub text: String,
}

impl ReasoningTextContent {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            type_: ReasoningTextKind::ReasoningText,
            text: text.into(),
        }
    }
}

/// Wire discriminator for a public reasoning summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummaryKind {
    SummaryText,
}

/// A public reasoning summary, distinct from plaintext or opaque reasoning state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ReasoningSummaryContent {
    #[serde(rename = "type")]
    pub type_: ReasoningSummaryKind,
    pub text: String,
}

impl ReasoningSummaryContent {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            type_: ReasoningSummaryKind::SummaryText,
            text: text.into(),
        }
    }
}

/// Lifecycle status of a reasoning item, not of its enclosing response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReasoningStatus {
    InProgress,
    Completed,
    Incomplete,
}

impl ReasoningStatus {
    /// Return the canonical wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
        }
    }
}
