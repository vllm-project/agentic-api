//! Conversation context and history.

use super::super::pool::{DbPool, DbResult};
use crate::utils::common::utcnow_str;

/// Conversation context and history.
///
/// Maps to the `conversations` table and represents a logical conversation
/// containing multiple responses and items.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Conversation {
    /// Unique conversation identifier.
    pub id: String,

    /// Optional metadata as JSON string.
    pub metadata: Option<String>,

    /// Creation timestamp as Unix timestamp in seconds.
    pub created_at: i64,
}

/// Create a new conversation.
///
/// # Errors
/// Returns `DbResult::Err` if the database insertion fails.
pub async fn create(pool: &DbPool, id: &str) -> DbResult<Conversation> {
    let now = utcnow_str();
    sqlx::query_as::<_, Conversation>(
        "INSERT INTO conversations (id, created_at) \
         VALUES (?, ?) RETURNING *",
    )
    .bind(id)
    .bind(now)
    .fetch_one(pool)
    .await
}

/// Get or create a conversation.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_or_create(pool: &DbPool, id: &str) -> DbResult<Conversation> {
    let now = utcnow_str();
    sqlx::query_as::<_, Conversation>(
        "INSERT INTO conversations (id, created_at) \
         VALUES (?, ?) \
         ON CONFLICT (id) DO UPDATE SET created_at = created_at \
         RETURNING *",
    )
    .bind(id)
    .bind(now)
    .fetch_one(pool)
    .await
}

/// Get a conversation by ID.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get(pool: &DbPool, id: &str) -> DbResult<Option<Conversation>> {
    sqlx::query_as::<_, Conversation>("SELECT * FROM conversations WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conversation_basic() {
        let conversation = Conversation {
            id: "conv_1".to_string(),
            metadata: None,
            created_at: 1_704_067_200,
        };

        assert_eq!(conversation.id, "conv_1");
        assert!(conversation.metadata.is_none());
        assert_eq!(conversation.created_at, 1_704_067_200);
    }
}
