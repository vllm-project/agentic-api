//! File and vector-store failures, with their public status and message.

use super::FileRowParam;

/// Failures retain diagnostic sources without returning provider or database details to callers.
#[derive(Debug, thiserror::Error)]
pub enum FileSearchError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("{0}")]
    NotFound(String),
    /// A Files API row lookup that matches no stored file, as the recorded row reports it.
    #[error("No such File object: {id}")]
    FileNotFound { id: String, param: FileRowParam },
    /// A Files API request the recorded rows reject: `invalid_request_error`, `code: null`.
    #[error("{message}")]
    FileRowRejected {
        message: String,
        param: Option<FileRowParam>,
    },
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("local file storage {operation} failed")]
    FileStorage {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("local file storage configuration failed: {0}")]
    Configuration(#[source] Box<crate::error::Error>),
    #[error("file blob store failed")]
    BlobStore(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("file search repository failed")]
    Repository(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("file search storage failed")]
    Storage(#[from] sqlx::Error),
    #[error("stored file search data could not be decoded")]
    Serialization(#[from] serde_json::Error),
    #[error("embedding provider request failed")]
    Provider(#[source] reqwest::Error),
    #[error("embedding provider returned an invalid response")]
    ProviderProtocol,
    #[error("embedding provider returned malformed JSON")]
    ProviderDecode(#[source] serde_json::Error),
    #[error("file search worker failed")]
    Worker(#[from] tokio::task::JoinError),
    #[cfg(feature = "file-search-pdf")]
    #[error("PDF parsing failed")]
    PdfParse(#[source] lopdf::Error),
}

impl FileSearchError {
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        match self {
            Self::InvalidRequest(_) | Self::FileRowRejected { .. } => 400,
            #[cfg(feature = "file-search-pdf")]
            Self::PdfParse(_) => 400,
            Self::NotFound(_) | Self::FileNotFound { .. } => 404,
            Self::Conflict(_) => 409,
            Self::Unavailable(_) | Self::FileStorage { .. } | Self::BlobStore(_) => 503,
            Self::Provider(_) | Self::ProviderProtocol | Self::ProviderDecode(_) => 502,
            Self::Storage(_)
            | Self::Serialization(_)
            | Self::Worker(_)
            | Self::Configuration(_)
            | Self::Repository(_) => 500,
        }
    }

    /// The request parameter a Files API row error names.
    #[must_use]
    pub const fn row_param(&self) -> Option<FileRowParam> {
        match self {
            Self::FileNotFound { param, .. } => Some(*param),
            Self::FileRowRejected { param, .. } => *param,
            _ => None,
        }
    }

    #[must_use]
    pub fn public_message(&self) -> String {
        match self {
            Self::InvalidRequest(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::Unavailable(message) => message.clone(),
            Self::FileNotFound { id, .. } => format!("No such File object: {id}"),
            Self::FileRowRejected { message, .. } => message.clone(),
            Self::Provider(_) | Self::ProviderProtocol | Self::ProviderDecode(_) => {
                "Embedding service request failed".into()
            }
            Self::Storage(_) | Self::Serialization(_) | Self::Worker(_) => "File search operation failed".into(),
            Self::FileStorage { .. } => {
                "Local file storage is unavailable; check the configured directory, permissions, and file integrity"
                    .into()
            }
            Self::BlobStore(_) => "File storage is unavailable".into(),
            Self::Repository(_) => "File search operation failed".into(),
            Self::Configuration(_) => "Local file storage configuration is invalid".into(),
            #[cfg(feature = "file-search-pdf")]
            Self::PdfParse(_) => "PDF text extraction failed; upload a valid unencrypted PDF containing text".into(),
        }
    }
}
