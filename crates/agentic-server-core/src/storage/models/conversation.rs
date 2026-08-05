//! Conversation context and history.

#![allow(clippy::missing_errors_doc)]

use super::super::pool::{DbPool, DbResult, DbTransaction};
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

    pub tenant_id: Option<String>,

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
    create_with_metadata(pool, id, None, None).await
}

pub async fn create_with_metadata(
    pool: &DbPool,
    id: &str,
    tenant_id: Option<&str>,
    metadata: Option<&str>,
) -> DbResult<Conversation> {
    let mut tx = pool.begin().await?;
    let conversation = create_with_metadata_in_tx(&mut tx, id, tenant_id, metadata).await?;
    tx.commit().await?;
    Ok(conversation)
}

pub async fn create_with_metadata_in_tx(
    tx: &mut DbTransaction<'_>,
    id: &str,
    tenant_id: Option<&str>,
    metadata: Option<&str>,
) -> DbResult<Conversation> {
    let now = utcnow_str();
    sqlx::query_as::<_, Conversation>(
        "INSERT INTO conversations (id, created_at, tenant_id, metadata) \
         VALUES ($1, $2, $3, $4) RETURNING *",
    )
    .bind(id)
    .bind(now)
    .bind(tenant_id)
    .bind(metadata)
    .fetch_one(&mut **tx)
    .await
}

/// Get or create a conversation.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_or_create(pool: &DbPool, id: &str) -> DbResult<Conversation> {
    get_or_create_for_tenant(pool, id, None).await
}

pub async fn get_or_create_for_tenant(pool: &DbPool, id: &str, tenant_id: Option<&str>) -> DbResult<Conversation> {
    let now = utcnow_str();
    if let Some(tenant_id) = tenant_id {
        sqlx::query_as::<_, Conversation>(
            "INSERT INTO conversations (id, created_at, tenant_id) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET created_at = conversations.created_at \
             WHERE conversations.tenant_id = $3 \
             RETURNING *",
        )
        .bind(id)
        .bind(now)
        .bind(tenant_id)
        .fetch_one(pool)
        .await
    } else {
        sqlx::query_as::<_, Conversation>(
            "INSERT INTO conversations (id, created_at, tenant_id) \
             VALUES ($1, $2, NULL) \
             ON CONFLICT (id) DO UPDATE SET created_at = conversations.created_at \
             WHERE conversations.tenant_id IS NULL \
             RETURNING *",
        )
        .bind(id)
        .bind(now)
        .fetch_one(pool)
        .await
    }
}

/// Get a conversation by ID.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get(pool: &DbPool, id: &str) -> DbResult<Option<Conversation>> {
    get_for_tenant(pool, id, None).await
}

pub async fn get_for_tenant(pool: &DbPool, id: &str, tenant_id: Option<&str>) -> DbResult<Option<Conversation>> {
    match tenant_id {
        Some(tenant_id) => {
            sqlx::query_as::<_, Conversation>("SELECT * FROM conversations WHERE id = $1 AND tenant_id = $2")
                .bind(id)
                .bind(tenant_id)
                .fetch_optional(pool)
                .await
        }
        None => {
            sqlx::query_as::<_, Conversation>("SELECT * FROM conversations WHERE id = $1 AND tenant_id IS NULL")
                .bind(id)
                .fetch_optional(pool)
                .await
        }
    }
}

pub async fn update_metadata(
    pool: &DbPool,
    id: &str,
    tenant_id: Option<&str>,
    metadata: Option<&str>,
) -> DbResult<Option<Conversation>> {
    match tenant_id {
        Some(tenant_id) => {
            sqlx::query_as::<_, Conversation>(
                "UPDATE conversations SET metadata = $3 \
                 WHERE id = $1 AND tenant_id = $2 RETURNING *",
            )
            .bind(id)
            .bind(tenant_id)
            .bind(metadata)
            .fetch_optional(pool)
            .await
        }
        None => {
            sqlx::query_as::<_, Conversation>(
                "UPDATE conversations SET metadata = $2 \
                 WHERE id = $1 AND tenant_id IS NULL RETURNING *",
            )
            .bind(id)
            .bind(metadata)
            .fetch_optional(pool)
            .await
        }
    }
}

pub async fn delete(pool: &DbPool, id: &str, tenant_id: Option<&str>) -> DbResult<bool> {
    let mut tx = pool.begin().await?;
    match tenant_id {
        Some(tenant_id) => {
            sqlx::query(
                "UPDATE items SET conversation_id = NULL, seq = NULL \
                 WHERE conversation_id = $1 AND tenant_id = $2",
            )
            .bind(id)
            .bind(tenant_id)
            .execute(&mut *tx)
            .await?;
        }
        None => {
            sqlx::query(
                "UPDATE items SET conversation_id = NULL, seq = NULL \
                 WHERE conversation_id = $1 AND tenant_id IS NULL",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
    }
    let result = match tenant_id {
        Some(tenant_id) => {
            sqlx::query("DELETE FROM conversations WHERE id = $1 AND tenant_id = $2")
                .bind(id)
                .bind(tenant_id)
                .execute(&mut *tx)
                .await?
        }
        None => {
            sqlx::query("DELETE FROM conversations WHERE id = $1 AND tenant_id IS NULL")
                .bind(id)
                .execute(&mut *tx)
                .await?
        }
    };
    let deleted = result.rows_affected() > 0;
    if deleted {
        tx.commit().await?;
    }
    Ok(deleted)
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
pub async fn lock_in_tx(tx: &mut DbTransaction<'_>, id: &str) -> DbResult<()> {
    lock_in_tx_for_tenant(tx, id, None).await
}

pub async fn lock_in_tx_for_tenant(tx: &mut DbTransaction<'_>, id: &str, tenant_id: Option<&str>) -> DbResult<()> {
    if DatabaseBackend::from_connection(tx.as_mut()) == DatabaseBackend::Postgres {
        let locked_id = match tenant_id {
            Some(tenant_id) => {
                sqlx::query_scalar::<_, String>(
                    "SELECT id FROM conversations WHERE id = $1 AND tenant_id = $2 FOR UPDATE",
                )
                .bind(id)
                .bind(tenant_id)
                .fetch_optional(&mut **tx)
                .await?
            }
            None => {
                sqlx::query_scalar::<_, String>(
                    "SELECT id FROM conversations WHERE id = $1 AND tenant_id IS NULL FOR UPDATE",
                )
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?
            }
        };
        return locked_id.map(|_| ()).ok_or(sqlx::Error::RowNotFound);
    }

    let result = match tenant_id {
        Some(tenant_id) => {
            sqlx::query("UPDATE conversations SET created_at = created_at WHERE id = $1 AND tenant_id = $2")
                .bind(id)
                .bind(tenant_id)
                .execute(&mut **tx)
                .await?
        }
        None => {
            sqlx::query("UPDATE conversations SET created_at = created_at WHERE id = $1 AND tenant_id IS NULL")
                .bind(id)
                .execute(&mut **tx)
                .await?
        }
    };
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conversation_basic() {
        let conversation = Conversation {
            id: "conv_1".to_string(),
            tenant_id: None,
            metadata: None,
            created_at: 1_704_067_200,
        };

        assert_eq!(conversation.id, "conv_1");
        assert!(conversation.metadata.is_none());
        assert_eq!(conversation.created_at, 1_704_067_200);
    }
}
