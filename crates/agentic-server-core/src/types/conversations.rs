//! Types for the OpenAI-compatible Conversations API.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::io::InOutItem;

/// String metadata attached to a conversation.
pub type ConversationMetadata = BTreeMap<String, String>;

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
    pub fn new(id: String, created_at: i64, metadata: Option<ConversationMetadata>) -> Self {
        Self {
            id,
            object: "conversation".to_string(),
            created_at,
            metadata: metadata.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests;
