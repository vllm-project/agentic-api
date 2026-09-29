//! Types for the OpenAI-compatible Conversations API.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::event::MessageStatus;
use super::io::{InputContent, InputMessageContent, InputTextContent};
use serde_json::Value;

use super::io::{InputItem, OutputItem};

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
    pub items: Option<Vec<ConversationItem>>,
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
    pub items: Vec<ConversationItem>,
}

/// Response for listing items with pagination.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ListItemsResponse {
    /// Object type, always "list".
    pub object: String,

    /// Array of conversation items.
    pub data: Vec<ItemResponse>,

    /// Whether there are more items available.
    pub has_more: bool,

    /// ID of the first item in this page.
    pub first_id: Option<String>,

    /// ID of the last item in this page.
    pub last_id: Option<String>,
}

/// A conversation item with its storage identity and flattened public payload.
#[derive(Debug, Clone)]
pub struct ItemResponse {
    pub id: String,
    pub item: ConversationItem,
}

#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for ItemResponse {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{AllOfBuilder, Type};
        use utoipa::openapi::{ObjectBuilder, Ref};
        AllOfBuilder::new()
            .item(
                ObjectBuilder::new()
                    .property("id", ObjectBuilder::new().schema_type(Type::String))
                    .required("id"),
            )
            .item(Ref::from_schema_name("ConversationItem"))
            .into()
    }
}

#[cfg(feature = "openapi")]
impl utoipa::ToSchema for ItemResponse {}

impl Serialize for ItemResponse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value = serde_json::to_value(&self.item).map_err(serde::ser::Error::custom)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| serde::ser::Error::custom("item must be an object"))?;
        // Stored Responses output may retain an upstream ID. The public resource
        // must use the same ID as its retrieval URL and pagination cursor, once.
        object.insert("id".to_owned(), Value::String(self.id.clone()));
        value.serialize(serializer)
    }
}

/// A supported conversation input or output item.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum ConversationItem {
    Input(InputItem),
    Output(OutputItem),
}

impl<'de> Deserialize<'de> for ConversationItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let input = InputItem::deserialize(&value).map_err(serde::de::Error::custom)?;
        if !matches!(input, InputItem::Unknown) {
            return Ok(Self::Input(input));
        }
        // InputItem's forward-compatible fallback must not consume output-only
        // kinds such as web_search_call and mcp_call and discard their payloads.
        let output = OutputItem::deserialize(value).map_err(serde::de::Error::custom)?;
        if matches!(output, OutputItem::Unknown) {
            return Err(serde::de::Error::custom("unsupported conversation item type"));
        }
        Ok(Self::Output(output))
    }
}

/// Ordering for conversation item pages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum ItemOrder {
    Asc,
    #[default]
    Desc,
}

/// Response for successful deletion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeletedResponse {
    /// ID of the deleted resource.
    pub id: String,

    /// Object type.
    pub object: String,

    /// Whether the deletion was successful.
    pub deleted: bool,
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

impl ItemResponse {
    /// Build the public resource, expanding shorthand message content.
    #[must_use]
    pub fn new(id: String, mut item: ConversationItem) -> Self {
        if let ConversationItem::Input(InputItem::Message(message)) = &mut item {
            message.status.get_or_insert(MessageStatus::Completed);
            if let InputMessageContent::Text(text) = &mut message.content {
                let content = InputTextContent::new(std::mem::take(text));
                message.content = InputMessageContent::Parts(vec![InputContent::InputText(content)]);
            }
        }
        Self { id, item }
    }
}

impl ListItemsResponse {
    /// Create a new list response.
    #[must_use]
    pub fn new(data: Vec<ItemResponse>, has_more: bool) -> Self {
        let first_id = data.first().map(|item| item.id.clone());
        let last_id = data.last().map(|item| item.id.clone());

        Self {
            object: "list".to_string(),
            data,
            has_more,
            first_id,
            last_id,
        }
    }
}

impl DeletedResponse {
    /// Create a deleted conversation response.
    #[must_use]
    pub fn conversation(id: String) -> Self {
        Self {
            id,
            object: "conversation.deleted".to_string(),
            deleted: true,
        }
    }

    /// Create a deleted item response.
    #[must_use]
    pub fn item(id: String) -> Self {
        Self {
            id,
            object: "conversation.item.deleted".to_string(),
            deleted: true,
        }
    }
}

#[cfg(test)]
mod tests;
