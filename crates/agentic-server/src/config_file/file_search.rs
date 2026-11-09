//! Configuration-file sections for durable files and vector search.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FilesFileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_dir: Option<PathBuf>,
}

impl FilesFileConfig {
    pub(super) fn is_empty(&self) -> bool {
        self.storage_dir.is_none()
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileSearchFileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sqlite_vec_extension_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

impl FileSearchFileConfig {
    pub(super) fn is_empty(&self) -> bool {
        self.sqlite_vec_extension_path.is_none()
            && self.embedding_base_url.is_none()
            && self.embedding_model.is_none()
            && self.api_key_env.is_none()
    }
}
