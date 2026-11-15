//! pgvector adapter over the portable metadata and blob-cleanup repository.
//!
//! A generated column projects vectors in the metadata transaction, so failed
//! publication and cascading deletion cannot leave searchable index orphans.

use super::{provider::FileSearchRepository, ranking};
use crate::{
    storage::{
        DbPool,
        file_search::{
            Collection, FilePublicationFailure, FileSearchStorage, PreparedAttachment, StoredVectorStore, UploadedFile,
            pgvector::{PgvectorStorage, validate_vector},
        },
    },
    types::file_search::{
        FileObject, FileSearchError, ListParams, PgvectorConfig, SearchMode, SearchRequest, SearchResult,
        VectorStoreFileObject, VectorStoreObject, invalid,
    },
};
use futures::future::BoxFuture;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// PostgreSQL metadata with native HNSW or `IVFFlat` cosine candidates.
/// Implements the same provider contract as SQL exact search and sqlite-vec.
/// Existing exact-search stores must be recreated when changing providers.
pub struct PgvectorRepository {
    metadata: FileSearchStorage,
    pool: Arc<DbPool>,
    index: PgvectorStorage,
    identity: String,
}

impl PgvectorRepository {
    /// Initializes the optional projection and indexes after ordinary migrations.
    ///
    /// # Errors
    /// Rejects non-PostgreSQL databases, invalid settings, pgvector older than
    /// 0.8.0, unavailable DDL privileges, or incompatible stored vectors.
    pub async fn open(pool: Arc<DbPool>, config: &PgvectorConfig) -> Result<Self, FileSearchError> {
        let index = PgvectorStorage::open(&pool, config).await?;
        Ok(Self {
            metadata: FileSearchStorage::new(pool.clone()),
            pool,
            index,
            identity: format!("pgvector:cosine-f32:v1:{}", config.dimensions),
        })
    }

    fn validate_attachment(&self, attachment: &PreparedAttachment) -> Result<(), FileSearchError> {
        if attachment.dimensions != 0
            && attachment.dimensions
                != i64::try_from(self.index.dimensions()).map_err(|_| FileSearchError::ProviderProtocol)?
        {
            return invalid("pgvector embeddings must match configured dimensions");
        }
        for chunk in &attachment.chunks {
            match &chunk.embedding {
                Some(vector) if attachment.dimensions != 0 => validate_vector(vector, self.index.dimensions())?,
                None if attachment.dimensions == 0 => {}
                _ => return invalid("pgvector attachment embeddings do not match its dimensions"),
            }
        }
        Ok(())
    }
}

impl FileSearchRepository for PgvectorRepository {
    fn index_identity(&self) -> Option<&str> {
        Some(&self.identity)
    }

