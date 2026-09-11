//! In-tree ingestion, retrieval, and the Responses file search built-in tool.

mod embeddings;
pub(crate) mod handler;
mod ingest;
mod models;
mod ranking;
mod service;

pub use crate::types::file_search::FileSearchError;
pub use handler::{FileSearchExecutionParams, FileSearchExecutor, FileSearchHandler};
pub use service::{FileSearchService, MAX_FILE_BYTES};
