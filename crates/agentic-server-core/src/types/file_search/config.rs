//! File and embedding configuration for the default and injected providers.

use std::{fmt, path::PathBuf};

use super::{FileSearchError, invalid};
use serde::{Deserialize, Serialize};

/// Deployment-controlled embedding connection. An absent connection selects keyword retrieval.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSearchConfig {
    pub files_storage_dir: Option<PathBuf>,
    pub sqlite_vec_extension_path: Option<PathBuf>,
    pub pgvector: Option<PgvectorConfig>,
    pub embedding_base_url: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_api_key: Option<String>,
}

impl fmt::Debug for FileSearchConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSearchConfig")
            .field("files_storage_dir", &self.files_storage_dir)
            .field("sqlite_vec_extension_path", &self.sqlite_vec_extension_path)
            .field("pgvector", &self.pgvector)
            .field("embedding_configured", &self.embedding_base_url.is_some())
            .field("embedding_model", &self.embedding_model)
            .field(
                "embedding_api_key",
                &self.embedding_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// PostgreSQL cosine index and bounded candidate retrieval configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgvectorConfig {
    pub dimensions: u16,
    pub index: PgvectorIndex,
    pub candidate_limit: u16,
}

/// Index construction options and per-search recall controls.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PgvectorIndex {
    Hnsw {
        m: u16,
        ef_construction: u16,
        ef_search: u16,
    },
    Ivfflat {
        lists: u16,
        probes: u16,
    },
}

impl PgvectorConfig {
    /// # Errors
    /// Rejects dimensions or index settings unsupported by the pgvector adapter.
    pub fn validate(&self) -> Result<(), FileSearchError> {
        if !(1..=2000).contains(&self.dimensions) || !(50..=1000).contains(&self.candidate_limit) {
            return invalid("pgvector dimensions must be 1 to 2000 and candidate_limit must be 50 to 1000");
        }
        match self.index {
            PgvectorIndex::Hnsw {
                m,
                ef_construction,
                ef_search,
            } => {
                if !(2..=100).contains(&m)
                    || !(4..=1000).contains(&ef_construction)
                    || ef_construction < 2 * m
                    || !(1..=1000).contains(&ef_search)
                {
                    return invalid(
                        "pgvector HNSW requires m 2 to 100, ef_construction 4 to 1000 and at least 2*m, and ef_search 1 to 1000",
                    );
                }
            }
            PgvectorIndex::Ivfflat { lists, probes } => {
                if !(2..=32768).contains(&lists) || probes == 0 || probes >= lists {
                    return invalid("pgvector IVFFlat requires lists 2 to 32768 and probes 1 to lists-1");
                }
            }
        }
        Ok(())
    }
}
