//! Store lifecycle transactions share the publication store guard.
use super::{DbTransaction, FileSearchStorage, StoredChunk, database_now};
use crate::types::file_search::{
    AttachmentStatus, FileAttributes, FileCounts, FileSearchError, UpdateVectorStoreRequest,
    VectorStoreExpirationAnchor, VectorStoreExpiresAfter, VectorStoreFileObject, VectorStoreObject, VectorStoreStatus,
};

#[derive(sqlx::FromRow)]
struct StoreLifecycle {
    data: String,
    last_active_at: Option<i64>,
    expires_after_days: Option<i64>,
    expires_at: Option<i64>,
    lifecycle_status: String,
}

impl StoreLifecycle {
    fn expired(&self, now: i64) -> bool {
        self.lifecycle_status == "expired" || self.expires_at.is_some_and(|deadline| deadline <= now)
    }

    fn object(&self, now: i64) -> Result<VectorStoreObject, FileSearchError> {
        let mut object: VectorStoreObject = serde_json::from_str(&self.data)?;
        object.last_active_at = self.last_active_at;
        object.expires_at = self.expires_at;
        object.expires_after = self
            .expires_after_days
            .map(|days| {
                u16::try_from(days)
                    .map(|days| VectorStoreExpiresAfter {
                        anchor: VectorStoreExpirationAnchor::LastActiveAt,
                        days,
                    })
                    .map_err(|_| FileSearchError::Unavailable("Stored expiration policy is invalid".into()))
            })
            .transpose()?;
        if self.expired(now) {
            object.status = VectorStoreStatus::Expired;
        }
        Ok(object)
    }
}

