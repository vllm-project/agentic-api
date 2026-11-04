//! Domain type for conversation storage.

use super::super::models::Conversation as StorageDbConversation;
use super::errors::{StorageError, StoreResult};
use super::response::ResponseMetadata;
use crate::types::conversations::ConversationMetadata;
use crate::types::io::InOutItem;

/// Rehydrated conversation items together with the metadata of the latest turn.
#[derive(Debug, Clone)]
pub struct ConversationSnapshot {
    /// Rehydrated input and output items in stored order.
    pub items: Vec<InOutItem>,
    /// The conversation's latest turn; `None` before the first turn.
    pub latest_response_id: Option<String>,
    /// `responses.metadata` of the latest turn; `None` before the first turn.
    pub turn_metadata: Option<ResponseMetadata>,
}

/// Domain entity for a stored conversation.
///
/// Represents a conversation context with metadata and history tracking.
#[derive(Debug, Clone)]
pub struct ConversationData {
    /// Unique conversation identifier
    pub conversation_id: String,
    /// Client-supplied metadata; empty when none was set
    pub metadata: ConversationMetadata,
    /// Creation timestamp as Unix timestamp in seconds
    pub created_at: i64,
    /// Tenant identifier for multi-tenancy isolation
    pub tenant_id: Option<String>,
}

impl TryFrom<StorageDbConversation> for ConversationData {
    type Error = StorageError;

    fn try_from(row: StorageDbConversation) -> Result<Self, Self::Error> {
        let metadata = row
            .metadata
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            conversation_id: row.id,
            metadata,
            created_at: row.created_at,
            tenant_id: row.tenant_id,
        })
    }
}

/// Encode client metadata for the `conversations.metadata` column.
///
/// # Errors
/// Returns a serialization error.
pub(in crate::storage) fn encode_metadata(metadata: &ConversationMetadata) -> StoreResult<String> {
    Ok(serde_json::to_string(metadata)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(metadata: Option<&str>) -> StorageDbConversation {
        StorageDbConversation {
            id: "conv_123".to_owned(),
            metadata: metadata.map(str::to_owned),
            created_at: 1_704_067_200,
            latest_response_id: None,
            tenant_id: Some("tenant_test".to_owned()),
        }
    }

    #[test]
    fn row_decodes_typed_metadata() {
        let conversation = ConversationData::try_from(row(Some(r#"{"key":"value"}"#))).unwrap();
        assert_eq!(conversation.conversation_id, "conv_123");
        assert_eq!(conversation.created_at, 1_704_067_200);
        assert_eq!(conversation.tenant_id.as_deref(), Some("tenant_test"));
        assert_eq!(conversation.metadata.get("key").map(String::as_str), Some("value"));
    }

    #[test]
    fn missing_metadata_is_empty() {
        assert!(ConversationData::try_from(row(None)).unwrap().metadata.is_empty());
    }

    #[test]
    fn malformed_metadata_is_an_error() {
        assert!(
            ConversationData::try_from(row(Some("not json")))
                .unwrap_err()
                .is_serialization()
        );
    }

    #[test]
    fn metadata_round_trips_through_its_column() {
        let metadata = ConversationMetadata::from([("topic".to_owned(), "billing".to_owned())]);
        let encoded = encode_metadata(&metadata).unwrap();
        let decoded = ConversationData::try_from(row(Some(&encoded))).unwrap();
        assert_eq!(decoded.metadata, metadata);
    }
}
