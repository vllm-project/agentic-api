//! File and embedding configuration for the default and injected providers.

use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

/// Deployment-controlled embedding connection. An absent connection selects keyword retrieval.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSearchConfig {
    pub files_storage_dir: Option<PathBuf>,
    pub sqlite_vec_extension_path: Option<PathBuf>,
    pub embedding_base_url: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_api_key: Option<String>,
}

impl fmt::Debug for FileSearchConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSearchConfig")
            .field("files_storage_dir", &self.files_storage_dir)
            .field("sqlite_vec_extension_path", &self.sqlite_vec_extension_path)
            .field("embedding_configured", &self.embedding_base_url.is_some())
            .field("embedding_model", &self.embedding_model)
            .field(
                "embedding_api_key",
                &self.embedding_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}
