//! Types for the OpenAI-compatible Conversations API.

use std::collections::BTreeMap;
use std::ops::Deref;

use serde::{Deserialize, Serialize};
#[cfg(feature = "openapi")]
use utoipa::{
    PartialSchema, ToSchema,
    openapi::schema::{ObjectBuilder, SchemaType, Type},
    openapi::{RefOr, Schema},
};

use super::io::InOutItem;

/// Maximum number of metadata key-value pairs, matching the Responses and Conversations limits.
pub(crate) const MAX_METADATA_ITEMS: usize = 16;
/// Maximum length of a metadata key in characters.
pub(crate) const MAX_METADATA_KEY_CHARS: usize = 64;
/// Maximum length of a metadata value in characters.
pub(crate) const MAX_METADATA_VALUE_CHARS: usize = 512;

/// Request parameter name reported in metadata validation errors.
pub(crate) const METADATA_PARAM: &str = "metadata";

/// A supplied metadata object outside the documented `OpenAI` limits: at most
/// [`MAX_METADATA_ITEMS`] pairs, keys of 1 to [`MAX_METADATA_KEY_CHARS`]
/// characters, and values up to [`MAX_METADATA_VALUE_CHARS`] characters. Lengths
/// are counted in characters, as documented, rather than bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidMetadata {
    #[error("metadata accepts at most {MAX_METADATA_ITEMS} key-value pairs")]
    TooManyItems,
    #[error("metadata keys must contain between 1 and {MAX_METADATA_KEY_CHARS} characters")]
    InvalidKey,
    #[error("metadata values must contain at most {MAX_METADATA_VALUE_CHARS} characters")]
    ValueTooLong,
}

impl InvalidMetadata {
    /// The request parameter the error is reported on.
    #[must_use]
    pub(crate) fn param(&self) -> &'static str {
        match self {
            Self::TooManyItems | Self::InvalidKey | Self::ValueTooLong => METADATA_PARAM,
        }
    }

    /// The machine-readable code carried in the API error envelope.
    #[must_use]
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::TooManyItems | Self::InvalidKey | Self::ValueTooLong => "invalid_value",
        }
    }
}

/// String key-value metadata, as `OpenAI` defines it for conversations and
/// responses alike. The type is validated at construction: an inbound object
/// that violates the documented limits cannot be decoded, so every request
/// path that carries the type enforces the same rule. Stored rows decode with
/// [`ConversationMetadata::from_stored`], which trusts already-persisted data
/// so conversations saved before the limit are still readable.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "BTreeMap<String, String>", into = "BTreeMap<String, String>")]
pub struct ConversationMetadata(BTreeMap<String, String>);

impl ConversationMetadata {
    /// Empty metadata.
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Rebuild metadata read from storage without re-applying the request
    /// limits, so rows persisted before validation still load.
    #[must_use]
    pub(crate) fn from_stored(map: BTreeMap<String, String>) -> Self {
        Self(map)
    }

    /// Consume the validated metadata into the underlying map.
    #[must_use]
    pub(crate) fn into_inner(self) -> BTreeMap<String, String> {
        self.0
    }

    /// Apply the documented request limits to a metadata map.
    pub(crate) fn validate(map: &BTreeMap<String, String>) -> Result<(), InvalidMetadata> {
        if map.len() > MAX_METADATA_ITEMS {
            return Err(InvalidMetadata::TooManyItems);
        }
        if map.keys().any(|key| {
            let len = key.chars().count();
            !(1..=MAX_METADATA_KEY_CHARS).contains(&len)
        }) {
            return Err(InvalidMetadata::InvalidKey);
        }
        if map
            .values()
            .any(|value| value.chars().count() > MAX_METADATA_VALUE_CHARS)
        {
            return Err(InvalidMetadata::ValueTooLong);
        }
        Ok(())
    }
}

impl TryFrom<BTreeMap<String, String>> for ConversationMetadata {
    type Error = InvalidMetadata;

    fn try_from(map: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        Self::validate(&map)?;
        Ok(Self(map))
    }
}

impl From<ConversationMetadata> for BTreeMap<String, String> {
    fn from(metadata: ConversationMetadata) -> Self {
        metadata.0
    }
}

impl Deref for ConversationMetadata {
    type Target = BTreeMap<String, String>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(feature = "openapi")]
impl PartialSchema for ConversationMetadata {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(SchemaType::new(Type::Object))
            .additional_properties(Some(ObjectBuilder::new().schema_type(SchemaType::new(Type::String))))
            .into()
    }
}

#[cfg(feature = "openapi")]
impl ToSchema for ConversationMetadata {}

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
