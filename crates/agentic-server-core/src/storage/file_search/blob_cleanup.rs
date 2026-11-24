//! Durable intent records for cross-store blob operations.

use sqlx::Acquire;

use super::{Collection, FileSearchError, FileSearchStorage};
use crate::storage::DatabaseBackend;

impl FileSearchStorage {
    /// Record cleanup intent before publishing bytes. SQLite uses FULL sync so
    /// a power loss cannot discard the intent while leaving a published blob.
    pub(crate) async fn stage_blob_cleanup(
        &self,
        id: &str,
        owner_token: &str,
        lease_expires_at: i64,
    ) -> Result<(), FileSearchError> {
        let mut connection = self.pool.acquire().await?;
        let sqlite = DatabaseBackend::from_connection(&connection) == DatabaseBackend::Sqlite;
        if sqlite {
            sqlx::query("PRAGMA synchronous = FULL")
                .execute(&mut *connection)
                .await?;
        }
        let result = sqlx::query(
            "INSERT INTO file_search_blob_cleanup (file_id, owner_token, intent, lease_expires_at) \
             VALUES ($1, $2, 'upload', $3)",
        )
        .bind(id)
        .bind(owner_token)
        .bind(lease_expires_at)
        .execute(&mut *connection)
        .await;
        if sqlite {
            // A failed reset leaves a more durable (but slower) connection.
            sqlx::query("PRAGMA synchronous = NORMAL")
                .execute(&mut *connection)
                .await?;
        }
        result?;
        Ok(())
    }

    pub(crate) async fn refresh_blob_cleanup(
        &self,
        id: &str,
        owner_token: &str,
        lease_expires_at: i64,
    ) -> Result<bool, FileSearchError> {
        let updated = sqlx::query(
            "UPDATE file_search_blob_cleanup SET lease_expires_at = $3 \
             WHERE file_id = $1 AND owner_token = $2 AND intent = 'upload'",
        )
        .bind(id)
        .bind(owner_token)
        .bind(lease_expires_at)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();
        Ok(updated != 0)
    }

    pub(crate) async fn delete(
        &self,
        collection: Collection,
        id: &str,
        store_id: Option<&str>,
    ) -> Result<(), FileSearchError> {
        let mut connection = self.pool.acquire().await?;
        let sqlite = matches!(collection, Collection::Files)
            && DatabaseBackend::from_connection(&connection) == DatabaseBackend::Sqlite;
        if sqlite {
            sqlx::query("PRAGMA synchronous = FULL")
                .execute(&mut *connection)
                .await?;
        }
        let result = async {
            let mut tx = connection.begin().await?;
            if matches!(collection, Collection::Files) {
                sqlx::query(
                    "INSERT INTO file_search_blob_cleanup (file_id, owner_token, intent, lease_expires_at) \
                     SELECT id, 'delete', 'delete', 0 FROM file_search_files \
                     WHERE id = $1 AND content_base64 = '' \
                     ON CONFLICT (file_id) DO UPDATE SET owner_token = 'delete', intent = 'delete', lease_expires_at = 0",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            let filter = if store_id.is_some() { " AND store_id = $2" } else { "" };
            let sql = format!(
                "DELETE FROM {} WHERE {} = $1{filter}",
                collection.table(),
                collection.id()
            );
            let mut query = sqlx::query(&sql).bind(id);
            if let Some(store_id) = store_id {
                query = query.bind(store_id);
            }
            let deleted = query.execute(&mut *tx).await?.rows_affected();
            if deleted == 0 {
                return Err(FileSearchError::NotFound("File or vector store not found".into()));
            }
            tx.commit().await?;
            Ok(())
        }
        .await;
        if sqlite {
            sqlx::query("PRAGMA synchronous = NORMAL")
                .execute(&mut *connection)
                .await?;
        }
        result
    }

    pub(crate) async fn pending_blob_cleanup(&self, id: &str) -> Result<Option<String>, FileSearchError> {
        sqlx::query_scalar(
            "SELECT owner_token FROM file_search_blob_cleanup \
            WHERE file_id = $1 AND intent IN ('delete', 'recovery')",
        )
        .bind(id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(Into::into)
    }

    pub(crate) async fn complete_blob_cleanup(&self, id: &str, owner_token: &str) -> Result<(), FileSearchError> {
        // A claimed upload can still finish a write after recovery removes its
        // currently visible bytes. Keep that tombstone for later sweeps until
        // the original publisher can be proved quiescent.
        sqlx::query(
            "DELETE FROM file_search_blob_cleanup WHERE file_id = $1 AND owner_token = $2 AND intent != 'abandoned'",
        )
        .bind(id)
        .bind(owner_token)
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    pub(crate) async fn blob_cleanup_batch(&self, limit: i64, now: i64) -> Result<Vec<String>, FileSearchError> {
        sqlx::query_scalar(
            "SELECT file_id FROM file_search_blob_cleanup WHERE lease_expires_at <= $1 \
             ORDER BY lease_expires_at, file_id LIMIT $2",
        )
        .bind(now)
        .bind(limit)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(Into::into)
    }

    /// Atomically fence a stale publisher before removing any of its bytes.
    pub(crate) async fn claim_blob_cleanup(
        &self,
        id: &str,
        owner_token: &str,
        now: i64,
        lease_expires_at: i64,
    ) -> Result<bool, FileSearchError> {
        let mut connection = self.pool.acquire().await?;
        let sqlite = DatabaseBackend::from_connection(&connection) == DatabaseBackend::Sqlite;
        if sqlite {
            sqlx::query("PRAGMA synchronous = FULL")
                .execute(&mut *connection)
                .await?;
        }
        let result = sqlx::query(
            "UPDATE file_search_blob_cleanup SET owner_token = $2, \
             intent = CASE WHEN intent IN ('upload', 'abandoned') THEN 'abandoned' ELSE 'recovery' END, \
             lease_expires_at = $4 WHERE file_id = $1 AND lease_expires_at <= $3",
        )
        .bind(id)
        .bind(owner_token)
        .bind(now)
        .bind(lease_expires_at)
        .execute(&mut *connection)
        .await;
        if sqlite {
            sqlx::query("PRAGMA synchronous = NORMAL")
                .execute(&mut *connection)
                .await?;
        }
        Ok(result?.rows_affected() != 0)
    }
}
