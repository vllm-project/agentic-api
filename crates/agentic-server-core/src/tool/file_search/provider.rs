//! Storage contracts for file search. Implementations own their publication and
//! deletion consistency; the service owns API validation and ingestion.

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::storage::file_search::{
    Collection, FilePublicationFailure, FileSearchStorage, PreparedAttachment, StoredVectorStore, UploadedFile,
};
use crate::storage::local_files::LocalFiles;
use crate::types::file_search::{
    FileObject, FileSearchError, ListParams, SearchMode, SearchRequest, SearchResult, VectorStoreFileObject,
    VectorStoreObject,
};

/// A durable blob store addressed only by generated file IDs. `publish` must
/// create without replacing an existing object, and a successful return means
/// the bytes can be read after a process restart. `remove` is idempotent.
/// Failed or cancelled publication must remove partial bytes. Implementations
/// must bound reads by `max_bytes` and check `expected_bytes`.
pub trait FileBlobStore: Send + Sync {
    fn publish<'a>(
        &'a self,
        id: &'a str,
        bytes: &'a [u8],
        cancelled: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), FileSearchError>>;

    fn read<'a>(
        &'a self,
        id: &'a str,
        expected_bytes: i64,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>, FileSearchError>>;

    fn remove<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), FileSearchError>>;
}

/// Durable file and vector-store metadata plus a searchable chunk index.
/// Implementations may use one database or compose a metadata database with an
/// external index. `create_store` and `attach` must expose only fully indexed
/// attachments; deletion must remove their searchable chunks. In particular,
/// an external index needs recovery or compensation for cross-system failures.
/// Upload must record durable cleanup intent before publishing bytes. File
/// deletion must record it in the same durable commit that removes metadata.
/// The service clears intent only after metadata is live or bytes are removed.
/// List methods return at most `limit + 1` rows in keyset order; `list_stores`
/// must return current file counts and usage for each row. The service builds
/// the response page and reverses `before` results.
pub trait FileSearchRepository: Send + Sync {
    /// Stable identity of the chunk index. `None` is reserved for the original
    /// SQL implementation so its existing vector stores remain compatible.
    /// Other implementations return a nonempty name that changes when their
    /// index is replaced with incompatible data.
    fn index_identity(&self) -> Option<&str>;

    fn upload_file_metadata<'a>(
        &'a self,
        file: &'a FileObject,
        content_type: &'a str,
        cancelled: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), FilePublicationFailure>>;

    fn file<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<UploadedFile, FileSearchError>>;
    fn file_object<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<FileObject, FileSearchError>>;
    fn store<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<StoredVectorStore, FileSearchError>>;
    fn store_object<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<VectorStoreObject, FileSearchError>>;

    fn create_store<'a>(
        &'a self,
        object: &'a VectorStoreObject,
        embedding_identity: &'a str,
        attachments: &'a [PreparedAttachment],
    ) -> BoxFuture<'a, Result<(), FileSearchError>>;

    fn attach<'a>(
        &'a self,
        store_id: &'a str,
        embedding_identity: &'a str,
        attachment: &'a PreparedAttachment,
    ) -> BoxFuture<'a, Result<(), FileSearchError>>;

    fn attachment<'a>(
        &'a self,
        store_id: &'a str,
        file_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<VectorStoreFileObject>, FileSearchError>>;

    fn delete<'a>(
        &'a self,
        collection: Collection,
        id: &'a str,
        store_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), FileSearchError>>;

    fn stage_blob_cleanup<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), FileSearchError>>;
    fn pending_blob_cleanup<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<bool, FileSearchError>>;
    fn complete_blob_cleanup<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), FileSearchError>>;
    fn blob_cleanup_batch<'a>(
        &'a self,
        after: &'a str,
        limit: i64,
    ) -> BoxFuture<'a, Result<Vec<String>, FileSearchError>>;

    fn list_files<'a>(&'a self, params: &'a ListParams) -> BoxFuture<'a, Result<Vec<FileObject>, FileSearchError>>;
    fn list_stores<'a>(
        &'a self,
        params: &'a ListParams,
    ) -> BoxFuture<'a, Result<Vec<VectorStoreObject>, FileSearchError>>;
    fn list_attachments<'a>(
        &'a self,
        store_id: &'a str,
        params: &'a ListParams,
    ) -> BoxFuture<'a, Result<Vec<VectorStoreFileObject>, FileSearchError>>;

    /// Allows an empty semantic store to return without calling embeddings.
    fn has_chunks<'a>(&'a self, store_ids: &'a [String]) -> BoxFuture<'a, Result<bool, FileSearchError>>;

    /// Apply filters, ranking, deduplication, threshold, and the global result
    /// limit. Index-backed implementations may retrieve candidates natively.
    fn search<'a>(
        &'a self,
        store_ids: &'a [String],
        queries: &'a [String],
        query_embeddings: &'a [Vec<f64>],
        expected_dimensions: Option<usize>,
        mode: SearchMode,
        request: &'a SearchRequest,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, FileSearchError>>;
}

