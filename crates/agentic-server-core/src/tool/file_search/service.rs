//! Shared durable ingestion and retrieval service used by HTTP and the built-in tool.

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::{embeddings::Embeddings, ingest, ranking};
use crate::{
    storage::{
        DbPool,
        file_search::{Collection, FileSearchStorage, PreparedAttachment, StoredChunk, StoredVectorStore},
    },
    types::file_search::{
        AttachFileRequest, ChunkingStrategy, CreateVectorStoreRequest, DeleteObject, FileAttributes, FileCounts,
        FileObject, FileSearchConfig, FileSearchError, ListParams, ListResponse, SearchMode, SearchQuery,
        SearchRequest, SearchResponse, VectorStoreFileObject, VectorStoreObject, invalid, validate_attributes,
    },
};

/// Maximum accepted size for one uploaded file.
pub const MAX_FILE_BYTES: usize = 20 * 1024 * 1024;

struct CancelIngestionOnDrop(Arc<AtomicBool>);

impl Drop for CancelIngestionOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// In-tree exact retrieval over persistent SQL storage.
#[derive(Clone)]
pub struct FileSearchService {
    storage: FileSearchStorage,
    embeddings: Option<Embeddings>,
    workers: Arc<Semaphore>,
}

impl std::fmt::Debug for FileSearchService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileSearchService")
            .field("embeddings_configured", &self.embeddings.is_some())
            .finish_non_exhaustive()
    }
}

