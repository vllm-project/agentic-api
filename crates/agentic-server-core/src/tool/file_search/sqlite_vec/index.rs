//! Transactional sqlite-vec publication and cleanup.

use std::collections::HashMap;

use super::SqliteVecRepository;
use crate::{
    storage::file_search::{Collection, PreparedAttachment},
    types::file_search::FileSearchError,
};

impl SqliteVecRepository {
    pub(super) async fn stage(
        &self,
        connection: &mut sqlx::SqliteConnection,
        store_id: &str,
        attachments: &[PreparedAttachment],
    ) -> Result<(), FileSearchError> {
        let Some(dimensions) = attachments
            .iter()
            .find_map(|attachment| (attachment.dimensions > 0).then_some(attachment.dimensions))
        else {
            return Ok(());
        };
        if attachments.iter().any(|attachment| attachment.dimensions != dimensions) {
            return Err(FileSearchError::Conflict(
                "Vector store attachments have incompatible embedding dimensions".into(),
            ));
        }
        let dimensions = usize::try_from(dimensions).map_err(|_| FileSearchError::ProviderProtocol)?;
        if !(1..=4096).contains(&dimensions) {
            return Err(FileSearchError::InvalidRequest(
                "sqlite-vec supports embedding dimensions from 1 to 4096".into(),
            ));
        }
        let table = table_name(store_id)?;
        sqlx::query(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {table} USING vec0(embedding float[{dimensions}] distance_metric=cosine)"
        ))
        .execute(&mut *connection)
        .await?;
        for attachment in attachments {
            for chunk in &attachment.chunks {
                let vector = chunk.embedding.as_ref().ok_or_else(|| {
                    FileSearchError::InvalidRequest("A semantic chunk is missing its embedding".into())
                })?;
                if vector.len() != dimensions {
                    return Err(FileSearchError::Conflict(
                        "Vector store attachments have incompatible embedding dimensions".into(),
                    ));
                }
                let encoded = normalized_vector(vector)?;
                let chunk_index = i64::try_from(chunk.chunk_index)
                    .map_err(|_| FileSearchError::InvalidRequest("Too many chunks".into()))?;
                // Reuse a staged row left by an interrupted publication, but never
                // replace an index entry whose metadata has already been published.
                let row_id: i64 = sqlx::query_scalar(
                    "INSERT INTO file_search_vec_map (store_id, file_id, chunk_index, created_at) \
                     VALUES ($1, $2, $3, $4) \
                     ON CONFLICT(store_id, file_id, chunk_index) DO UPDATE SET created_at = excluded.created_at \
                     WHERE NOT EXISTS (SELECT 1 FROM file_search_chunks AS published \
                       WHERE published.store_id = file_search_vec_map.store_id \
                         AND published.file_id = file_search_vec_map.file_id \
                         AND published.chunk_index = file_search_vec_map.chunk_index) \
                     RETURNING id",
                )
                .bind(store_id)
                .bind(&attachment.object.id)
                .bind(chunk_index)
                .bind(chrono::Utc::now().timestamp())
                .fetch_optional(&mut *connection)
                .await?
                .ok_or_else(|| FileSearchError::Conflict("File is already indexed in this vector store".into()))?;
                sqlx::query(&format!("DELETE FROM {table} WHERE rowid = $1"))
                    .bind(row_id)
                    .execute(&mut *connection)
                    .await?;
                sqlx::query(&format!("INSERT INTO {table} (rowid, embedding) VALUES ($1, $2)"))
                    .bind(row_id)
                    .bind(encoded)
                    .execute(&mut *connection)
                    .await?;
            }
        }
        Ok(())
    }

    pub(super) async fn remove_index_rows(
        &self,
        collection: Collection,
        id: &str,
        store_id: Option<&str>,
    ) -> Result<(), FileSearchError> {
        let mut tx = self.index.begin_with("BEGIN IMMEDIATE").await?;
        let rows: Vec<(i64, String)> = match collection {
            Collection::Files => {
                sqlx::query_as(
                    "SELECT indexed.id, indexed.store_id FROM file_search_vec_map AS indexed \
                     WHERE indexed.file_id = $1 AND NOT EXISTS \
                     (SELECT 1 FROM file_search_chunks AS chunks WHERE chunks.store_id = indexed.store_id \
                       AND chunks.file_id = indexed.file_id AND chunks.chunk_index = indexed.chunk_index)",
                )
                .bind(id)
                .fetch_all(&mut *tx)
                .await?
            }
            Collection::Stores => {
                sqlx::query_as(
                    "SELECT indexed.id, indexed.store_id FROM file_search_vec_map AS indexed \
                     WHERE indexed.store_id = $1 AND NOT EXISTS \
                     (SELECT 1 FROM file_search_chunks AS chunks WHERE chunks.store_id = indexed.store_id \
                       AND chunks.file_id = indexed.file_id AND chunks.chunk_index = indexed.chunk_index)",
                )
                .bind(id)
                .fetch_all(&mut *tx)
                .await?
            }
            Collection::Attachments => {
                sqlx::query_as(
                    "SELECT indexed.id, indexed.store_id FROM file_search_vec_map AS indexed \
                     WHERE indexed.file_id = $1 AND indexed.store_id = $2 AND NOT EXISTS \
                     (SELECT 1 FROM file_search_chunks AS chunks WHERE chunks.store_id = indexed.store_id \
                       AND chunks.file_id = indexed.file_id AND chunks.chunk_index = indexed.chunk_index)",
                )
                .bind(id)
                .bind(store_id.ok_or_else(|| FileSearchError::InvalidRequest("Missing vector store ID".into()))?)
                .fetch_all(&mut *tx)
                .await?
            }
        };
        let mut tables = HashMap::new();
        for (row_id, row_store_id) in rows {
            let table = table_name(&row_store_id)?;
            let exists = if let Some(exists) = tables.get(&table) {
                *exists
            } else {
                let exists: Option<String> =
                    sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = $1")
                        .bind(&table)
                        .fetch_optional(&mut *tx)
                        .await?;
                tables.insert(table.clone(), exists.is_some());
                exists.is_some()
            };
            if exists {
                sqlx::query(&format!("DELETE FROM {table} WHERE rowid = $1"))
                    .bind(row_id)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("DELETE FROM file_search_vec_map WHERE id = $1")
                .bind(row_id)
                .execute(&mut *tx)
                .await?;
        }
        if matches!(collection, Collection::Stores) {
            let live: Option<i64> = sqlx::query_scalar("SELECT 1 FROM file_search_stores WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
            if live.is_none() {
                sqlx::query(&format!("DROP TABLE IF EXISTS {}", table_name(id)?))
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub(super) async fn recover_orphans(&self) -> Result<(), FileSearchError> {
        // Publication and index staging share a transaction. BEGIN IMMEDIATE
        // excludes concurrent writers while orphan candidates are selected.
        let mut tx = self.index.begin_with("BEGIN IMMEDIATE").await?;
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT indexed.id, indexed.store_id FROM file_search_vec_map AS indexed \
             LEFT JOIN file_search_chunks AS chunks ON chunks.store_id = indexed.store_id \
               AND chunks.file_id = indexed.file_id AND chunks.chunk_index = indexed.chunk_index \
             WHERE chunks.store_id IS NULL",
        )
        .fetch_all(&mut *tx)
        .await?;
        for (row_id, store_id) in rows {
            let table = table_name(&store_id)?;
            let exists: Option<String> =
                sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = $1")
                    .bind(&table)
                    .fetch_optional(&mut *tx)
                    .await?;
            if exists.is_some() {
                sqlx::query(&format!("DELETE FROM {table} WHERE rowid = $1"))
                    .bind(row_id)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("DELETE FROM file_search_vec_map WHERE id = $1")
                .bind(row_id)
                .execute(&mut *tx)
                .await?;
        }
        // A failed store deletion can leave an empty vec0 table after all map
        // rows have been removed. Reclaim only names generated from store UUIDs.
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'file_search_vec_%'")
                .fetch_all(&mut *tx)
                .await?;
        for table in tables {
            let Some(suffix) = table.strip_prefix("file_search_vec_") else {
                continue;
            };
            let Ok(uuid) = uuid::Uuid::parse_str(suffix) else {
                continue;
            };
            let store_id = format!("vs_{uuid}");
            if table_name(&store_id)? != table {
                continue;
            }
            let live: Option<i64> = sqlx::query_scalar("SELECT 1 FROM file_search_stores WHERE id = $1")
                .bind(&store_id)
                .fetch_optional(&mut *tx)
                .await?;
            if live.is_none() {
                sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }
}

pub(super) fn table_name(store_id: &str) -> Result<String, FileSearchError> {
    let id = store_id
        .strip_prefix("vs_")
        .ok_or_else(|| FileSearchError::InvalidRequest("Invalid vector store ID".into()))?;
    let id =
        uuid::Uuid::parse_str(id).map_err(|_| FileSearchError::InvalidRequest("Invalid vector store ID".into()))?;
    Ok(format!("file_search_vec_{}", id.simple()))
}

pub(super) fn normalized_vector(vector: &[f64]) -> Result<String, FileSearchError> {
    if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
        return Err(FileSearchError::ProviderProtocol);
    }
    let scale = vector.iter().map(|value| value.abs()).fold(0.0_f64, f64::max);
    let normalized = if scale == 0.0 {
        vec![0.0_f64; vector.len()]
    } else {
        let norm = vector.iter().map(|value| (value / scale).powi(2)).sum::<f64>().sqrt();
        // sqlite-vec converts the JSON numbers to its f32 storage format.
        // Keep the normalized values as f64 here to avoid an unchecked cast.
        vector.iter().map(|value| value / scale / norm).collect()
    };
    Ok(serde_json::to_string(&normalized)?)
}
