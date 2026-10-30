//! Durable intent records for cross-store blob operations.

use sqlx::Acquire;

use super::{Collection, FileSearchError, FileSearchStorage};
use crate::storage::DatabaseBackend;

impl FileSearchStorage {
    /// Record cleanup intent before publishing bytes. SQLite uses FULL sync so
    /// a power loss cannot discard the intent while leaving a published blob.
    pub(crate) async fn stage_blob_cleanup(&self, id: &str) -> Result<(), FileSearchError> {
        let mut connection = self.pool.acquire().await?;
        let sqlite = DatabaseBackend::from_connection(&connection) == DatabaseBackend::Sqlite;
        if sqlite {
            sqlx::query("PRAGMA synchronous = FULL")
                .execute(&mut *connection)
                .await?;
        }
        let result =
            sqlx::query("INSERT INTO file_search_blob_cleanup (file_id) VALUES ($1) ON CONFLICT (file_id) DO NOTHING")
                .bind(id)
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
                    "INSERT INTO file_search_blob_cleanup (file_id) \
                     SELECT id FROM file_search_files WHERE id = $1 AND content_base64 = '' \
                     ON CONFLICT (file_id) DO NOTHING",
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

    pub(crate) async fn pending_blob_cleanup(&self, id: &str) -> Result<bool, FileSearchError> {
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_blob_cleanup WHERE file_id = $1")
            .bind(id)
            .fetch_one(self.pool.as_ref())
            .await?;
        Ok(pending != 0)
    }

    pub(crate) async fn complete_blob_cleanup(&self, id: &str) -> Result<(), FileSearchError> {
        sqlx::query("DELETE FROM file_search_blob_cleanup WHERE file_id = $1")
            .bind(id)
            .execute(self.pool.as_ref())
            .await?;
        Ok(())
    }

    pub(crate) async fn blob_cleanup_batch(&self, after: &str, limit: i64) -> Result<Vec<String>, FileSearchError> {
        sqlx::query_scalar("SELECT file_id FROM file_search_blob_cleanup WHERE file_id > $1 ORDER BY file_id LIMIT $2")
            .bind(after)
            .bind(limit)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(Into::into)
    }
}
