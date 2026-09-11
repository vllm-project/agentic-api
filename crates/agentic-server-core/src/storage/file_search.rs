//! Portable persistence for durable file search. Ingestion is published in one transaction.

use std::sync::Arc;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlx::FromRow;
use tokio_util::sync::CancellationToken;

use super::{DbPool, DbTransaction};
use crate::types::file_search::{
    FileObject, FileSearchError, ListOrder, ListParams, VectorStoreFileObject, VectorStoreObject,
};

const MAX_CORPUS_BYTES: i64 = 64 * 1024 * 1024;
const MAX_CORPUS_CHUNKS: i64 = 10_000;

#[derive(Clone)]
pub(crate) struct FileSearchStorage {
    pool: Arc<DbPool>,
    pgvector: Option<super::pgvector::PgvectorStorage>,
}

#[derive(FromRow)]
pub(crate) struct UploadedFile {
    pub data: String,
    pub content_type: String,
    pub content_base64: String,
}

pub(crate) enum FilePublicationFailure {
    SafeToRemove(FileSearchError),
    // A lost connection during COMMIT can leave the outcome unknown. Keeping
    // the blob prevents a successfully committed row from losing its content.
    Indeterminate(sqlx::Error),
}

#[derive(FromRow)]
pub(crate) struct StoredVectorStore {
    pub data: String,
    pub embedding_identity: String,
    pub embedding_dimensions: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StoredChunk {
    pub file_id: String,
    pub filename: String,
    pub chunk_index: usize,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_text: Option<String>,
    pub embedding: Option<Vec<f64>>,
    pub attributes: crate::types::file_search::FileAttributes,
}

pub(crate) struct PreparedAttachment {
    pub object: VectorStoreFileObject,
    pub chunks: Vec<StoredChunk>,
    pub dimensions: i64,
}

#[derive(Clone, Copy)]
pub(crate) enum Collection {
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
    #[cfg(test)]
    pub(crate) fn new(pool: Arc<DbPool>) -> Self {
        Self { pool, pgvector: None }
    }

    pub(crate) fn with_backend(
        pool: Arc<DbPool>,
        backend: &crate::types::file_search::FileSearchBackend,
    ) -> Result<Self, FileSearchError> {
        let pgvector = super::pgvector::PgvectorStorage::from_config(&pool, backend)?;
        Ok(Self { pool, pgvector })
    }

    pub(crate) fn vector_dimensions(&self) -> Option<usize> {
        self.pgvector.as_ref().map(super::pgvector::PgvectorStorage::dimensions)
    }

    pub(crate) async fn initialize(&self) -> Result<(), FileSearchError> {
        if let Some(pgvector) = &self.pgvector {
            pgvector.initialize(&self.pool).await?;
        }
        Ok(())
    }

