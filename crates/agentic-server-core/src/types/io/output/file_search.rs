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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<crate::types::file_search::FileAttributes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "file_citation")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FileCitation {
    pub file_id: String,
    pub filename: String,
    pub index: usize,
}

#[cfg(test)]
mod tests {
    use super::FileSearchCallResult;

    #[test]
    fn partial_search_results_preserve_only_supplied_fields() {
        let partial = serde_json::json!({"file_id": "file_1", "text": "excerpt"});
        let result: FileSearchCallResult = serde_json::from_value(partial.clone()).unwrap();
        assert!(result.filename.is_none());
        assert!(result.score.is_none());
        assert!(result.attributes.is_none());
        assert_eq!(serde_json::to_value(result).unwrap(), partial);

        let null_fields: FileSearchCallResult = serde_json::from_value(serde_json::json!({
            "attributes": null, "filename": null, "score": null
        }))
        .unwrap();
        assert_eq!(serde_json::to_value(null_fields).unwrap(), serde_json::json!({}));
    }
}
