//! Conversation context and history.

use sqlx::{FromRow, Row};

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

/// Get or create a conversation.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_or_create(pool: &DbPool, id: &str) -> DbResult<Conversation> {
    let now = utcnow_str();
    // The no-op update makes `RETURNING` yield the existing row. PostgreSQL rejects an
    // unqualified `created_at` here as ambiguous with `EXCLUDED.created_at`.
    sqlx::query_as::<_, Conversation>(
        "INSERT INTO conversations (id, created_at) \
         VALUES ($1, $2) \
         ON CONFLICT (id) DO UPDATE SET created_at = conversations.created_at \
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

/// The latest turn of a conversation: the committing response's ID and its `metadata` column.
pub type LatestTurn = (String, Option<String>);

/// Get what rehydration needs from a conversation in one statement: its `revision`, its
/// `latest_response_id`, the latest turn when that response belongs to this conversation, and its
/// items in sequence order.
///
/// Reading the header and the items in one statement keeps the conversation version consistent with
/// the items. Items are ordered here rather than in SQL so the database does not sort item data.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_with_items(
    pool: &DbPool,
    id: &str,
) -> DbResult<Option<(i64, Option<String>, Option<LatestTurn>, Vec<Item>)>> {
    let rows = sqlx::query(
        "SELECT conversations.revision, conversations.latest_response_id, \
                responses.id AS turn_id, responses.metadata, \
                NULL AS id, NULL AS reference_id, NULL AS public_id, NULL AS data, \
                NULL AS created_at, NULL AS conversation_id, NULL AS seq, NULL AS tenant_id \
         FROM conversations \
         LEFT JOIN responses ON responses.id = conversations.latest_response_id \
             AND responses.conversation_id = conversations.id \
         WHERE conversations.id = $1 \
         UNION ALL \
         SELECT NULL, NULL, NULL, NULL, \
                items.id, items.reference_id, items.public_id, items.data, \
                items.created_at, items.conversation_id, items.seq, items.tenant_id \
         FROM items WHERE items.conversation_id = $1",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let mut header = None;
    let mut items = Vec::new();
    for row in &rows {
        // Only the conversation row has a revision.
        match row.try_get::<Option<i64>, _>("revision")? {
            Some(revision) => {
                let turn = row
                    .try_get::<Option<String>, _>("turn_id")?
                    .map(|turn_id| Ok::<_, sqlx::Error>((turn_id, row.try_get("metadata")?)))
                    .transpose()?;
                header = Some((revision, row.try_get("latest_response_id")?, turn));
            }
            None => items.push(Item::from_row(row)?),
        }
    }
    let Some((revision, latest_response_id, turn)) = header else {
        return Ok(None);
    };
    items.sort_unstable_by_key(|item| item.seq);
    Ok(Some((revision, latest_response_id, turn, items)))
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