impl FileSearchService {
    /// Creates a service; no embeddings configuration selects keyword retrieval.
    ///
    /// # Errors
    /// Returns invalid-request when embedding configuration is incomplete or malformed.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the shared service constructor owns deployment configuration"
    )]
    pub fn new(
        pool: Arc<DbPool>,
        client: Arc<reqwest::Client>,
        config: FileSearchConfig,
    ) -> Result<Self, FileSearchError> {
        Ok(Self {
            storage: FileSearchStorage::new(pool),
            embeddings: Embeddings::from_config(client, &config)?,
            workers: Arc::new(Semaphore::new(4)),
        })
    }

    fn permit(&self) -> Result<Arc<OwnedSemaphorePermit>, FileSearchError> {
        self.workers
            .clone()
            .try_acquire_owned()
            .map(Arc::new)
            .map_err(|_| FileSearchError::Unavailable("File search is busy; retry the request".into()))
    }

    fn identity(&self) -> String {
        self.embeddings
            .as_ref()
            .map_or_else(|| "keyword:cl100k_base:v1".into(), Embeddings::identity)
    }

    fn compatible(&self, store: &StoredVectorStore) -> Result<(), FileSearchError> {
        if store.embedding_identity != self.identity() {
            return Err(FileSearchError::Conflict("Vector store was created with a different embedding configuration; restore that configuration or create a new vector store".into()));
        }
        Ok(())
    }

    /// Stores uploaded bytes durably in the database.
    ///
    /// # Errors
    /// Returns validation, resource-limit, or storage errors.
    pub async fn upload_file(
        &self,
        filename: &str,
        content_type: &str,
        purpose: &str,
        bytes: Vec<u8>,
    ) -> Result<FileObject, FileSearchError> {
        let permit = self.permit()?;
        if filename.is_empty() || filename.len() > 255 || filename.chars().any(char::is_control) {
            return invalid("filename must contain 1 to 255 bytes without control characters");
        }
        if !matches!(purpose, "assistants" | "user_data") {
            return invalid("file search accepts purpose assistants or user_data");
        }
        if bytes.is_empty() || bytes.len() > MAX_FILE_BYTES {
            return invalid("file must contain 1 byte to 20 MiB");
        }
        ingest::validate_content_type(filename, content_type)?;
        let file = FileObject {
            id: format!("file-{}", uuid::Uuid::now_v7()),
            object: "file".into(),
            bytes: size_i64(bytes.len())?,
            created_at: chrono::Utc::now().timestamp(),
            filename: filename.into(),
            purpose: purpose.into(),
            status: "processed".into(),
        };
        let worker_permit = permit.clone();
        let content = tokio::task::spawn_blocking(move || {
            let _permit = worker_permit;
            STANDARD.encode(bytes)
        })
        .await?;
        self.storage.upload(&file, content_type, &content).await?;
        Ok(file)
    }

    /// # Errors
    /// Returns pagination validation or storage errors.
    pub async fn list_files(&self, params: &ListParams) -> Result<ListResponse<FileObject>, FileSearchError> {
        validate_list(params)?;
        Ok(page(
            self.storage.list(Collection::Files, None, params).await?,
            params,
            |file: &FileObject| &file.id,
        ))
    }

    /// # Errors
    /// Returns not-found or storage errors.
    pub async fn get_file(&self, id: &str) -> Result<FileObject, FileSearchError> {
        self.storage.file_object(id).await
    }

    /// # Errors
    /// Returns not-found, resource-limit, or storage errors.
    pub async fn file_content(&self, id: &str) -> Result<Vec<u8>, FileSearchError> {
        let permit = self.permit()?;
        let file = self.storage.file(id).await?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            decode_content(&file.content_base64)
        })
        .await?
    }

    /// Deletes the file and all attachments/chunks atomically.
    ///
    /// # Errors
    /// Returns not-found or storage errors.
    pub async fn delete_file(&self, id: &str) -> Result<DeleteObject, FileSearchError> {
        self.storage.delete(Collection::Files, id, None).await?;
        Ok(deleted(id, "file.deleted"))
    }

    /// Creates a vector store, publishing initial file ingestion atomically.
    ///
    /// # Errors
    /// Returns validation, ingestion, provider, or storage errors.
    pub async fn create_vector_store(
        &self,
        request: CreateVectorStoreRequest,
    ) -> Result<VectorStoreObject, FileSearchError> {
        let permit = self.permit()?;
        if request.name.as_ref().is_some_and(|name| name.len() > 256) {
            return invalid("vector store name must not exceed 256 bytes");
        }
        if request.metadata.len() > 16
            || request
                .metadata
                .iter()
                .any(|(key, value)| key.is_empty() || key.len() > 64 || value.len() > 512)
        {
            return invalid("metadata accepts at most 16 entries, with 1 to 64 byte keys and values up to 512 bytes");
        }
        if request.file_ids.len() > 16
            || request.file_ids.iter().collect::<HashSet<_>>().len() != request.file_ids.len()
        {
            return invalid("file_ids accepts at most 16 unique file IDs per request");
        }
        let strategy = request.chunking_strategy.unwrap_or_default();
        ingest::chunking_config(&strategy)?;
        let object = VectorStoreObject {
            id: format!("vs_{}", uuid::Uuid::now_v7()),
            object: "vector_store".into(),
            created_at: chrono::Utc::now().timestamp(),
            name: request.name.unwrap_or_default(),
            usage_bytes: 0,
            file_counts: FileCounts::default(),
            status: "completed".into(),
            metadata: request.metadata,
        };
        let mut attachments = Vec::new();
        let mut dimensions = None;
        let mut total_bytes = 0usize;
        for file_id in request.file_ids {
            let prepared = self
                .prepare(
                    &object.id,
                    AttachFileRequest {
                        file_id,
                        attributes: FileAttributes::default(),
                        chunking_strategy: Some(strategy.clone()),
                    },
                    dimensions,
                    permit.clone(),
                )
                .await?;
            dimensions = usize::try_from(prepared.dimensions).ok().filter(|value| *value != 0);
            for chunk in &prepared.chunks {
                total_bytes = total_bytes.saturating_add(
                    chunk.text.len() + chunk.embedding.as_ref().map_or(0, |embedding| embedding.len() * 8),
                );
            }
            if total_bytes > 64 * 1024 * 1024 {
                return invalid(
                    "initial ingestion exceeds 64 MiB; create an empty vector store and attach files separately",
                );
            }
            attachments.push(prepared);
        }
        self.storage
            .create_store(&object, &self.identity(), &attachments)
            .await?;
        self.storage.store_object(&object.id).await
    }

    /// # Errors
    /// Returns pagination validation or storage errors.
    pub async fn list_vector_stores(
        &self,
        params: &ListParams,
    ) -> Result<ListResponse<VectorStoreObject>, FileSearchError> {
        validate_list(params)?;
        let stores: Vec<VectorStoreObject> = self.storage.list(Collection::Stores, None, params).await?;
        let mut page = page(stores, params, |store| &store.id);
        for store in &mut page.data {
            *store = self.storage.store_object(&store.id).await?;
        }
        Ok(page)
    }

    /// # Errors
    /// Returns not-found or storage errors.
    pub async fn get_vector_store(&self, id: &str) -> Result<VectorStoreObject, FileSearchError> {
        self.storage.store_object(id).await
    }

    /// # Errors
    /// Returns not-found or storage errors.
    pub async fn delete_vector_store(&self, id: &str) -> Result<DeleteObject, FileSearchError> {
        self.storage.delete(Collection::Stores, id, None).await?;
        Ok(deleted(id, "vector_store.deleted"))
    }

    /// Generates every chunk and embedding before atomically publishing the attachment.
    ///
    /// # Errors
    /// Returns validation, conflict, ingestion, provider, or storage errors.
    pub async fn attach_file(
        &self,
        store_id: &str,
        request: AttachFileRequest,
    ) -> Result<VectorStoreFileObject, FileSearchError> {
        let permit = self.permit()?;
        validate_attributes(&request.attributes)?;
        ingest::chunking_config(request.chunking_strategy.as_ref().unwrap_or(&ChunkingStrategy::Auto))?;
        let store = self.storage.store(store_id).await?;
        self.compatible(&store)?;
        if self.storage.attachment(store_id, &request.file_id).await?.is_some() {
            return Err(FileSearchError::Conflict(
                "File is already attached to this vector store".into(),
            ));
        }
        let dimensions = usize::try_from(store.embedding_dimensions)
            .ok()
            .filter(|value| *value != 0);
        let prepared = self.prepare(store_id, request, dimensions, permit.clone()).await?;
        self.storage.attach(store_id, &self.identity(), &prepared).await?;
        Ok(prepared.object)
    }

    async fn prepare(
        &self,
        store_id: &str,
        request: AttachFileRequest,
        dimensions: Option<usize>,
        permit: Arc<OwnedSemaphorePermit>,
    ) -> Result<PreparedAttachment, FileSearchError> {
        validate_attributes(&request.attributes)?;
        let uploaded = self.storage.file(&request.file_id).await?;
        let file: FileObject = serde_json::from_str(&uploaded.data)?;
        let strategy = request.chunking_strategy.unwrap_or_default();
        let chunking = ingest::chunking_config(&strategy)?;
        let filename = file.filename.clone();
        let worker_permit = permit.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelIngestionOnDrop(cancelled.clone());
        let texts = tokio::task::spawn_blocking(move || {
            let _permit = worker_permit;
            let bytes = decode_content(&uploaded.content_base64)?;
            ingest::extract_and_chunk(bytes, &filename, &uploaded.content_type, &chunking, &cancelled)
        })
        .await??;
        let vectors = if let Some(embeddings) = &self.embeddings {
            embeddings.embed(&texts, dimensions).await?
        } else {
            Vec::new()
        };
        let embedding_dimensions = vectors.first().map_or(0, Vec::len);
        let mut usage_bytes = 0usize;
        let mut embeddings = vectors.into_iter();
        let chunks = texts
            .into_iter()
            .enumerate()
            .map(|(chunk_index, text)| {
                let embedding = embeddings.next();
                usage_bytes = usage_bytes
                    .saturating_add(text.len() + embedding.as_ref().map_or(0, |embedding| embedding.len() * 8));
                StoredChunk {
                    file_id: file.id.clone(),
                    filename: file.filename.clone(),
                    chunk_index,
                    text,
                    embedding,
                    attributes: request.attributes.clone(),
                }
            })
            .collect();
        let object = VectorStoreFileObject {
            id: file.id,
            object: "vector_store.file".into(),
            created_at: chrono::Utc::now().timestamp(),
            vector_store_id: store_id.into(),
            status: "completed".into(),
            usage_bytes: size_i64(usage_bytes)?,
            attributes: request.attributes,
            chunking_strategy: strategy,
            last_error: None,
        };
        Ok(PreparedAttachment {
            object,
            chunks,
            dimensions: size_i64(embedding_dimensions)?,
        })
    }

    /// # Errors
    /// Returns not-found, pagination validation, or storage errors.
    pub async fn list_vector_store_files(
        &self,
        store_id: &str,
        params: &ListParams,
    ) -> Result<ListResponse<VectorStoreFileObject>, FileSearchError> {
        validate_list(params)?;
        self.storage.store(store_id).await?;
        Ok(page(
            self.storage
                .list(Collection::Attachments, Some(store_id), params)
                .await?,
            params,
            |file: &VectorStoreFileObject| &file.id,
        ))
    }

    /// # Errors
    /// Returns not-found or storage errors.
    pub async fn get_vector_store_file(
        &self,
        store_id: &str,
        file_id: &str,
    ) -> Result<VectorStoreFileObject, FileSearchError> {
        self.storage.store(store_id).await?;
        self.storage
            .attachment(store_id, file_id)
            .await?
            .ok_or_else(|| FileSearchError::NotFound("Vector store file not found".into()))
    }

    /// Deletes an attachment and all its chunks in one transaction, preserving the upload.
    ///
    /// # Errors
    /// Returns not-found or storage errors.
    pub async fn detach_file(&self, store_id: &str, file_id: &str) -> Result<DeleteObject, FileSearchError> {
        self.storage
            .delete(Collection::Attachments, file_id, Some(store_id))
            .await?;
        Ok(deleted(file_id, "vector_store.file.deleted"))
    }

    /// Retrieves and globally ranks deduplicated chunks across selected vector stores.
    ///
    /// # Errors
    /// Returns validation, configuration, provider, resource-limit, or storage errors.
    pub async fn search(
        &self,
        store_ids: &[String],
        request: &SearchRequest,
    ) -> Result<SearchResponse, FileSearchError> {
        request.validate()?;
        if store_ids.is_empty() || store_ids.len() > 16 || store_ids.iter().any(|id| id.is_empty() || id.len() > 128) {
            return invalid("search requires 1 to 16 vector store IDs of at most 128 bytes each");
        }
        let permit = self.permit()?;
        let mode = request.search_mode.unwrap_or(if self.embeddings.is_some() {
            SearchMode::Hybrid
        } else {
            SearchMode::Keyword
        });
        if mode != SearchMode::Keyword && self.embeddings.is_none() {
            return Err(FileSearchError::Unavailable(
                "Semantic and hybrid search require configured embeddings; use keyword search".into(),
            ));
        }
        if mode != SearchMode::Hybrid
            && request
                .ranking_options
                .as_ref()
                .is_some_and(|options| options.hybrid_search.is_some())
        {
            return invalid("hybrid_search weights require hybrid search mode");
        }
        let mut dimensions = None;
        for id in store_ids {
            let store = self.storage.store(id).await?;
            if mode != SearchMode::Keyword {
                self.compatible(&store)?;
                if store.embedding_dimensions != 0 {
                    let dimension =
                        usize::try_from(store.embedding_dimensions).map_err(|_| FileSearchError::ProviderProtocol)?;
                    if dimensions.is_some_and(|expected| expected != dimension) {
                        return Err(FileSearchError::Conflict(
                            "Selected vector stores have incompatible embedding dimensions".into(),
                        ));
                    }
                    dimensions = Some(dimension);
                }
            }
        }
        let chunks = self.storage.chunks(store_ids).await?;
        if mode != SearchMode::Keyword {
            for chunk in &chunks {
                let vector = chunk.embedding.as_ref().ok_or_else(|| {
                    FileSearchError::Unavailable("Stored embeddings are missing; recreate this vector store".into())
                })?;
                if dimensions.is_some_and(|expected| expected != vector.len())
                    || vector.is_empty()
                    || vector.iter().any(|value| !value.is_finite())
                {
                    return Err(FileSearchError::Unavailable(
                        "Stored embeddings are incompatible; recreate this vector store".into(),
                    ));
                }
                dimensions = Some(vector.len());
            }
        }
        let queries = match &request.query {
            SearchQuery::Text(query) => vec![query.clone()],
            SearchQuery::Texts(queries) => queries.clone(),
        };
        let vectors = if mode == SearchMode::Keyword || chunks.is_empty() {
            Vec::new()
        } else {
            let embeddings = self
                .embeddings
                .as_ref()
                .ok_or_else(|| FileSearchError::Unavailable("Embeddings are not configured".into()))?;
            embeddings.embed(&queries, dimensions).await?
        };
        let worker_queries = queries.clone();
        let request = request.clone();
        let data = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            ranking::rank(chunks, &worker_queries, &vectors, mode, &request)
        })
        .await?;
        Ok(SearchResponse {
            object: "vector_store.search_results.page".into(),
            search_query: queries,
            data,
            has_more: false,
            next_page: None,
        })
    }
}

