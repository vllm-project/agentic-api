//! Domain type for conversation storage.

use super::super::models::Conversation as StorageDbConversation;

/// Domain entity for a stored conversation.
///
/// Represents a conversation context with metadata and history tracking.
#[derive(Debug, Clone)]
pub struct ConversationData {
    /// Unique conversation identifier
    pub conversation_id: String,
    /// Optional metadata as JSON string
    pub metadata: Option<String>,
    /// Creation timestamp as Unix timestamp in seconds
    pub created_at: i64,
}

impl From<StorageDbConversation> for ConversationData {
    fn from(row: StorageDbConversation) -> Self {
        Self {
            conversation_id: row.id,
            metadata: row.metadata,
            created_at: row.created_at,
        }
    }
}

impl From<ConversationData> for StorageDbConversation {
    fn from(data: ConversationData) -> Self {
        Self {
            id: data.conversation_id,
            metadata: data.metadata,
            created_at: data.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conversation_from_db_conversation() {
        let db_row = StorageDbConversation {
            id: "conv_123".to_string(),
            metadata: None,
            created_at: 1_704_067_200,
        };

        let conversation: ConversationData = db_row.into();
        assert_eq!(conversation.conversation_id, "conv_123");
        assert_eq!(conversation.created_at, 1_704_067_200);
    }

    #[test]
    fn test_conversation_roundtrip() {
        let data = ConversationData {
            conversation_id: "conv_456".to_string(),
            metadata: Some(r#"{"key":"value"}"#.to_string()),
            created_at: 1_704_067_200,
        };

        let db_row: StorageDbConversation = data.into();
        assert_eq!(db_row.id, "conv_456");
        assert_eq!(db_row.metadata, Some(r#"{"key":"value"}"#.to_string()));
        assert_eq!(db_row.created_at, 1_704_067_200);
    }

    #[test]
    fn test_conversation_data_clone() {
        let data = ConversationData {
            conversation_id: "conv_clone".to_string(),
            metadata: None,
            created_at: 1_704_067_200,
        };

        let cloned = data.clone();
        assert_eq!(cloned.conversation_id, data.conversation_id);
        assert_eq!(cloned.created_at, data.created_at);
    }

    #[test]
    fn test_conversation_data_debug_format() {
        let data = ConversationData {
            conversation_id: "conv_debug".to_string(),
            metadata: None,
            created_at: 1_704_067_200,
        };

        let debug_str = format!("{data:?}");
        assert!(debug_str.contains("conv_debug"));
        assert!(debug_str.contains("ConversationData"));
    }

    #[test]
    fn test_conversation_bidirectional_conversion() {
        let original = ConversationData {
            conversation_id: "conv_bidir".to_string(),
            metadata: None,
            created_at: 1_706_790_600,
        };

        let db_row: StorageDbConversation = original.clone().into();
        let recovered: ConversationData = db_row.into();

        assert_eq!(original.conversation_id, recovered.conversation_id);
        assert_eq!(original.created_at, recovered.created_at);
    }
}
