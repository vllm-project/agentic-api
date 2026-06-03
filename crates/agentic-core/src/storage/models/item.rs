//! Conversation history item stored in the database.

use tracing::warn;

use super::super::pool::{DbPool, DbResult, DbTransaction};
use super::super::types::item::InOutItem;
use crate::types::io::{InputItem, OutputItem};
use crate::utils::common::{deserialize_from_str_opt, utcnow_str};

/// Conversation history item stored in the database.
///
/// Maps to the `items` table and represents a single message/event
/// in a conversation timeline.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Item {
    /// Unique identifier for this item.
    pub id: String,

    /// Item data stored as JSON text.
    /// Deserialized based on context (`message`, `tool_call`, etc.)
    pub data: String,

    /// Creation timestamp as Unix timestamp in seconds.
    pub created_at: i64,

    /// Optional conversation ID for grouping items.
    pub conversation_id: Option<String>,

    /// Optional sequence number within conversation.
    pub seq: Option<i64>,
}

impl Item {
    /// Deserialize data column as `InputItem`.
    #[must_use]
    pub fn as_input(&self) -> Option<InputItem> {
        deserialize_from_str_opt(&self.data)
    }

    /// Deserialize data column as `OutputItem`.
    #[must_use]
    pub fn as_output(&self) -> Option<OutputItem> {
        deserialize_from_str_opt(&self.data)
    }

    /// Deserialize data column as either `InputItem` or `OutputItem`.
    #[must_use]
    pub fn as_inout(&self) -> Option<InOutItem> {
        match (self.as_input(), self.as_output()) {
            (Some(input), _) if !matches!(input, InputItem::Unknown) => Some(InOutItem::Input(input)),
            (_, Some(output)) if !matches!(output, OutputItem::Unknown) => Some(InOutItem::Output(output)),
            _ => {
                warn!(item_id = %self.id, "unrecognized item type in stored data");
                None
            }
        }
    }
}

/// Create items in a transaction with optional conversation context.
///
/// If `conversation_id` and `seq_start` are provided, items are created with sequence numbers.
/// Otherwise, items are created without conversation context.
///
/// # Errors
/// Returns `DbResult::Err` if the database insertion fails.
pub async fn create_in_tx(
    tx: &mut DbTransaction<'_>,
    items: Vec<(String, String)>,
    conversation_id: Option<&str>,
    seq_start: Option<i64>,
) -> DbResult<Vec<Item>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }

    let now = utcnow_str();
    let placeholders: Vec<&str> = vec!["(?, ?, ?, ?, ?)"; items.len()];
    let values_clause = placeholders.join(", ");
    let sql =
        format!("INSERT INTO items (id, data, created_at, conversation_id, seq) VALUES {values_clause} RETURNING *");

    let mut query = sqlx::query_as::<_, Item>(&sql);
    #[allow(clippy::cast_possible_wrap)]
    for (idx, (id, data)) in items.iter().enumerate() {
        let seq = seq_start.map(|start| start + idx as i64);
        query = query.bind(id).bind(data).bind(now).bind(conversation_id).bind(seq);
    }

    query.fetch_all(&mut **tx).await
}

/// Get items by IDs.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_items(pool: &DbPool, ids: &[String]) -> DbResult<Vec<Item>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!("SELECT * FROM items WHERE id IN ({placeholders})");
    let mut q = sqlx::query_as::<_, Item>(&sql);
    for id in ids {
        q = q.bind(id);
    }
    q.fetch_all(pool).await
}

/// Get items by conversation ID ordered by sequence.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_items_by_conversation(pool: &DbPool, conversation_id: &str) -> DbResult<Vec<Item>> {
    sqlx::query_as::<_, Item>("SELECT * FROM items WHERE conversation_id = ? ORDER BY seq ASC")
        .bind(conversation_id)
        .fetch_all(pool)
        .await
}

/// Get next sequence number for items in a conversation.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn conversation_item_count(pool: &DbPool, conversation_id: &str) -> DbResult<Option<i64>> {
    let max_seq: Option<i64> = sqlx::query_scalar("SELECT MAX(seq) FROM items WHERE conversation_id = ?")
        .bind(conversation_id)
        .fetch_optional(pool)
        .await?
        .flatten();

    Ok(Some(max_seq.unwrap_or(-1) + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_item_basic() {
        let item = Item {
            id: "item_123".to_string(),
            data: r#"{"role":"user","content":"hello"}"#.to_string(),
            created_at: 1_704_067_200,
            conversation_id: Some("conv_456".to_string()),
            seq: Some(1),
        };

        assert_eq!(item.id, "item_123");
        assert_eq!(item.conversation_id, Some("conv_456".to_string()));
        assert_eq!(item.seq, Some(1));
    }

    #[test]
    fn test_item_optional_fields() {
        let item = Item {
            id: "item_789".to_string(),
            data: r#"{"role":"assistant"}"#.to_string(),
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
        };

        assert!(item.conversation_id.is_none());
        assert!(item.seq.is_none());
    }
}