pub(super) async fn lock_store(tx: &mut DbTransaction<'_>, id: &str) -> Result<(), FileSearchError> {
    if sqlx::query("UPDATE file_search_stores SET id = id WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?
        .rows_affected()
        != 1
    {
        return Err(FileSearchError::NotFound("Vector store not found".into()));
    }
    Ok(())
}

pub(super) async fn require_live_store(tx: &mut DbTransaction<'_>, id: &str, now: i64) -> Result<(), FileSearchError> {
    let live: Option<String> = sqlx::query_scalar("SELECT id FROM file_search_stores WHERE id = $1 AND lifecycle_status != 'expired' AND (expires_at IS NULL OR expires_at > $2)")
        .bind(id).bind(now).fetch_optional(&mut **tx).await?;
    if live.is_none() {
        return Err(FileSearchError::NotFound("Vector store not found or expired".into()));
    }
    Ok(())
}

pub(super) async fn touch_store(tx: &mut DbTransaction<'_>, id: &str, now: i64) -> Result<(), FileSearchError> {
    sqlx::query(
        "UPDATE file_search_stores SET last_active_at = $2, expires_at = $2 + expires_after_days * 86400 WHERE id = $1",
    )
    .bind(id)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl FileSearchStorage {
    pub(crate) async fn store_object(&self, id: &str) -> Result<VectorStoreObject, FileSearchError> {
        let mut connection = self.pool.acquire().await?;
        let row: StoreLifecycle = sqlx::query_as("SELECT data, last_active_at, expires_after_days, expires_at, lifecycle_status FROM file_search_stores WHERE id = $1")
            .bind(id).fetch_optional(&mut *connection).await?.ok_or_else(|| FileSearchError::NotFound("Vector store not found".into()))?;
        let now = database_now(&mut connection).await?;
        let mut object = row.object(now)?;
        object.file_counts = FileCounts::default();
        object.usage_bytes = 0;
        if object.status != VectorStoreStatus::Expired {
            let counts: Vec<(String, i64, i64)> = sqlx::query_as("SELECT status, COUNT(*), CAST(COALESCE(SUM(usage_bytes), 0) AS BIGINT) FROM file_search_attachments WHERE store_id = $1 AND file_id IN (SELECT id FROM file_search_files WHERE expires_at IS NULL OR expires_at > $2) GROUP BY status")
                .bind(id).bind(now).fetch_all(&mut *connection).await?;
            for (status, count, bytes) in counts {
                match status.as_str() {
                    "in_progress" => object.file_counts.in_progress = count,
                    "completed" => object.file_counts.completed = count,
                    "failed" => object.file_counts.failed = count,
                    "cancelled" => object.file_counts.cancelled = count,
                    _ => {
                        return Err(FileSearchError::Unavailable(
                            "Stored attachment status is invalid".into(),
                        ));
                    }
                }
                object.file_counts.total += count;
                object.usage_bytes += bytes;
            }
            object.status = if object.file_counts.in_progress > 0 {
                VectorStoreStatus::InProgress
            } else {
                VectorStoreStatus::Completed
            };
        }
        Ok(object)
    }

    pub(crate) async fn update_store(
        &self,
        id: &str,
        request: UpdateVectorStoreRequest,
    ) -> Result<(), FileSearchError> {
        let mut tx = self.pool.begin().await?;
        lock_store(&mut tx, id).await?;
        let now = database_now(&mut tx).await?;
        require_live_store(&mut tx, id, now).await?;
        let row: StoreLifecycle = sqlx::query_as("SELECT data, last_active_at, expires_after_days, expires_at, lifecycle_status FROM file_search_stores WHERE id = $1")
            .bind(id).fetch_one(&mut *tx).await?;
        let mut object = row.object(now)?;
        if let Some(name) = request.name.0 {
            object.name = name.unwrap_or_default();
        }
        if let Some(metadata) = request.metadata.0 {
            object.metadata = metadata;
        }
        if let Some(policy) = request.expires_after.0 {
            object.expires_after = policy;
            object.expires_at = object.expires_after.as_ref().map(|policy| {
                object
                    .last_active_at
                    .unwrap_or(object.created_at)
                    .saturating_add(i64::from(policy.days) * 86400)
            });
        }
        sqlx::query("UPDATE file_search_stores SET data = $2, expires_after_days = $3, expires_at = $4 WHERE id = $1")
            .bind(id)
            .bind(serde_json::to_string(&object)?)
            .bind(object.expires_after.as_ref().map(|policy| i64::from(policy.days)))
            .bind(object.expires_at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn refresh_activity(&self, ids: &[String]) -> Result<(), FileSearchError> {
        let mut ids = ids.iter().collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        let mut tx = self.pool.begin().await?;
        for id in &ids {
            lock_store(&mut tx, id).await?;
        }
        let now = database_now(&mut tx).await?;
        for id in ids {
            require_live_store(&mut tx, id, now).await?;
            touch_store(&mut tx, id, now).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn expire_stores(&self, limit: usize) -> Result<usize, FileSearchError> {
        // Scan outside each write transaction so SQLite never upgrades a stale read snapshot.
        let visibility = self.file_visibility().await?;
        let ids: Vec<String> = sqlx::query_scalar(&format!("SELECT id FROM file_search_stores WHERE lifecycle_status != 'expired' AND NOT {visibility} ORDER BY expires_at, id LIMIT $1"))
            .bind(i64::try_from(limit).unwrap_or(1000)).fetch_all(self.pool.as_ref()).await?;
        let mut expired = 0;
        for id in ids {
            let mut tx = self.pool.begin().await?;
            match lock_store(&mut tx, &id).await {
                Ok(()) => {}
                Err(FileSearchError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            }
            let now = database_now(&mut tx).await?;
            let changed = sqlx::query("UPDATE file_search_stores SET lifecycle_status = 'expired' WHERE id = $1 AND lifecycle_status != 'expired' AND expires_at <= $2")
                .bind(&id).bind(now).execute(&mut *tx).await?.rows_affected();
            if changed == 1 {
                sqlx::query("DELETE FROM file_search_attachments WHERE store_id = $1")
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                expired += 1;
            }
            tx.commit().await?;
        }
        Ok(expired)
    }

    pub(crate) async fn update_attachment(
        &self,
        store_id: &str,
        file_id: &str,
        attributes: FileAttributes,
    ) -> Result<VectorStoreFileObject, FileSearchError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE file_search_files SET id = id WHERE id = $1")
            .bind(file_id)
            .execute(&mut *tx)
            .await?;
        lock_store(&mut tx, store_id).await?;
        let now = database_now(&mut tx).await?;
        require_live_store(&mut tx, store_id, now).await?;
        let data: String = sqlx::query_scalar("SELECT data FROM file_search_attachments WHERE store_id = $1 AND file_id = $2 AND file_id IN (SELECT id FROM file_search_files WHERE expires_at IS NULL OR expires_at > $3)")
            .bind(store_id).bind(file_id).bind(now).fetch_optional(&mut *tx).await?.ok_or_else(|| FileSearchError::NotFound("Vector store file not found".into()))?;
        let mut object: VectorStoreFileObject = serde_json::from_str(&data)?;
        object.attributes = attributes;
        sqlx::query("UPDATE file_search_attachments SET data = $3 WHERE store_id = $1 AND file_id = $2")
            .bind(store_id)
            .bind(file_id)
            .bind(serde_json::to_string(&object)?)
            .execute(&mut *tx)
            .await?;
        // Bounded by the store's existing serialized corpus budget; preserve every other chunk field.
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT chunk_index, data FROM file_search_chunks WHERE store_id = $1 AND file_id = $2")
                .bind(store_id)
                .bind(file_id)
                .fetch_all(&mut *tx)
                .await?;
        let mut bytes = 0i64;
        let mut chunks = Vec::with_capacity(rows.len());
        for (index, data) in rows {
            let mut chunk: StoredChunk = serde_json::from_str(&data)?;
            chunk.attributes.clone_from(&object.attributes);
            let data = serde_json::to_string(&chunk)?;
            bytes = bytes.saturating_add(i64::try_from(data.len()).unwrap_or(i64::MAX));
            chunks.push((index, data));
            if chunks.len() % 32 == 0 {
                tokio::task::yield_now().await;
            }
        }
        let other_bytes: i64 = sqlx::query_scalar("SELECT CAST(COALESCE(SUM(storage_bytes), 0) AS BIGINT) FROM file_search_attachments WHERE store_id = $1 AND file_id != $2")
            .bind(store_id).bind(file_id).fetch_one(&mut *tx).await?;
        super::validate_capacity(other_bytes.saturating_add(bytes), 0)?;
        for (index, data) in chunks {
            sqlx::query(
                "UPDATE file_search_chunks SET data = $4 WHERE store_id = $1 AND file_id = $2 AND chunk_index = $3",
            )
            .bind(store_id)
            .bind(file_id)
            .bind(index)
            .bind(data)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE file_search_attachments SET storage_bytes = $3 WHERE store_id = $1 AND file_id = $2")
            .bind(store_id)
            .bind(file_id)
            .bind(bytes)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(object)
    }

    pub(crate) async fn parsed_content(
        &self,
        store_id: &str,
        file_id: &str,
    ) -> Result<Option<String>, FileSearchError> {
        let row: Option<Option<String>> = sqlx::query_scalar(
            "SELECT parsed_content FROM file_search_attachments WHERE store_id = $1 AND file_id = $2 AND status = $3",
        )
        .bind(store_id)
        .bind(file_id)
        .bind(AttachmentStatus::Completed.as_str())
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.ok_or_else(|| FileSearchError::NotFound("Completed vector store file not found".into()))
    }
}
