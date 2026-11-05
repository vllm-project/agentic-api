//! Portable persistence for durable file search. Ingestion is published in one transaction.

use std::sync::Arc;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlx::FromRow;
use tokio_util::sync::CancellationToken;

use super::DbPool;
use crate::types::file_search::{
    FileObject, FileSearchError, ListOrder, ListParams, VectorStoreFileObject, VectorStoreObject,
    cancelled_upload_error,
};

mod blob_cleanup;
mod list;
mod publication;
pub(crate) mod sqlite_vec_index;

use publication::publish_attachment;
pub(crate) use publication::publish_attachment_sqlite;

pub(crate) const MAX_CORPUS_BYTES: i64 = 64 * 1024 * 1024;
// The indexed provider does not load every vector into memory for normal search.
pub(crate) const SQLITE_VEC_MAX_CORPUS_BYTES: i64 = 256 * 1024 * 1024;
pub(crate) const MAX_CORPUS_CHUNKS: i64 = 10_000;

/// The default SQL-backed file metadata and exact chunk repository.
#[derive(Clone)]
pub struct FileSearchStorage {
    pool: Arc<DbPool>,
}

/// File metadata returned to the ingestion layer. Older inline uploads retain
/// their base64 payload until they are migrated to a blob store.
pub struct UploadedFile {
    pub object: FileObject,
    pub content_type: String,
    pub content: StoredFileContent,
}

/// Where an upload's bytes live. Inline content exists only for older data.
pub enum StoredFileContent {
    Blob,
    LegacyInlineBase64(String),
}

/// Whether a failed metadata publication permits deleting the uploaded blob.
#[derive(Debug)]
pub enum FilePublicationFailure {
    SafeToRemove(FileSearchError),
    // A lost connection during COMMIT can leave the outcome unknown. Keeping
    // the blob prevents a successfully committed row from losing its content.
    Indeterminate(FileSearchError),
}

#[derive(FromRow)]
pub struct StoredVectorStore {
    pub embedding_identity: String,
    pub embedding_dimensions: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredChunk {
    pub file_id: String,
    pub filename: String,
    pub chunk_index: usize,
    pub text: String,
    pub embedding: Option<Vec<f64>>,
    pub attributes: crate::types::file_search::FileAttributes,
}

pub struct PreparedAttachment {
    pub object: VectorStoreFileObject,
    pub chunks: Vec<StoredChunk>,
    pub dimensions: i64,
}

#[derive(Clone, Copy)]
pub enum Collection {
    Files,
    Stores,
    Attachments,
}

impl Collection {
    const fn table(self) -> &'static str {
        match self {
            Self::Files => "file_search_files",
            Self::Stores => "file_search_stores",
            Self::Attachments => "file_search_attachments",
        }
    }
    const fn id(self) -> &'static str {
        if matches!(self, Self::Attachments) {
            "file_id"
        } else {
            "id"
        }
    }
}

