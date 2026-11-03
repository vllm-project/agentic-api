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
        let mut tx = self.index.begin().await?;
        sqlx::query(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {table} USING vec0(embedding float[{dimensions}] distance_metric=cosine)"
        ))
        .execute(&mut *tx)
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
                let row_id: i64 = sqlx::query_scalar(
                    "INSERT INTO file_search_vec_map (store_id, file_id, chunk_index, created_at) \
                     VALUES ($1, $2, $3, $4) RETURNING id",
                )
                .bind(store_id)
                .bind(&attachment.object.id)
                .bind(chunk_index)
                .bind(chrono::Utc::now().timestamp())
                .fetch_one(&mut *tx)
                .await?;
                sqlx::query(&format!("INSERT INTO {table} (rowid, embedding) VALUES ($1, $2)"))
                    .bind(row_id)
                    .bind(encoded)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub(super) async fn remove_index_rows(
        &self,
        collection: Collection,
        id: &str,
        store_id: Option<&str>,
    ) -> Result<(), FileSearchError> {
        let mut tx = self.index.begin().await?;
        let rows: Vec<(i64, String)> = match collection {
            Collection::Files => {
                sqlx::query_as("SELECT id, store_id FROM file_search_vec_map WHERE file_id = $1")
                    .bind(id)
                    .fetch_all(&mut *tx)
                    .await?
            }
            Collection::Stores => {
                sqlx::query_as("SELECT id, store_id FROM file_search_vec_map WHERE store_id = $1")
                    .bind(id)
                    .fetch_all(&mut *tx)
                    .await?
            }
            Collection::Attachments => {
                sqlx::query_as("SELECT id, store_id FROM file_search_vec_map WHERE file_id = $1 AND store_id = $2")
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
            sqlx::query(&format!("DROP TABLE IF EXISTS {}", table_name(id)?))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(super) async fn recover_orphans(&self) -> Result<(), FileSearchError> {
        // A process can stop after index staging and before metadata commits.
        // Ignore recent rows so another replica's in-flight publication is safe.
        let cutoff = chrono::Utc::now().timestamp() - 24 * 60 * 60;
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT indexed.id, indexed.store_id FROM file_search_vec_map AS indexed \
             LEFT JOIN file_search_chunks AS chunks ON chunks.store_id = indexed.store_id \
               AND chunks.file_id = indexed.file_id AND chunks.chunk_index = indexed.chunk_index \
             WHERE chunks.store_id IS NULL AND indexed.created_at < $1",
        )
        .bind(cutoff)
        .fetch_all(&self.index)
        .await?;
        let mut tx = self.index.begin().await?;
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

#[expect(
    clippy::cast_possible_truncation,
    reason = "unit vectors are deliberately stored as sqlite-vec f32"
)]
pub(super) fn normalized_vector(vector: &[f64]) -> Result<String, FileSearchError> {
    if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
        return Err(FileSearchError::ProviderProtocol);
    }
    let scale = vector.iter().map(|value| value.abs()).fold(0.0_f64, f64::max);
    let normalized = if scale == 0.0 {
        vec![0.0_f32; vector.len()]
    } else {
        let norm = vector.iter().map(|value| (value / scale).powi(2)).sum::<f64>().sqrt();
        vector.iter().map(|value| (value / scale / norm) as f32).collect()
    };
    Ok(serde_json::to_string(&normalized)?)
}