impl FileBlobStore for LocalFiles {
    fn publish<'a>(
        &'a self,
        id: &'a str,
        bytes: &'a [u8],
        cancelled: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(LocalFiles::publish(self, id, bytes, cancelled))
    }

    fn read<'a>(
        &'a self,
        id: &'a str,
        expected_bytes: i64,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>, FileSearchError>> {
        Box::pin(LocalFiles::read(self, id, expected_bytes, max_bytes))
    }

    fn remove<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(LocalFiles::remove(self, id))
    }
}

impl FileSearchRepository for FileSearchStorage {
    fn index_identity(&self) -> Option<&str> {
        None
    }

    fn upload_file_metadata<'a>(
        &'a self,
        file: &'a FileObject,
        content_type: &'a str,
        cancelled: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), FilePublicationFailure>> {
        Box::pin(FileSearchStorage::upload(self, file, content_type, cancelled))
    }

    fn file<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<UploadedFile, FileSearchError>> {
        Box::pin(FileSearchStorage::file(self, id))
    }

    fn file_object<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<FileObject, FileSearchError>> {
        Box::pin(FileSearchStorage::file_object(self, id))
    }

    fn store<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<StoredVectorStore, FileSearchError>> {
        Box::pin(FileSearchStorage::store(self, id))
    }

    fn store_object<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<VectorStoreObject, FileSearchError>> {
        Box::pin(FileSearchStorage::store_object(self, id))
    }

    fn create_store<'a>(
        &'a self,
        object: &'a VectorStoreObject,
        embedding_identity: &'a str,
        attachments: &'a [PreparedAttachment],
    ) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(FileSearchStorage::create_store(
            self,
            object,
            embedding_identity,
            attachments,
        ))
    }

    fn attach<'a>(
        &'a self,
        store_id: &'a str,
        embedding_identity: &'a str,
        attachment: &'a PreparedAttachment,
    ) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(FileSearchStorage::attach(
            self,
            store_id,
            embedding_identity,
            attachment,
        ))
    }

    fn attachment<'a>(
        &'a self,
        store_id: &'a str,
        file_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<VectorStoreFileObject>, FileSearchError>> {
        Box::pin(FileSearchStorage::attachment(self, store_id, file_id))
    }

    fn delete<'a>(
        &'a self,
        collection: Collection,
        id: &'a str,
        store_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(FileSearchStorage::delete(self, collection, id, store_id))
    }

    fn stage_blob_cleanup<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(FileSearchStorage::stage_blob_cleanup(self, id))
    }

    fn pending_blob_cleanup<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<bool, FileSearchError>> {
        Box::pin(FileSearchStorage::pending_blob_cleanup(self, id))
    }

    fn complete_blob_cleanup<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), FileSearchError>> {
        Box::pin(FileSearchStorage::complete_blob_cleanup(self, id))
    }

    fn blob_cleanup_batch<'a>(
        &'a self,
        after: &'a str,
        limit: i64,
    ) -> BoxFuture<'a, Result<Vec<String>, FileSearchError>> {
        Box::pin(FileSearchStorage::blob_cleanup_batch(self, after, limit))
    }

    fn list_files<'a>(&'a self, params: &'a ListParams) -> BoxFuture<'a, Result<Vec<FileObject>, FileSearchError>> {
        Box::pin(FileSearchStorage::list(self, Collection::Files, None, params))
    }

    fn list_stores<'a>(
        &'a self,
        params: &'a ListParams,
    ) -> BoxFuture<'a, Result<Vec<VectorStoreObject>, FileSearchError>> {
        Box::pin(FileSearchStorage::list_stores(self, params))
    }

    fn list_attachments<'a>(
        &'a self,
        store_id: &'a str,
        params: &'a ListParams,
    ) -> BoxFuture<'a, Result<Vec<VectorStoreFileObject>, FileSearchError>> {
        Box::pin(FileSearchStorage::list(
            self,
            Collection::Attachments,
            Some(store_id),
            params,
        ))
    }

    fn has_chunks<'a>(&'a self, store_ids: &'a [String]) -> BoxFuture<'a, Result<bool, FileSearchError>> {
        Box::pin(FileSearchStorage::has_chunks(self, store_ids))
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
            let chunks = self.chunks(store_ids).await?;
            if mode != SearchMode::Keyword {
                if !chunks.is_empty() && query_embeddings.len() != queries.len() {
                    return Err(FileSearchError::Unavailable(
                        "Indexed content changed during search; retry the request".into(),
                    ));
                }
                let mut dimension = expected_dimensions;
                for chunk in &chunks {
                    let vector = chunk.embedding.as_ref().ok_or_else(|| {
                        FileSearchError::Unavailable("Stored embeddings are missing; recreate this vector store".into())
                    })?;
                    if dimension.is_some_and(|expected| expected != vector.len())
                        || vector.is_empty()
                        || vector.iter().any(|value| !value.is_finite())
                    {
                        return Err(FileSearchError::Unavailable(
                            "Stored embeddings are incompatible; recreate this vector store".into(),
                        ));
                    }
                    dimension = Some(vector.len());
                }
            }
            let queries = queries.to_vec();
            let vectors = query_embeddings.to_vec();
            let request = request.clone();
            tokio::task::spawn_blocking(move || super::ranking::rank(chunks, &queries, &vectors, mode, &request))
                .await
                .map_err(Into::into)
        })
    }
}
