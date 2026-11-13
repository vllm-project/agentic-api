//! Types for the OpenAI-compatible Conversations API.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::io::InOutItem;

/// Most key-value pairs one metadata object may hold.
pub const MAX_METADATA_PAIRS: usize = 16;
/// Longest metadata key, in characters.
pub const MAX_METADATA_KEY_CHARS: usize = 64;
/// Longest metadata value, in characters.
pub const MAX_METADATA_VALUE_CHARS: usize = 512;

/// String key-value metadata, as `OpenAI` defines it for conversations,
/// responses and vector stores: at most 16 pairs, keys of 1 to 64 characters
/// and values of at most 512 characters.
///
/// The limits are checked when the value is decoded, like [`ItemLimit`], so a
/// request carrying invalid metadata cannot be parsed and every type holding
/// this one gets the rule. Rows stored before the limits were enforced are
/// read back with [`Self::deserialize_stored`], which decodes the inner map
/// without checking it.
///
/// [`ItemLimit`]: super::io::ItemLimit
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "BTreeMap<String, String>", into = "BTreeMap<String, String>")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ConversationMetadata(BTreeMap<String, String>);

impl ConversationMetadata {
    /// Metadata without pairs.
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// The pairs as a map.
    #[must_use]
    pub fn into_inner(self) -> BTreeMap<String, String> {
        self.0
    }

    /// Decodes stored metadata from its inner map without checking the limits,
    /// so metadata saved before they were enforced still loads. For fields of
    /// stored or returned objects only; requests decode through `TryFrom`.
    ///
    /// # Errors
    ///
    /// Returns the deserializer's error when the value is not a string map.
    pub fn deserialize_stored<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        BTreeMap::deserialize(deserializer).map(Self)
    }

    /// Wraps a stored map without checking the limits; see [`Self::deserialize_stored`].
    #[must_use]
    pub(crate) fn from_stored(pairs: BTreeMap<String, String>) -> Self {
        Self(pairs)
    }
}

impl std::ops::Deref for ConversationMetadata {
    type Target = BTreeMap<String, String>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TryFrom<BTreeMap<String, String>> for ConversationMetadata {
    type Error = InvalidMetadata;

    fn try_from(pairs: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        if pairs.len() > MAX_METADATA_PAIRS {
            return Err(InvalidMetadata::TooManyPairs(pairs.len()));
        }
        for (key, value) in &pairs {
            let key_chars = key.chars().count();
            if key_chars == 0 || key_chars > MAX_METADATA_KEY_CHARS {
                return Err(InvalidMetadata::KeyLength(key_chars));
            }
            let value_chars = value.chars().count();
            if value_chars > MAX_METADATA_VALUE_CHARS {
                return Err(InvalidMetadata::ValueTooLong {
                    key: key.clone(),
                    chars: value_chars,
                });
            }
        }
        Ok(Self(pairs))
    }
}

impl From<ConversationMetadata> for BTreeMap<String, String> {
    fn from(metadata: ConversationMetadata) -> Self {
        metadata.0
    }
}

/// Metadata that breaks the limits `OpenAI` documents for it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidMetadata {
    #[error("metadata accepts at most {MAX_METADATA_PAIRS} key-value pairs, but got {0}")]
    TooManyPairs(usize),
    #[error("metadata keys must have 1 to {MAX_METADATA_KEY_CHARS} characters, but got a key of {0} characters")]
    KeyLength(usize),
    #[error(
        "metadata values must have at most {MAX_METADATA_VALUE_CHARS} characters, but the value of {key:?} has {chars}"
    )]
    ValueTooLong { key: String, chars: usize },
}

impl InvalidMetadata {
    /// Request parameter the error is reported on.
    #[must_use]
    pub fn param(&self) -> &'static str {
        "metadata"
    }

    /// Machine-readable code for invalid metadata.
    #[must_use]
    pub fn code(&self) -> &'static str {
        "invalid_value"
    }
}

/// Request to create a new conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CreateConversationRequest {
    /// Optional metadata as a JSON object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ConversationMetadata>,

    /// Optional initial items to add to the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<InOutItem>>,
}

/// Request to update a conversation's metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UpdateConversationRequest {
    /// Metadata as a JSON object.
    pub metadata: ConversationMetadata,
}

/// Response for a conversation resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ConversationResponse {
    /// Unique conversation identifier.
    pub id: String,

    /// Object type, always "conversation".
    pub object: String,

    /// Creation timestamp as Unix timestamp in seconds.
    pub created_at: i64,

    /// Metadata as a JSON object.
    #[serde(default, deserialize_with = "ConversationMetadata::deserialize_stored")]
    pub metadata: ConversationMetadata,
}

/// Request to append up to 20 items to a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CreateItemRequest {
    /// Items to append in the supplied order.
    pub items: Vec<InOutItem>,
}

impl ConversationResponse {
    /// Create a new conversation response.
    #[must_use]
    pub fn new(id: String, created_at: i64, metadata: ConversationMetadata) -> Self {
        Self {
            id,
            object: "conversation".to_string(),
            created_at,
            metadata,
        }
    }
}

#[cfg(test)]
mod tests;