    fn upload_file_metadata<'a>(
        &'a self,
        file: &'a FileObject,
        content_type: &'a str,
        owner_token: &'a str,
        cancelled: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), FilePublicationFailure>> {
        Box::pin(self.metadata.upload(file, content_type, owner_token, cancelled))
    }

    fn file<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<UploadedFile, FileSearchError>> {
        Box::pin(self.metadata.file(id))
    }

    fn file_object<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<FileObject, FileSearchError>> {
        Box::pin(self.metadata.file_object(id))
    }

    fn store<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<StoredVectorStore, FileSearchError>> {
        Box::pin(self.metadata.store(id))
    }

    fn store_object<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<VectorStoreObject, FileSearchError>> {
        Box::pin(self.metadata.store_object(id))
    }

    fn create_store<'a>(
        &'a self,
        object: &'a VectorStoreObject,
        embedding_identity: &'a str,
        attachments: &'a [PreparedAttachment],
    ) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(async move {
            for attachment in attachments {
                self.validate_attachment(attachment)?;
            }
            self.metadata
                .create_store(object, embedding_identity, attachments)
                .await
        })
    }

    fn attach<'a>(
        &'a self,
        store_id: &'a str,
        embedding_identity: &'a str,
        attachment: &'a PreparedAttachment,
    ) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(async move {
            self.validate_attachment(attachment)?;
            self.metadata.attach(store_id, embedding_identity, attachment).await
        })
    }

    fn attachment<'a>(
        &'a self,
        store_id: &'a str,
        file_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<VectorStoreFileObject>, FileSearchError>> {
        Box::pin(self.metadata.attachment(store_id, file_id))
    }

    fn delete<'a>(
        &'a self,
        collection: Collection,
        id: &'a str,
        store_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(self.metadata.delete(collection, id, store_id))
    }

    fn stage_blob_cleanup<'a>(
        &'a self,
        id: &'a str,
        owner_token: &'a str,
        lease_expires_at: i64,
    ) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(self.metadata.stage_blob_cleanup(id, owner_token, lease_expires_at))
    }

    fn refresh_blob_cleanup<'a>(
        &'a self,
        id: &'a str,
        owner_token: &'a str,
        lease_expires_at: i64,
    ) -> BoxFuture<'a, Result<bool, FileSearchError>> {
        Box::pin(self.metadata.refresh_blob_cleanup(id, owner_token, lease_expires_at))
    }

    fn pending_blob_cleanup<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<String>, FileSearchError>> {
        Box::pin(self.metadata.pending_blob_cleanup(id))
    }

    fn complete_blob_cleanup<'a>(
        &'a self,
        id: &'a str,
        owner_token: &'a str,
    ) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(self.metadata.complete_blob_cleanup(id, owner_token))
    }

    fn blob_cleanup_batch(&self, limit: i64, now: i64) -> BoxFuture<'_, Result<Vec<String>, FileSearchError>> {
        Box::pin(self.metadata.blob_cleanup_batch(limit, now))
    }

    fn claim_blob_cleanup<'a>(
        &'a self,
        id: &'a str,
        owner_token: &'a str,
        now: i64,
        lease_expires_at: i64,
    ) -> BoxFuture<'a, Result<bool, FileSearchError>> {
        Box::pin(self.metadata.claim_blob_cleanup(id, owner_token, now, lease_expires_at))
    }

    fn list_files<'a>(&'a self, params: &'a ListParams) -> BoxFuture<'a, Result<Vec<FileObject>, FileSearchError>> {
        Box::pin(self.metadata.list(Collection::Files, None, params))
    }

    fn list_stores<'a>(
        &'a self,
        params: &'a ListParams,
    ) -> BoxFuture<'a, Result<Vec<VectorStoreObject>, FileSearchError>> {
        Box::pin(self.metadata.list_stores(params))
    }

    fn list_attachments<'a>(
        &'a self,
        store_id: &'a str,
        params: &'a ListParams,
    ) -> BoxFuture<'a, Result<Vec<VectorStoreFileObject>, FileSearchError>> {
        Box::pin(self.metadata.list(Collection::Attachments, Some(store_id), params))
    }

    fn has_chunks<'a>(&'a self, store_ids: &'a [String]) -> BoxFuture<'a, Result<bool, FileSearchError>> {
        Box::pin(self.metadata.has_chunks(store_ids))
    }

    fn search<'a>(
        &'a self,
        store_ids: &'a [String],
        queries: &'a [String],
        query_embeddings: &'a [Vec<f64>],
        expected_dimensions: Option<usize>,
        mode: SearchMode,
        request: &'a SearchRequest,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, FileSearchError>> {
        Box::pin(async move {
            request.validate()?;
            if store_ids.is_empty() {
                return Ok(Vec::new());
            }
            if mode != SearchMode::Keyword
                && (expected_dimensions.is_some_and(|n| n != self.index.dimensions())
                    || query_embeddings.len() != queries.len())
            {
                return Err(FileSearchError::InvalidRequest(
                    "pgvector query dimensions or embedding count do not match".into(),
                ));
            }
            let chunks = self
                .index
                .candidates(
                    &self.pool,
                    store_ids,
                    queries,
                    query_embeddings,
                    mode,
                    request.filters.as_ref(),
                )
                .await?;
            let queries = queries.to_vec();
            let vectors = query_embeddings.to_vec();
            let request = request.clone();
            tokio::task::spawn_blocking(move || ranking::rank(chunks, &queries, &vectors, mode, &request))
                .await
                .map_err(Into::into)
        })
    }
}