impl FileSearchStorage {
    /// Creates the default SQL file search repository.
    #[must_use]
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self { pool }
    }

    pub(crate) async fn upload(
        &self,
        file: &FileObject,
        content_type: &str,
        owner_token: &str,
        cancelled: &CancellationToken,
    ) -> Result<(), FilePublicationFailure> {
        let data = serde_json::to_string(file).map_err(|error| FilePublicationFailure::SafeToRemove(error.into()))?;
        let mut tx = tokio::select! {
            biased;
            () = cancelled.cancelled() => return Err(FilePublicationFailure::SafeToRemove(cancelled_upload_error())),
            result = self.pool.begin() => result.map_err(|error| FilePublicationFailure::SafeToRemove(error.into()))?,
        };
        // Lock the upload intent through metadata publication. A recovery
        // claimant must wait for this transaction and cannot fence it halfway
        // between the metadata insert and intent removal.
        let ownership = sqlx::query(
            "UPDATE file_search_blob_cleanup SET lease_expires_at = $3 \
            WHERE file_id = $1 AND owner_token = $2 AND intent = 'upload'",
        )
        .bind(&file.id)
        .bind(owner_token)
        .bind(chrono::Utc::now().timestamp() + 60)
        .execute(&mut *tx)
        .await;
        match ownership {
            Ok(result) if result.rows_affected() == 1 => {}
            Ok(_) => {
                let _ = tx.rollback().await;
                return Err(FilePublicationFailure::SafeToRemove(FileSearchError::Unavailable(
                    "Upload ownership expired; retry the upload".into(),
                )));
            }
            Err(error) => {
                let _ = tx.rollback().await;
                return Err(FilePublicationFailure::SafeToRemove(error.into()));
            }
        }
        let query = sqlx::query(
            "INSERT INTO file_search_files (id, created_at, data, content_type, content_base64) \
            SELECT $1, $2, $3, $4, '' WHERE EXISTS (SELECT 1 FROM file_search_blob_cleanup \
            WHERE file_id = $1 AND owner_token = $5 AND intent = 'upload')",
        )
        .bind(&file.id)
        .bind(file.created_at)
        .bind(data)
        .bind(content_type)
        .bind(owner_token);
        let inserted = tokio::select! {
            biased;
            () = cancelled.cancelled() => Err(cancelled_upload_error()),
            result = query.execute(&mut *tx) => result.map(|result| result.rows_affected()).map_err(FileSearchError::from),
        };
        let inserted = inserted.and_then(|rows| {
            if cancelled.is_cancelled() {
                Err(cancelled_upload_error())
            } else if rows == 0 {
                Err(FileSearchError::Unavailable(
                    "Upload ownership expired; retry the upload".into(),
                ))
            } else {
                Ok(())
            }
        });
        if let Err(error) = inserted {
            if let Err(rollback) = tx.rollback().await {
                tracing::warn!(%rollback, "file metadata rollback failed; connection will be discarded");
            }
            return Err(FilePublicationFailure::SafeToRemove(error));
        }
        let cleared = sqlx::query(
            "DELETE FROM file_search_blob_cleanup WHERE file_id = $1 AND owner_token = $2 AND intent = 'upload'",
        )
        .bind(&file.id)
        .bind(owner_token)
        .execute(&mut *tx)
        .await;
        match cleared {
            Ok(result) if result.rows_affected() == 1 => {}
            Ok(_) => {
                let _ = tx.rollback().await;
                return Err(FilePublicationFailure::SafeToRemove(FileSearchError::Unavailable(
                    "Upload ownership expired; retry the upload".into(),
                )));
            }
            Err(error) => {
                let _ = tx.rollback().await;
                return Err(FilePublicationFailure::SafeToRemove(error.into()));
            }
        }
        // Complete COMMIT even if the caller cancels at this point. The owned
        // operation task preserves the blob whenever the outcome is uncertain.
        tx.commit()
            .await
            .map_err(|error| FilePublicationFailure::Indeterminate(error.into()))
    }

    pub(crate) async fn file(&self, id: &str) -> Result<UploadedFile, FileSearchError> {
        let row: (String, String, String) =
            sqlx::query_as("SELECT data, content_type, content_base64 FROM file_search_files WHERE id = $1")
                .bind(id)
                .fetch_optional(self.pool.as_ref())
                .await?
                .ok_or_else(|| FileSearchError::NotFound("File not found".into()))?;
        Ok(UploadedFile {
            object: serde_json::from_str(&row.0)?,
            content_type: row.1,
            content: if row.2.is_empty() {
                StoredFileContent::Blob
            } else {
                StoredFileContent::LegacyInlineBase64(row.2)
            },
        })
    }

    pub(crate) async fn file_object(&self, id: &str) -> Result<FileObject, FileSearchError> {
        let data: String = sqlx::query_scalar("SELECT data FROM file_search_files WHERE id = $1")
            .bind(id)
            .fetch_optional(self.pool.as_ref())
            .await?
            .ok_or_else(|| FileSearchError::NotFound("File not found".into()))?;
        Ok(serde_json::from_str(&data)?)
    }

    pub(crate) async fn store(&self, id: &str) -> Result<StoredVectorStore, FileSearchError> {
        sqlx::query_as("SELECT embedding_identity, embedding_dimensions FROM file_search_stores WHERE id = $1")
            .bind(id)
            .fetch_optional(self.pool.as_ref())
            .await?
            .ok_or_else(|| FileSearchError::NotFound("Vector store not found".into()))
    }

    pub(crate) async fn store_object(&self, id: &str) -> Result<VectorStoreObject, FileSearchError> {
        let data: String = sqlx::query_scalar("SELECT data FROM file_search_stores WHERE id = $1")
            .bind(id)
            .fetch_optional(self.pool.as_ref())
            .await?
            .ok_or_else(|| FileSearchError::NotFound("Vector store not found".into()))?;
        let mut store: VectorStoreObject = serde_json::from_str(&data)?;
        let (count, bytes): (i64, i64) = sqlx::query_as("SELECT COUNT(*), CAST(COALESCE(SUM(usage_bytes), 0) AS BIGINT) FROM file_search_attachments WHERE store_id = $1")
            .bind(id).fetch_one(self.pool.as_ref()).await?;
        store.file_counts.completed = count;
        store.file_counts.total = count;
        store.usage_bytes = bytes;
        Ok(store)
    }

    pub(crate) async fn create_store(
        &self,
        object: &VectorStoreObject,
        identity: &str,
        attachments: &[PreparedAttachment],
    ) -> Result<(), FileSearchError> {
        let mut encoded = Vec::with_capacity(attachments.len());
        let mut total_bytes = 0;
        for attachment in attachments {
            let chunks = serialize_chunks(&attachment.chunks).await?;
            total_bytes += chunks.1;
            validate_capacity(total_bytes, 0)?;
            encoded.push(chunks);
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO file_search_stores (id, created_at, data, embedding_identity, embedding_dimensions) VALUES ($1, $2, $3, $4, 0)")
            .bind(&object.id).bind(object.created_at).bind(serde_json::to_string(object)?).bind(identity)
            .execute(&mut *tx).await?;
        for (attachment, (chunks, storage_bytes)) in attachments.iter().zip(encoded) {
            publish_attachment(&mut tx, &object.id, identity, attachment, &chunks, storage_bytes).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn attach(
        &self,
        store_id: &str,
        identity: &str,
        attachment: &PreparedAttachment,
    ) -> Result<(), FileSearchError> {
        let (chunks, storage_bytes) = serialize_chunks(&attachment.chunks).await?;
        let mut tx = self.pool.begin().await?;
        publish_attachment(&mut tx, store_id, identity, attachment, &chunks, storage_bytes).await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn attachment(
        &self,
        store_id: &str,
        file_id: &str,
    ) -> Result<Option<VectorStoreFileObject>, FileSearchError> {
        let data: Option<String> =
            sqlx::query_scalar("SELECT data FROM file_search_attachments WHERE store_id = $1 AND file_id = $2")
                .bind(store_id)
                .bind(file_id)
                .fetch_optional(self.pool.as_ref())
                .await?;
        data.as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(Into::into)
    }

    /// Keyset pagination orders ties by ID and never fetches uploaded bytes.
    pub(crate) async fn list<T: DeserializeOwned>(
        &self,
        collection: Collection,
        store_id: Option<&str>,
        params: &ListParams,
    ) -> Result<Vec<T>, FileSearchError> {
        let ascending = matches!(params.order.unwrap_or_default(), ListOrder::Asc);
        let cursor = params.after.as_deref().or(params.before.as_deref()).unwrap_or("");
        let mut created = 0i64;
        if !cursor.is_empty() {
            let filter = if store_id.is_some() { " AND store_id = $2" } else { "" };
            let sql = format!(
                "SELECT created_at FROM {} WHERE {} = $1{filter}",
                collection.table(),
                collection.id()
            );
            let mut query = sqlx::query_scalar(&sql).bind(cursor);
            if let Some(store_id) = store_id {
                query = query.bind(store_id);
            }
            created = query.fetch_optional(self.pool.as_ref()).await?.ok_or_else(|| {
                FileSearchError::InvalidRequest("Pagination cursor does not exist in this collection".into())
            })?;
        }
        let operator = if ascending == params.before.is_none() { ">" } else { "<" };
        let order = if ascending == params.before.is_none() {
            "ASC"
        } else {
            "DESC"
        };
        let filter = if store_id.is_some() { " AND store_id = $4" } else { "" };
        let sql = format!(
            "SELECT data FROM {table} WHERE ($1 = '' OR created_at {operator} $2 OR (created_at = $2 AND {id} {operator} $1)){filter} ORDER BY created_at {order}, {id} {order} LIMIT $3",
            table = collection.table(),
            id = collection.id()
        );
        let limit = i64::try_from(params.limit.unwrap_or(20) + 1)
            .map_err(|_| FileSearchError::InvalidRequest("Invalid page size".into()))?;
        let mut query = sqlx::query_scalar(&sql).bind(cursor).bind(created).bind(limit);
        if let Some(store_id) = store_id {
            query = query.bind(store_id);
        }
        let rows: Vec<String> = query.fetch_all(self.pool.as_ref()).await?;
        rows.iter()
            .map(|row| serde_json::from_str(row).map_err(Into::into))
            .collect()
    }

    /// Checks whether selected stores contain indexed chunks.
    pub(crate) async fn has_chunks(&self, store_ids: &[String]) -> Result<bool, FileSearchError> {
        let placeholders = (1..=store_ids.len())
            .map(|index| format!("${index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT 1 FROM file_search_chunks WHERE store_id IN ({placeholders}) LIMIT 1");
        let mut query = sqlx::query_scalar::<_, i64>(&sql);
        for id in store_ids {
            query = query.bind(id);
        }
        Ok(query.fetch_optional(self.pool.as_ref()).await?.is_some())
    }

    /// Streaming row decoding bounds corpus memory before deserializing vectors.
    /// The repository passes its own admission limit for exact search.
    pub(crate) async fn chunks_with_limit(
        &self,
        store_ids: &[String],
        max_bytes: i64,
    ) -> Result<Vec<StoredChunk>, FileSearchError> {
        use futures::TryStreamExt;
        let placeholders = (1..=store_ids.len())
            .map(|index| format!("${index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT data FROM file_search_chunks WHERE store_id IN ({placeholders}) ORDER BY store_id, file_id, chunk_index"
        );
        let mut query = sqlx::query_scalar::<_, String>(&sql);
        for id in store_ids {
            query = query.bind(id);
        }
        let mut rows = query.fetch(self.pool.as_ref());
        let mut chunks = Vec::new();
        let mut total_bytes = 0usize;
        while let Some(data) = rows.try_next().await? {
            total_bytes = total_bytes.saturating_add(data.len());
            if i64::try_from(total_bytes).unwrap_or(i64::MAX) > max_bytes
                || i64::try_from(chunks.len()).unwrap_or(i64::MAX) >= MAX_CORPUS_CHUNKS
            {
                return Err(FileSearchError::Unavailable(
                    "Selected corpus exceeds the exact retrieval capacity; search fewer vector stores or files".into(),
                ));
            }
            chunks.push(serde_json::from_str(&data)?);
        }
        Ok(chunks)
    }
}

pub(crate) fn validate_capacity(bytes: i64, chunks: i64) -> Result<(), FileSearchError> {
    validate_capacity_with_limit(bytes, chunks, MAX_CORPUS_BYTES)
}

pub(crate) fn validate_capacity_with_limit(bytes: i64, chunks: i64, max_bytes: i64) -> Result<(), FileSearchError> {
    if bytes > max_bytes || chunks > MAX_CORPUS_CHUNKS {
        return Err(FileSearchError::InvalidRequest(format!(
            "Vector store exceeds {} MiB of serialized chunks or 10000 chunks; reduce file size or use another vector store",
            max_bytes / (1024 * 1024)
        )));
    }
    Ok(())
}

pub(crate) async fn serialize_chunks(chunks: &[StoredChunk]) -> Result<(Vec<String>, i64), FileSearchError> {
    serialize_chunks_with_limit(chunks, MAX_CORPUS_BYTES).await
}

pub(crate) async fn serialize_chunks_with_limit(
    chunks: &[StoredChunk],
    max_bytes: i64,
) -> Result<(Vec<String>, i64), FileSearchError> {
    let mut encoded = Vec::with_capacity(chunks.len());
    let mut bytes = 0i64;
    for (index, chunk) in chunks.iter().enumerate() {
        let data = serde_json::to_string(chunk)?;
        bytes = bytes.saturating_add(i64::try_from(data.len()).unwrap_or(i64::MAX));
        validate_capacity_with_limit(bytes, i64::try_from(index + 1).unwrap_or(i64::MAX), max_bytes)?;
        encoded.push(data);
        // Each chunk is bounded. Yield between small groups so cancellation and
        // other requests remain responsive while preparing a large publication.
        if index % 32 == 0 {
            tokio::task::yield_now().await;
        }
    }
    Ok((encoded, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        storage::create_pool_with_schema,
        tool::file_search::FileSearchService,
        types::file_search::{ChunkingStrategy, CreateVectorStoreRequest, FileAttributes, FileSearchConfig},
    };

    async fn prepared(
        service: &FileSearchService,
        store_id: &str,
        count: usize,
        dimensions: usize,
    ) -> PreparedAttachment {
        let file = service
            .upload_file("capacity.txt", "text/plain", "assistants", b"capacity".to_vec())
            .await
            .unwrap();
        PreparedAttachment {
            object: VectorStoreFileObject {
                id: file.id.clone(),
                object: "vector_store.file".into(),
                created_at: 0,
                vector_store_id: store_id.into(),
                status: "completed".into(),
                usage_bytes: 0,
                attributes: FileAttributes::default(),
                chunking_strategy: ChunkingStrategy::Auto,
                last_error: None,
            },
            dimensions: i64::try_from(dimensions).unwrap(),
            chunks: (0..count)
                .map(|chunk_index| StoredChunk {
                    file_id: file.id.clone(),
                    filename: file.filename.clone(),
                    chunk_index,
                    text: "capacity".into(),
                    attributes: FileAttributes::default(),
                    embedding: (dimensions != 0).then(|| vec![std::f64::consts::PI; dimensions]),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn large_serialized_embedding_is_published_within_capacity() {
        let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
        let files = tempfile::tempdir().unwrap();
        let service = FileSearchService::new(
            pool.clone(),
            Arc::new(reqwest::Client::new()),
            FileSearchConfig {
                files_storage_dir: Some(files.path().to_owned()),
                ..FileSearchConfig::default()
            },
        )
        .unwrap();
        let store = service
            .create_vector_store(CreateVectorStoreRequest::default())
            .await
            .unwrap();
        let attachment = prepared(&service, &store.id, 512, 4096).await;
        let storage = FileSearchStorage::new(pool);
        storage
            .attach(&store.id, "keyword:cl100k_base:v1", &attachment)
            .await
            .unwrap();
        assert_eq!(
            storage
                .chunks_with_limit(std::slice::from_ref(&store.id), MAX_CORPUS_BYTES)
                .await
                .unwrap()
                .len(),
            512
        );
        assert_eq!(service.get_vector_store(&store.id).await.unwrap().file_counts.total, 1);
    }

    #[tokio::test]
    async fn store_chunk_capacity_is_checked_across_attachments() {
        let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
        let files = tempfile::tempdir().unwrap();
        let service = FileSearchService::new(
            pool.clone(),
            Arc::new(reqwest::Client::new()),
            FileSearchConfig {
                files_storage_dir: Some(files.path().to_owned()),
                ..FileSearchConfig::default()
            },
        )
        .unwrap();
        let store = service
            .create_vector_store(CreateVectorStoreRequest::default())
            .await
            .unwrap();
        let storage = FileSearchStorage::new(pool);
        for _ in 0..5 {
            storage
                .attach(
                    &store.id,
                    "keyword:cl100k_base:v1",
                    &prepared(&service, &store.id, 2000, 0).await,
                )
                .await
                .unwrap();
        }
        let overflow = prepared(&service, &store.id, 1, 0).await;
        assert!(matches!(
            storage.attach(&store.id, "keyword:cl100k_base:v1", &overflow).await,
            Err(FileSearchError::InvalidRequest(_))
        ));
        assert_eq!(
            storage
                .chunks_with_limit(std::slice::from_ref(&store.id), MAX_CORPUS_BYTES)
                .await
                .unwrap()
                .len(),
            10000
        );
        assert_eq!(service.get_vector_store(&store.id).await.unwrap().file_counts.total, 5);
    }

    #[tokio::test]
    async fn store_serialized_capacity_is_checked_across_attachments() {
        let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
        let files = tempfile::tempdir().unwrap();
        let service = FileSearchService::new(
            pool.clone(),
            Arc::new(reqwest::Client::new()),
            FileSearchConfig {
                files_storage_dir: Some(files.path().to_owned()),
                ..FileSearchConfig::default()
            },
        )
        .unwrap();
        let store = service
            .create_vector_store(CreateVectorStoreRequest::default())
            .await
            .unwrap();
        let storage = FileSearchStorage::new(pool);
        let first = prepared(&service, &store.id, 512, 4096).await;
        storage
            .attach(&store.id, "keyword:cl100k_base:v1", &first)
            .await
            .unwrap();
        let overflow = prepared(&service, &store.id, 512, 4096).await;
        assert!(matches!(
            storage.attach(&store.id, "keyword:cl100k_base:v1", &overflow).await,
            Err(FileSearchError::InvalidRequest(_))
        ));
        assert_eq!(
            storage
                .chunks_with_limit(std::slice::from_ref(&store.id), MAX_CORPUS_BYTES)
                .await
                .unwrap()
                .len(),
            512
        );
        assert_eq!(service.get_vector_store(&store.id).await.unwrap().file_counts.total, 1);
        assert!(
            storage
                .attachment(&store.id, &overflow.object.id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
