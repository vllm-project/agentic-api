//! Configuration-file sections for durable files and vector search.

use std::path::{Path, PathBuf};

use agentic_core::{error::Error, types::file_search::PgvectorConfig};
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
    pub pgvector: Option<PgvectorConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

impl FileSearchFileConfig {
    pub(super) fn validate(&self, path: &Path) -> Result<(), Error> {
        if self
            .sqlite_vec_extension_path
            .as_ref()
            .is_some_and(|extension| extension.as_os_str().is_empty())
        {
            return Err(Error::Config(format!(
                "configuration file {} contains an empty file_search.sqlite_vec_extension_path",
                path.display()
            )));
        }
        if self.api_key_env.as_deref().is_some_and(|name| name.trim().is_empty()) {
            return Err(Error::Config(format!(
                "configuration file {} contains an empty file_search.api_key_env",
                path.display()
            )));
        }
        if let Some(pgvector) = &self.pgvector {
            pgvector
                .validate()
                .map_err(|error| Error::Config(format!("configuration file {}: {error}", path.display())))?;
        }
        Ok(())
    }

    pub(super) fn is_empty(&self) -> bool {
        self.sqlite_vec_extension_path.is_none()
            && self.pgvector.is_none()
            && self.embedding_base_url.is_none()
            && self.embedding_model.is_none()
            && self.api_key_env.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::{FileSearchFileConfig, Path};

    #[test]
    fn typed_pgvector_configuration_is_retained_and_validated() {
        let input = r#"
[pgvector]
dimensions = 768
candidate_limit = 100
[pgvector.index]
type = "hnsw"
m = 16
ef_construction = 64
ef_search = 100
"#;
        let config: FileSearchFileConfig = toml::from_str(input).unwrap();
        assert!(!config.is_empty());
        config.validate(Path::new("config.toml")).unwrap();
        assert!(toml::to_string(&config).unwrap().contains("pgvector"));
        let invalid: FileSearchFileConfig = toml::from_str(&input.replace("m = 16", "m = 0")).unwrap();
        assert!(invalid.validate(Path::new("config.toml")).is_err());
        let conflicting: FileSearchFileConfig =
            toml::from_str(&format!("sqlite_vec_extension_path = '/vec0.so'\n{input}")).unwrap();
        // Provider selection is validated once by FileSearchService::configured.
        conflicting.validate(Path::new("config.toml")).unwrap();
    }
}
