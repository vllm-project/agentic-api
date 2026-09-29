//! Conversation context and history.

use super::super::pool::{DbPool, DbResult, DbTransaction};
use super::item::Item;
use crate::storage::backend::DatabaseBackend;
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

    /// Response that committed the latest conversation turn.
    pub latest_response_id: Option<String>,

    /// Tenant identifier for multi-tenancy isolation.
    pub tenant_id: Option<String>,

    /// Revision incremented for every change to conversation items.
    pub revision: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct ConversationSnapshotRow {
    revision: i64,
    latest_response_id: Option<String>,
    item_id: Option<String>,
    item_reference_id: Option<String>,
    item_data: Option<String>,
    item_created_at: Option<i64>,
    item_conversation_id: Option<String>,
    item_sequence: Option<i64>,
}

/// Item rows and the latest persisted response captured by one database statement.
#[derive(Debug)]
pub struct ConversationSnapshotRows {
    pub revision: i64,
    pub latest_response_id: Option<String>,
    pub items: Vec<Item>,
}

/// Create a new conversation.
///
/// # Errors
/// Returns `DbResult::Err` if the database insertion fails.
pub async fn create(pool: &DbPool, id: &str) -> DbResult<Conversation> {
    let mut tx = pool.begin().await?;
    let row = create_in_tx(&mut tx, id, None, None).await?;
    tx.commit().await?;
    Ok(row)
}

/// Get or create a conversation.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_or_create(pool: &DbPool, id: &str) -> DbResult<Conversation> {
    let now = utcnow_str();
    sqlx::query_as::<_, Conversation>(
        "INSERT INTO conversations (id, created_at) \
         VALUES ($1, $2) \
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
    sqlx::query_as::<_, Conversation>("SELECT * FROM conversations WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Get conversation items and the latest response pointer in one consistent snapshot.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails or a joined item row is malformed.
pub async fn get_snapshot(pool: &DbPool, id: &str) -> DbResult<ConversationSnapshotRows> {
    let rows = sqlx::query_as::<_, ConversationSnapshotRow>(
        "SELECT conversations.latest_response_id, conversations.revision, \
                items.id AS item_id, items.reference_id AS item_reference_id, \
                items.data AS item_data, \
                items.created_at AS item_created_at, \
                items.conversation_id AS item_conversation_id, \
                items.seq AS item_sequence \
         FROM conversations \
         LEFT JOIN items ON items.conversation_id = conversations.id \
         WHERE conversations.id = $1 \
         ORDER BY items.seq ASC",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;

    let revision = rows.first().map_or(0, |row| row.revision);
    let latest_response_id = rows.first().and_then(|row| row.latest_response_id.clone());
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        match (
            row.item_id,
            row.item_data,
            row.item_created_at,
            row.item_conversation_id,
        ) {
            (Some(id), Some(data), Some(created_at), Some(conversation_id)) => items.push(Item {
                id,
                data,
                created_at,
                conversation_id: Some(conversation_id),
                seq: row.item_sequence,
                tenant_id: None,
                reference_id: row.item_reference_id,
            }),
            (None, None, None, None) => {}
            _ => {
                return Err(sqlx::Error::Protocol(
                    "conversation snapshot contains a partial item row".to_owned(),
                ));
            }
        }
    }

    Ok(ConversationSnapshotRows {
        revision,
        latest_response_id,
        items,
    })
}

/// Locks an existing conversation for the lifetime of the transaction.
///
/// `PostgreSQL` takes a row lock without writing the row. `SQLite` uses a no-op
/// update to acquire its database-wide write lock, which serializes persistence
/// across all conversations. Both protect sequence allocation when multiple
/// gateway replicas persist turns concurrently, but with different lock granularity.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails or the conversation does not exist.
pub async fn lock_in_tx(tx: &mut DbTransaction<'_>, id: &str) -> DbResult<Conversation> {
    if DatabaseBackend::from_connection(tx.as_mut()) == DatabaseBackend::Postgres {
        return sqlx::query_as::<_, Conversation>("SELECT * FROM conversations WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut **tx)
            .await;
    }

    let result = sqlx::query("UPDATE conversations SET created_at = created_at WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    sqlx::query_as::<_, Conversation>("SELECT * FROM conversations WHERE id = $1")
        .bind(id)
        .fetch_one(&mut **tx)
        .await
}

/// Point the conversation at the response that committed its latest turn.
///
/// # Errors
/// Returns `DbResult::Err` if the database update fails or the conversation does not exist.
pub async fn set_latest_response_in_tx(tx: &mut DbTransaction<'_>, id: &str, response_id: &str) -> DbResult<()> {
    let result = sqlx::query("UPDATE conversations SET latest_response_id = $1 WHERE id = $2")
        .bind(response_id)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

/// Create a conversation in the same transaction as its initial items.
///
/// # Errors
/// Returns an error if insertion fails.
pub async fn create_in_tx(
    tx: &mut DbTransaction<'_>,
    id: &str,
    tenant_id: Option<&str>,
    metadata: Option<&str>,
) -> DbResult<Conversation> {
    sqlx::query_as::<_, Conversation>(
        "INSERT INTO conversations (id, tenant_id, metadata, created_at) VALUES ($1, $2, $3, $4) RETURNING *",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(metadata)
    .bind(utcnow_str())
    .fetch_one(&mut **tx)
    .await
}

/// Get a conversation owned by the supplied tenant.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get_by_tenant(pool: &DbPool, tenant_id: &str, conversation_id: &str) -> DbResult<Option<Conversation>> {
    sqlx::query_as("SELECT * FROM conversations WHERE id = $1 AND tenant_id = $2")
        .bind(conversation_id)
        .bind(tenant_id)
        .fetch_optional(pool)
        .await
}

/// Replace conversation metadata.
///
/// # Errors
/// Returns an error if the query fails or the conversation is not owned by the tenant.
pub async fn update_metadata(
    pool: &DbPool,
    tenant_id: &str,
    conversation_id: &str,
    metadata: &str,
) -> DbResult<Conversation> {
    sqlx::query_as("UPDATE conversations SET metadata = $1 WHERE id = $2 AND tenant_id = $3 RETURNING *")
        .bind(metadata)
        .bind(conversation_id)
        .bind(tenant_id)
        .fetch_one(pool)
        .await
}

/// Delete a locked conversation after its items have been detached.
///
/// # Errors
/// Returns an error if deletion fails.
pub async fn delete_in_tx(tx: &mut DbTransaction<'_>, conversation_id: &str) -> DbResult<()> {
    sqlx::query("DELETE FROM conversations WHERE id = $1")
        .bind(conversation_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Advance the item revision while holding the conversation lock.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn bump_revision_in_tx(tx: &mut DbTransaction<'_>, id: &str) -> DbResult<()> {
    sqlx::query("UPDATE conversations SET revision = revision + 1 WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
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
            latest_response_id: None,
            tenant_id: None,
            revision: 0,
        };

        assert_eq!(conversation.id, "conv_1");
        assert!(conversation.metadata.is_none());
        assert_eq!(conversation.created_at, 1_704_067_200);
    }
}
