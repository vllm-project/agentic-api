//! pgvector adapter over the portable metadata and blob-cleanup repository.
//!
//! A generated column projects vectors in the metadata transaction, so failed
//! publication and cascading deletion cannot leave searchable index orphans.

use super::{provider::FileSearchRepository, ranking};
use crate::{
    storage::{
        DbPool,
        file_search::{
            FileSearchStorage, PreparedAttachment,
            pgvector::{PgvectorStorage, validate_vector},
        },
    },
    types::file_search::{
        FileSearchError, PgvectorConfig, SearchMode, SearchRequest, SearchResult, VectorStoreObject, invalid,
    },
};
use futures::future::BoxFuture;
use std::sync::Arc;

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
    fn metadata(&self) -> &FileSearchStorage {
        &self.metadata
    }

    fn index_identity(&self) -> Option<&str> {
        Some(&self.identity)
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
