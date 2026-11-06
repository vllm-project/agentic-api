//! Public file search output items and citations.

use serde::{Deserialize, Serialize};

use super::{AgentAttribution, GatewayCallStatus};

/// Lifecycle status of a `file_search_call` output item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum FileSearchCallStatus {
    InProgress,
    Searching,
    Completed,
    Incomplete,
    Failed,
}

impl From<GatewayCallStatus> for FileSearchCallStatus {
    fn from(status: GatewayCallStatus) -> Self {
        match status {
            GatewayCallStatus::InProgress => Self::InProgress,
            GatewayCallStatus::Completed => Self::Completed,
            GatewayCallStatus::Failed => Self::Failed,
        }
    }
}

/// A retrieved passage exposed when `file_search_call.results` is requested.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FileSearchCallResult {
    pub file_id: String,
    pub filename: String,
    pub score: f64,
    pub attributes: crate::types::file_search::FileAttributes,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FileSearchCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    pub id: String,
    pub status: FileSearchCallStatus,
    pub queries: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<FileSearchCallResult>>,
}

/// Annotation for a file explicitly cited by the model from retrieved context.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename = "file_citation")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FileCitation {
    pub file_id: String,
    pub filename: String,
    pub index: usize,
}
