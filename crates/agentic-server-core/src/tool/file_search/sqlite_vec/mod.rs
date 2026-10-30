//! SQLite vector index over the portable file-search metadata repository.
//!
//! Index rows are staged before metadata publication. Search joins them back to
//! published chunks, so an interrupted publication cannot expose partial data.

use std::{path::Path, str::FromStr, sync::Arc};

use futures::future::BoxFuture;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::provider::FileSearchRepository;
use crate::{
    storage::{
        DbPool,
        file_search::{
            Collection, FilePublicationFailure, FileSearchStorage, PreparedAttachment, StoredVectorStore, UploadedFile,
        },
    },
    types::file_search::{
        FileObject, FileSearchError, ListParams, SearchMode, SearchRequest, SearchResult, VectorStoreFileObject,
        VectorStoreObject,
    },
};

const INDEX_IDENTITY: &str = "sqlite-vec:cosine-f32:v1";

mod index;
mod search;

/// A `vec0` index backed by the same file-based SQLite database as metadata.
/// The extension path is deployment-controlled and must name a trusted binary.
pub struct SqliteVecRepository {
    metadata: FileSearchStorage,
    index: SqlitePool,
    operations: Mutex<()>,
}

impl SqliteVecRepository {
    /// Opens sqlite-vec on a file-based SQLite database after its ordinary
    /// migrations have completed. Other databases use another repository.
    ///
    /// # Errors
    /// Returns an error for an invalid extension, unsupported database URL,
    /// missing metadata schema, or failed SQLite connection.
    pub async fn open(pool: Arc<DbPool>, extension_path: &Path) -> Result<Self, FileSearchError> {
        let url = pool.connect_options().database_url.clone();
        if url.scheme() != "sqlite"
            || url.path().contains(":memory:")
            || url.query_pairs().any(|(key, value)| key == "mode" && value == "memory")
        {
            return Err(FileSearchError::InvalidRequest(
                "sqlite-vec requires a file-backed SQLite database".into(),
            ));
        }
        let path = extension_path
            .canonicalize()
            .map_err(|error| FileSearchError::Repository(Box::new(error)))?;
        if !path.is_file() {
            return Err(FileSearchError::InvalidRequest(
                "sqlite-vec extension path must name a regular file".into(),
            ));
        }
        let options = SqliteConnectOptions::from_str(url.as_str())?
            .extension(path.to_string_lossy().into_owned())
            .foreign_keys(true);
        let index = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        let _: String = sqlx::query_scalar("SELECT vec_version()").fetch_one(&index).await?;
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM file_search_chunks")
            .fetch_one(&index)
            .await?;
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS file_search_vec_map (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             store_id TEXT NOT NULL, file_id TEXT NOT NULL, chunk_index INTEGER NOT NULL, \
             created_at INTEGER NOT NULL, \
             UNIQUE(store_id, file_id, chunk_index)); \
             CREATE INDEX IF NOT EXISTS file_search_vec_map_file ON file_search_vec_map(file_id)",
        )
        .execute(&index)
        .await?;
        let repository = Self {
            metadata: FileSearchStorage::new(pool),
            index,
            operations: Mutex::new(()),
        };
        repository.recover_orphans().await?;
        Ok(repository)
    }
}

impl FileSearchRepository for SqliteVecRepository {
    fn index_identity(&self) -> Option<&str> {
        Some(INDEX_IDENTITY)
    }

    fn upload_file_metadata<'a>(
        &'a self,
        file: &'a FileObject,
        content_type: &'a str,
        cancelled: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), FilePublicationFailure>> {
        Box::pin(self.metadata.upload(file, content_type, cancelled))
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
            let _guard = self.operations.lock().await;
            self.stage(&object.id, attachments).await?;
            if let Err(error) = self
                .metadata
                .create_store(object, embedding_identity, attachments)
                .await
            {
                if let Err(cleanup) = self.remove_index_rows(Collection::Stores, &object.id, None).await {
                    tracing::warn!(%cleanup, store_id = %object.id, "sqlite-vec staging cleanup failed");
                }
                return Err(error);
            }
            Ok(())
        })
    }

    fn attach<'a>(
        &'a self,
        store_id: &'a str,
        embedding_identity: &'a str,
        attachment: &'a PreparedAttachment,
    ) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            let store = self.metadata.store(store_id).await?;
            if store.embedding_identity != embedding_identity
                || (store.embedding_dimensions != 0 && store.embedding_dimensions != attachment.dimensions)
            {
                return Err(FileSearchError::Conflict(
                    "Vector store embedding configuration changed".into(),
                ));
            }
            if self
                .metadata
                .attachment(store_id, &attachment.object.id)
                .await?
                .is_some()
            {
                return Err(FileSearchError::Conflict(
                    "File is already attached to this vector store".into(),
                ));
            }
            self.stage(store_id, std::slice::from_ref(attachment)).await?;
            if let Err(error) = self.metadata.attach(store_id, embedding_identity, attachment).await {
                if let Err(cleanup) = self
                    .remove_index_rows(Collection::Attachments, &attachment.object.id, Some(store_id))
                    .await
                {
                    tracing::warn!(%cleanup, %store_id, file_id = %attachment.object.id, "sqlite-vec staging cleanup failed");
                }
                return Err(error);
            }
            Ok(())
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
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            self.metadata.delete(collection, id, store_id).await?;
            // Metadata deletion is the visibility gate. Orphaned index rows are
            // excluded by the search join even if cleanup is interrupted.
            if let Err(error) = self.remove_index_rows(collection, id, store_id).await {
                tracing::warn!(%error, %id, "sqlite-vec orphan cleanup failed");
            }
            Ok(())
        })
    }

    fn pending_blob_deletion<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<bool, FileSearchError>> {
        Box::pin(self.metadata.pending_blob_deletion(id))
    }

    fn complete_blob_deletion<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(self.metadata.complete_blob_deletion(id))
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
            if mode == SearchMode::Semantic {
                let _guard = self.operations.lock().await;
                self.semantic_search(store_ids, queries, query_embeddings, expected_dimensions, request)
                    .await
            } else {
                self.metadata
                    .search(store_ids, queries, query_embeddings, expected_dimensions, mode, request)
                    .await
            }
        })
    }
}
