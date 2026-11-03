//! Atomic attachment metadata publication for the SQLite vector provider.

use sqlx::SqliteConnection;

use super::{PreparedAttachment, validate_capacity};
use crate::types::file_search::FileSearchError;

/// Publishes metadata on the same SQLite connection as a provider-specific
/// index. Keeping both writes in one transaction fences concurrent replicas.
pub(crate) async fn publish_attachment_sqlite(
    connection: &mut SqliteConnection,
    store_id: &str,
    identity: &str,
    attachment: &PreparedAttachment,
    chunks: &[String],
    storage_bytes: i64,
) -> Result<(), FileSearchError> {
    let changed = sqlx::query("UPDATE file_search_stores SET embedding_dimensions = $1 WHERE id = $2 AND embedding_identity = $3 AND (embedding_dimensions = 0 OR embedding_dimensions = $1)")
        .bind(attachment.dimensions).bind(store_id).bind(identity).execute(&mut *connection).await?.rows_affected();
    if changed != 1 {
        return Err(FileSearchError::Conflict(
            "Vector store embedding configuration changed or the vector store was deleted".into(),
        ));
    }
    let bytes: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(storage_bytes), 0) AS BIGINT) FROM file_search_attachments WHERE store_id = $1",
    )
    .bind(store_id)
    .fetch_one(&mut *connection)
    .await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE store_id = $1")
        .bind(store_id)
        .fetch_one(&mut *connection)
        .await?;
    validate_capacity(
        bytes.saturating_add(storage_bytes),
        count.saturating_add(i64::try_from(chunks.len()).unwrap_or(i64::MAX)),
    )?;
    let object = &attachment.object;
    let inserted = sqlx::query("INSERT INTO file_search_attachments (store_id, file_id, created_at, usage_bytes, data, storage_bytes) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(store_id).bind(&object.id).bind(object.created_at).bind(object.usage_bytes).bind(serde_json::to_string(object)?)
        .bind(storage_bytes)
        .execute(&mut *connection).await;
    if let Err(sqlx::Error::Database(error)) = &inserted {
        if error.is_unique_violation() {
            return Err(FileSearchError::Conflict(
                "File is already attached to this vector store".into(),
            ));
        }
        if error.is_foreign_key_violation() {
            return Err(FileSearchError::NotFound("File was deleted during ingestion".into()));
        }
    }
    inserted?;
    for (chunk, data) in attachment.chunks.iter().zip(chunks) {
        let index =
            i64::try_from(chunk.chunk_index).map_err(|_| FileSearchError::InvalidRequest("Too many chunks".into()))?;
        sqlx::query("INSERT INTO file_search_chunks (store_id, file_id, chunk_index, data) VALUES ($1, $2, $3, $4)")
            .bind(store_id)
            .bind(&object.id)
            .bind(index)
            .bind(data)
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}