fn size_i64(value: usize) -> Result<i64, FileSearchError> {
    i64::try_from(value)
        .map_err(|_| FileSearchError::InvalidRequest("File search size exceeds supported bounds".into()))
}
fn deleted(id: &str, object: &str) -> DeleteObject {
    DeleteObject {
        id: id.into(),
        object: object.into(),
        deleted: true,
    }
}
fn decode_content(encoded: &str) -> Result<Vec<u8>, FileSearchError> {
    if encoded.len() > MAX_FILE_BYTES.div_ceil(3) * 4 {
        return Err(FileSearchError::Unavailable(
            "Stored file exceeds the file size limit".into(),
        ));
    }
    STANDARD
        .decode(encoded)
        .map_err(|_| FileSearchError::Unavailable("Stored file content could not be decoded".into()))
}
fn validate_list(params: &ListParams) -> Result<(), FileSearchError> {
    if !(1..=100).contains(&params.limit.unwrap_or(20)) {
        return invalid("limit must be between 1 and 100");
    }
    if params.after.is_some() && params.before.is_some() {
        return invalid("provide only one of after and before");
    }
    if params
        .after
        .iter()
        .chain(params.before.iter())
        .any(|cursor| cursor.is_empty() || cursor.len() > 128)
    {
        return invalid("pagination cursors must contain 1 to 128 bytes");
    }
    Ok(())
}
fn page<T>(mut data: Vec<T>, params: &ListParams, id: impl Fn(&T) -> &str) -> ListResponse<T> {
    let limit = params.limit.unwrap_or(20);
    let has_more = data.len() > limit;
    data.truncate(limit);
    if params.before.is_some() {
        data.reverse();
    }
    ListResponse {
        object: "list".into(),
        first_id: data.first().map(|item| id(item).to_owned()),
        last_id: data.last().map(|item| id(item).to_owned()),
        data,
        has_more,
    }
}
