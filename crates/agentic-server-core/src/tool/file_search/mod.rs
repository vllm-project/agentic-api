//! In-tree ingestion, retrieval, and the Responses file search built-in tool.

mod embeddings;
pub(crate) mod handler;
mod ingest;
mod provider;
mod ranking;
mod service;
mod sqlite_vec;

pub use crate::storage::file_search::{
    Collection, FilePublicationFailure, FileSearchStorage as SqlFileSearchRepository, PreparedAttachment, StoredChunk,
    StoredFileContent, StoredVectorStore, UploadedFile,
};
pub use crate::storage::local_files::LocalFiles as LocalFileBlobStore;
pub use crate::types::file_search::FileSearchError;
pub use handler::{FileSearchExecutionParams, FileSearchExecutor, FileSearchHandler};
pub use provider::{FileBlobStore, FileSearchRepository};
pub use service::{FileSearchService, MAX_FILE_BYTES};
pub use sqlite_vec::SqliteVecRepository;