    pub(crate) async fn has_chunks(&self, stores: &[String]) -> Result<bool, FileSearchError> {
        let placeholders = (1..=stores.len())
            .map(|index| format!("${index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT chunk_index FROM file_search_chunks WHERE store_id IN ({placeholders}) LIMIT 1");
        let mut query = sqlx::query_scalar::<_, i64>(&sql);
        for store in stores {
            query = query.bind(store);
        }
        Ok(query.fetch_optional(self.pool.as_ref()).await?.is_some())
    }

    pub(crate) async fn candidates(
        &self,
        stores: &[String],
        queries: &[String],
        vectors: &[Vec<f64>],
        mode: crate::types::file_search::SearchMode,
        filter: Option<&crate::types::file_search::SearchFilter>,
    ) -> Result<Vec<StoredChunk>, FileSearchError> {
        match &self.pgvector {
            Some(pgvector) => {
                pgvector
                    .candidates(&self.pool, stores, queries, vectors, mode, filter)
                    .await
            }
            None => self.chunks(stores).await,
        }
    }

    pub(crate) async fn upload(
        &self,
        file: &FileObject,
        content_type: &str,
        cancelled: &CancellationToken,
    ) -> Result<(), FilePublicationFailure> {
        let data = serde_json::to_string(file).map_err(|error| FilePublicationFailure::SafeToRemove(error.into()))?;
        let mut tx = tokio::select! {
            biased;
            () = cancelled.cancelled() => return Err(FilePublicationFailure::SafeToRemove(super::local_files::cancelled_error())),
            result = self.pool.begin() => result.map_err(|error| FilePublicationFailure::SafeToRemove(error.into()))?,
        };
        let query = sqlx::query("INSERT INTO file_search_files (id, created_at, data, content_type, content_base64) VALUES ($1, $2, $3, $4, '')")
            .bind(&file.id).bind(file.created_at).bind(data).bind(content_type);
        let inserted = tokio::select! {
            biased;
            () = cancelled.cancelled() => Err(super::local_files::cancelled_error()),
            result = query.execute(&mut *tx) => result.map(|_| ()).map_err(FileSearchError::from),
        };
        let inserted = inserted.and_then(|()| {
            if cancelled.is_cancelled() {
                Err(super::local_files::cancelled_error())
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
        // Complete COMMIT even if the caller cancels at this point. The owned
        // operation task preserves the blob whenever the outcome is uncertain.
        tx.commit().await.map_err(FilePublicationFailure::Indeterminate)
    }

    pub(crate) async fn file(&self, id: &str) -> Result<UploadedFile, FileSearchError> {
        sqlx::query_as("SELECT data, content_type, content_base64 FROM file_search_files WHERE id = $1")
            .bind(id)
            .fetch_optional(self.pool.as_ref())
            .await?
            .ok_or_else(|| FileSearchError::NotFound("File not found".into()))
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
        sqlx::query_as("SELECT data, embedding_identity, embedding_dimensions FROM file_search_stores WHERE id = $1")
            .bind(id)
            .fetch_optional(self.pool.as_ref())
            .await?
            .ok_or_else(|| FileSearchError::NotFound("Vector store not found".into()))
    }

    pub(crate) async fn store_object(&self, id: &str) -> Result<VectorStoreObject, FileSearchError> {
        let row = self.store(id).await?;
        let mut store: VectorStoreObject = serde_json::from_str(&row.data)?;
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
        self.initialize().await?;
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
        self.initialize().await?;
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

    pub(crate) async fn delete(
        &self,
        collection: Collection,
        id: &str,
        store_id: Option<&str>,
    ) -> Result<(), FileSearchError> {
        let mut tx = self.pool.begin().await?;
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

    /// Streaming row decoding bounds corpus memory before deserializing vectors.
    pub(crate) async fn chunks(&self, store_ids: &[String]) -> Result<Vec<StoredChunk>, FileSearchError> {
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
            if i64::try_from(total_bytes).unwrap_or(i64::MAX) > MAX_CORPUS_BYTES
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

fn validate_capacity(bytes: i64, chunks: i64) -> Result<(), FileSearchError> {
    if bytes > MAX_CORPUS_BYTES || chunks > MAX_CORPUS_CHUNKS {
        return Err(FileSearchError::InvalidRequest(
            "Vector store exceeds 64 MiB of serialized chunks or 10000 chunks; reduce file size or use another vector store".into(),
        ));
    }
    Ok(())
}

async fn serialize_chunks(chunks: &[StoredChunk]) -> Result<(Vec<String>, i64), FileSearchError> {
    let mut encoded = Vec::with_capacity(chunks.len());
    let mut bytes = 0i64;
    for (index, chunk) in chunks.iter().enumerate() {
        let data = serde_json::to_string(chunk)?;
        bytes = bytes.saturating_add(i64::try_from(data.len()).unwrap_or(i64::MAX));
        validate_capacity(bytes, i64::try_from(index + 1).unwrap_or(i64::MAX))?;
        encoded.push(data);
        // Each chunk is bounded. Yield between small groups so cancellation and
        // other requests remain responsive while preparing a large publication.
        if index % 32 == 0 {
            tokio::task::yield_now().await;
        }
    }
    Ok((encoded, bytes))
}

async fn publish_attachment(
    tx: &mut DbTransaction<'_>,
    store_id: &str,
    identity: &str,
    attachment: &PreparedAttachment,
    chunks: &[String],
    storage_bytes: i64,
) -> Result<(), FileSearchError> {
    // The conditional write serializes concurrent ingestions and establishes the
    // model dimension exactly once; no network work takes place in this transaction.
    let changed = sqlx::query("UPDATE file_search_stores SET embedding_dimensions = $1 WHERE id = $2 AND embedding_identity = $3 AND (embedding_dimensions = 0 OR embedding_dimensions = $1)")
        .bind(attachment.dimensions).bind(store_id).bind(identity).execute(&mut **tx).await?.rows_affected();
    if changed != 1 {
        return Err(FileSearchError::Conflict(
            "Vector store embedding configuration changed or the vector store was deleted".into(),
        ));
    }
    let bytes: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(storage_bytes), 0) AS BIGINT) FROM file_search_attachments WHERE store_id = $1",
    )
    .bind(store_id)
    .fetch_one(&mut **tx)
    .await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE store_id = $1")
        .bind(store_id)
        .fetch_one(&mut **tx)
        .await?;
    validate_capacity(
        bytes.saturating_add(storage_bytes),
        count.saturating_add(i64::try_from(chunks.len()).unwrap_or(i64::MAX)),
    )?;
    let object = &attachment.object;
    let inserted = sqlx::query("INSERT INTO file_search_attachments (store_id, file_id, created_at, usage_bytes, data, storage_bytes) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(store_id).bind(&object.id).bind(object.created_at).bind(object.usage_bytes).bind(serde_json::to_string(object)?)
        .bind(storage_bytes)
        .execute(&mut **tx).await;
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
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
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
                    embedding_text: None,
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
    async fn serialized_embedding_capacity_is_checked_before_publication() {
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
        let attachment = prepared(&service, &store.id, 1024, 4096).await;
        let storage = FileSearchStorage::new(pool);
        assert!(matches!(
            storage.attach(&store.id, "keyword:cl100k_base:v1", &attachment).await,
            Err(FileSearchError::InvalidRequest(_))
        ));
        assert!(
            storage
                .chunks(std::slice::from_ref(&store.id))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(service.get_vector_store(&store.id).await.unwrap().file_counts.total, 0);
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
            storage.chunks(std::slice::from_ref(&store.id)).await.unwrap().len(),
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
            storage.chunks(std::slice::from_ref(&store.id)).await.unwrap().len(),
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
