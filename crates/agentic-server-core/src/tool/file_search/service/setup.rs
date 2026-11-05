//! Construction of the default and injected file-search providers.

use std::sync::Arc;

use tokio::sync::Semaphore;

use super::FileSearchService;
use crate::{
    storage::{DbPool, file_search::FileSearchStorage, local_files::LocalFiles},
    tool::file_search::{
        embeddings::Embeddings,
        provider::{FileBlobStore, FileSearchRepository},
    },
    types::file_search::{FileSearchConfig, FileSearchError},
};

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
        let directory = match &config.files_storage_dir {
            Some(directory) => directory.clone(),
            None => crate::config::agentic_api_home()
                .map_err(|error| FileSearchError::Configuration(Box::new(error)))?
                .join("files"),
        };
        Self::with_providers(
            Arc::new(FileSearchStorage::new(pool)),
            Arc::new(LocalFiles::new(directory)?),
            client,
            &config,
        )
    }

    /// Constructs the service with caller-supplied metadata/index and blob
    /// providers. The default `new` constructor supplies SQL and local files.
    ///
    /// # Errors
    /// Returns invalid-request when embedding configuration is incomplete or malformed.
    pub fn with_providers(
        storage: Arc<dyn FileSearchRepository>,
        files: Arc<dyn FileBlobStore>,
        client: Arc<reqwest::Client>,
        config: &FileSearchConfig,
    ) -> Result<Self, FileSearchError> {
        if storage
            .index_identity()
            .is_some_and(|identity| identity.is_empty() || identity.len() > 256)
        {
            return Err(FileSearchError::InvalidRequest(
                "search provider identity must contain 1 to 256 bytes".into(),
            ));
        }
        Ok(Self {
            storage,
            files,
            embeddings: Embeddings::from_config(client, config)?,
            workers: Arc::new(Semaphore::new(4)),
        })
    }
}

pub(super) fn scoped_identity(index: Option<&str>, embedding: &str) -> String {
    match index {
        Some(index) => format!("index:{}:{index}:{embedding}", index.len()),
        None => embedding.to_owned(),
    }
}

pub(super) fn same_index(index: Option<&str>, stored: &str) -> bool {
    match index {
        Some(index) => stored.starts_with(&format!("index:{}:{index}:", index.len())),
        None => !stored.starts_with("index:"),
    }
}

#[cfg(test)]
mod tests {
    use super::{same_index, scoped_identity};

    #[test]
    fn index_identity_rejects_cross_provider_reuse_even_in_keyword_mode() {
        let external = scoped_identity(Some("milvus:tenant"), "keyword:cl100k_base:v1");
        let legacy = scoped_identity(None, "keyword:cl100k_base:v1");
        assert!(same_index(Some("milvus:tenant"), &external));
        assert!(!same_index(Some("milvus:other"), &external));
        assert!(!same_index(None, &external));
        assert!(!same_index(Some("milvus:tenant"), &legacy));
        assert!(same_index(None, &legacy));
    }
}
