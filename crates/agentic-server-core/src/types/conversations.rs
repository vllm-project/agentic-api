//! Types for the OpenAI-compatible Conversations API.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::io::InOutItem;

/// String key-value metadata, as `OpenAI` defines it for conversations and responses alike.
pub type ConversationMetadata = BTreeMap<String, String>;

/// Most key-value pairs one metadata object may hold.
pub const MAX_METADATA_ENTRIES: usize = 16;
/// Longest metadata key, in bytes.
pub const MAX_METADATA_KEY_BYTES: usize = 64;
/// Longest metadata value, in bytes.
pub const MAX_METADATA_VALUE_BYTES: usize = 512;

/// Why a metadata object breaks the limits `OpenAI` documents for it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetadataError {
    #[error("metadata accepts at most {MAX_METADATA_ENTRIES} key-value pairs, but got {0}")]
    TooManyEntries(usize),
    #[error("metadata keys must contain 1 to {MAX_METADATA_KEY_BYTES} bytes, but got a key of {0} bytes")]
    InvalidKeyLength(usize),
    #[error(
        "metadata values must contain at most {MAX_METADATA_VALUE_BYTES} bytes, but the value of {key:?} has {len}"
    )]
    ValueTooLong { key: String, len: usize },
}

/// Checks `metadata` against the `OpenAI` limits: at most 16 pairs, keys of 1 to 64 bytes and values of at most
/// 512 bytes.
///
/// # Errors
///
/// Returns the first limit the object breaks.
pub fn validate_metadata(metadata: &ConversationMetadata) -> Result<(), MetadataError> {
    if metadata.len() > MAX_METADATA_ENTRIES {
        return Err(MetadataError::TooManyEntries(metadata.len()));
    }
    for (key, value) in metadata {
        if key.is_empty() || key.len() > MAX_METADATA_KEY_BYTES {
            return Err(MetadataError::InvalidKeyLength(key.len()));
        }
        if value.len() > MAX_METADATA_VALUE_BYTES {
            return Err(MetadataError::ValueTooLong {
                key: key.clone(),
                len: value.len(),
            });
        }
    }
    Ok(())
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
    #[serde(default)]
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
