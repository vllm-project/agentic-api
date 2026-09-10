//! Typed contracts for durable files, vector stores, and retrieval.

use std::{collections::BTreeMap, fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

/// Deployment-controlled embedding connection. An absent connection selects keyword retrieval.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSearchConfig {
    pub files_storage_dir: Option<PathBuf>,
    pub embedding_base_url: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_api_key: Option<String>,
}

impl fmt::Debug for FileSearchConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSearchConfig")
            .field("files_storage_dir", &self.files_storage_dir)
            .field("embedding_configured", &self.embedding_base_url.is_some())
            .field("embedding_model", &self.embedding_model)
            .field(
                "embedding_api_key",
                &self.embedding_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Failures retain diagnostic sources without returning provider or database details to callers.
#[derive(Debug, thiserror::Error)]
pub enum FileSearchError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("{0}")]
    NotFound(String),
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
            Self::InvalidRequest(_) => 400,
            #[cfg(feature = "file-search-pdf")]
            Self::PdfParse(_) => 400,
            Self::NotFound(_) => 404,
            Self::Conflict(_) => 409,
            Self::Unavailable(_) | Self::FileStorage { .. } => 503,
            Self::Provider(_) | Self::ProviderProtocol | Self::ProviderDecode(_) => 502,
            Self::Storage(_) | Self::Serialization(_) | Self::Worker(_) | Self::Configuration(_) => 500,
        }
    }

    #[must_use]
    pub fn public_message(&self) -> String {
        match self {
            Self::InvalidRequest(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::Unavailable(message) => message.clone(),
            Self::Provider(_) | Self::ProviderProtocol | Self::ProviderDecode(_) => {
                "Embedding service request failed".into()
            }
            Self::Storage(_) | Self::Serialization(_) | Self::Worker(_) => "File search operation failed".into(),
            Self::FileStorage { .. } => {
                "Local file storage is unavailable; check the configured directory, permissions, and file integrity"
                    .into()
            }
            Self::Configuration(_) => "Local file storage configuration is invalid".into(),
            #[cfg(feature = "file-search-pdf")]
            Self::PdfParse(_) => "PDF text extraction failed; upload a valid unencrypted PDF containing text".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum AttributeValue {
    String(String),
    Number(f64),
    Boolean(bool),
}

pub type FileAttributes = BTreeMap<String, AttributeValue>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum FilterValue {
    Scalar(AttributeValue),
    List(Vec<AttributeValue>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum ComparisonOperator {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    Nin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum CompoundOperator {
    And,
    Or,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum SearchFilter {
    Comparison(ComparisonFilter),
    Compound(CompoundFilter),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ComparisonFilter {
    #[serde(rename = "type")]
    pub operator: ComparisonOperator,
    pub key: String,
    pub value: FilterValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CompoundFilter {
    #[serde(rename = "type")]
    pub operator: CompoundOperator,
    #[cfg_attr(feature = "openapi", schema(no_recursion))]
    pub filters: Vec<SearchFilter>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RankingOptions {
    pub ranker: Option<String>,
    pub score_threshold: Option<f64>,
    pub hybrid_search: Option<HybridSearchOptions>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct HybridSearchOptions {
    pub embedding_weight: Option<f64>,
    pub text_weight: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum SearchMode {
    #[serde(alias = "vector")]
    Semantic,
    Keyword,
    Hybrid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum SearchQuery {
    Text(String),
    Texts(Vec<String>),
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self::Texts(Vec::new())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SearchRequest {
    pub query: SearchQuery,
    pub max_num_results: Option<usize>,
    pub filters: Option<SearchFilter>,
    pub ranking_options: Option<RankingOptions>,
    pub search_mode: Option<SearchMode>,
    #[serde(default)]
    pub rewrite_query: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SearchResponse {
    pub object: String,
    pub search_query: Vec<String>,
    pub data: Vec<SearchResult>,
    pub has_more: bool,
    pub next_page: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SearchResult {
    pub file_id: String,
    pub filename: String,
    pub score: f64,
    pub attributes: FileAttributes,
    pub content: Vec<SearchContent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SearchContent {
    #[serde(rename = "type")]
    pub type_: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FileObject {
    pub id: String,
    pub object: String,
    pub bytes: i64,
    pub created_at: i64,
    pub filename: String,
    pub purpose: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum ChunkingStrategy {
    #[default]
    Auto,
    Static {
        #[serde(rename = "static")]
        config: StaticChunking,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct StaticChunking {
    pub max_chunk_size_tokens: usize,
    pub chunk_overlap_tokens: usize,
}

impl Default for StaticChunking {
    fn default() -> Self {
        Self {
            max_chunk_size_tokens: 800,
            chunk_overlap_tokens: 400,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CreateVectorStoreRequest {
    pub name: Option<String>,
    #[serde(default)]
    pub file_ids: Vec<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    pub chunking_strategy: Option<ChunkingStrategy>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AttachFileRequest {
    pub file_id: String,
    #[serde(default)]
    pub attributes: FileAttributes,
    pub chunking_strategy: Option<ChunkingStrategy>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FileCounts {
    pub in_progress: i64,
    pub completed: i64,
    pub failed: i64,
    pub cancelled: i64,
    pub total: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct VectorStoreObject {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub name: String,
    pub usage_bytes: i64,
    pub file_counts: FileCounts,
    pub status: String,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct VectorStoreFileObject {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub vector_store_id: String,
    pub status: String,
    pub usage_bytes: i64,
    pub attributes: FileAttributes,
    pub chunking_strategy: ChunkingStrategy,
    pub last_error: Option<VectorStoreFileError>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct VectorStoreFileError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeleteObject {
    pub id: String,
    pub object: String,
    pub deleted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ListResponse<T> {
    pub object: String,
    pub data: Vec<T>,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
    pub has_more: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum ListOrder {
    Asc,
    #[default]
    Desc,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ListParams {
    pub limit: Option<usize>,
    pub after: Option<String>,
    pub before: Option<String>,
    pub order: Option<ListOrder>,
}

impl SearchRequest {
    /// Validates query, ranking, and filter bounds before retrieval or inference.
    ///
    /// # Errors
    /// Returns an invalid-request error for unsupported options or exceeded limits.
    pub fn validate(&self) -> Result<(), FileSearchError> {
        let queries = match &self.query {
            SearchQuery::Text(query) => std::slice::from_ref(query),
            SearchQuery::Texts(queries) => queries,
        };
        if queries.is_empty()
            || queries.len() > 16
            || queries
                .iter()
                .any(|query| query.trim().is_empty() || query.len() > 4096)
        {
            return invalid("query must contain 1 to 16 nonempty strings of at most 4096 bytes each");
        }
        if !(1..=50).contains(&self.max_num_results.unwrap_or(10)) {
            return invalid("max_num_results must be between 1 and 50");
        }
        if self.rewrite_query {
            return invalid("rewrite_query is not supported by in-tree file search");
        }
        if let Some(options) = &self.ranking_options {
            options.validate()?;
        }
        if let Some(filter) = &self.filters {
            filter.validate_at(0, &mut 0)?;
        }
        Ok(())
    }
}

impl RankingOptions {
    /// # Errors
    /// Returns an invalid-request error for unsupported ranking or invalid weights.
    pub fn validate(&self) -> Result<(), FileSearchError> {
        if self
            .ranker
            .as_deref()
            .is_some_and(|ranker| !matches!(ranker, "auto" | "none"))
        {
            return invalid("ranker must be auto or none; neural and classifier reranking are not supported");
        }
        if self
            .score_threshold
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return invalid("score_threshold must be between 0 and 1");
        }
        if let Some(weights) = &self.hybrid_search {
            let embedding = weights.embedding_weight.unwrap_or(1.0);
            let text = weights.text_weight.unwrap_or(1.0);
            if !embedding.is_finite()
                || !text.is_finite()
                || embedding < 0.0
                || text < 0.0
                || !((embedding + text).is_finite())
                || embedding + text == 0.0
            {
                return invalid("hybrid weights must be finite and nonnegative, with a positive sum");
            }
        }
        Ok(())
    }
}

pub(crate) fn invalid<T>(message: &str) -> Result<T, FileSearchError> {
    Err(FileSearchError::InvalidRequest(message.into()))
}

pub(crate) fn validate_attributes(attributes: &FileAttributes) -> Result<(), FileSearchError> {
    if attributes.len() > 16 {
        return invalid("attributes accepts at most 16 keys");
    }
    for (key, value) in attributes {
        if key.is_empty() || key.len() > 64 {
            return invalid("attribute keys must contain 1 to 64 bytes");
        }
        validate_attribute_value(value)?;
    }
    Ok(())
}

fn validate_attribute_value(value: &AttributeValue) -> Result<(), FileSearchError> {
    match value {
        AttributeValue::String(value) if value.len() > 512 => invalid("attribute values must not exceed 512 bytes"),
        AttributeValue::Number(value) if !value.is_finite() => invalid("attribute numbers must be finite"),
        _ => Ok(()),
    }
}

impl SearchFilter {
    fn validate_at(&self, depth: usize, nodes: &mut usize) -> Result<(), FileSearchError> {
        *nodes += 1;
        if depth > 8 || *nodes > 64 {
            return invalid("filters exceed the maximum depth of 8 or 64 total nodes");
        }
        match self {
            Self::Compound(filter) => {
                if filter.filters.is_empty() {
                    return invalid("compound filters must contain at least one filter");
                }
                for child in &filter.filters {
                    child.validate_at(depth + 1, nodes)?;
                }
            }
            Self::Comparison(filter) => {
                if filter.key.is_empty() || filter.key.len() > 64 {
                    return invalid("filter keys must contain 1 to 64 bytes");
                }
                match (&filter.operator, &filter.value) {
                    (ComparisonOperator::In | ComparisonOperator::Nin, FilterValue::List(values))
                        if !values.is_empty() && values.len() <= 64 =>
                    {
                        for value in values {
                            validate_attribute_value(value)?;
                        }
                    }
                    (ComparisonOperator::Eq | ComparisonOperator::Ne, FilterValue::Scalar(value)) => {
                        validate_attribute_value(value)?;
                    }
                    (
                        ComparisonOperator::Gt
                        | ComparisonOperator::Gte
                        | ComparisonOperator::Lt
                        | ComparisonOperator::Lte,
                        FilterValue::Scalar(value @ (AttributeValue::String(_) | AttributeValue::Number(_))),
                    ) => validate_attribute_value(value)?,
                    _ => return invalid("filter value does not match its comparison operator"),
                }
            }
        }
        Ok(())
    }

    pub(crate) fn matches(&self, attributes: &FileAttributes) -> bool {
        match self {
            Self::Compound(filter) => match filter.operator {
                CompoundOperator::And => filter.filters.iter().all(|filter| filter.matches(attributes)),
                CompoundOperator::Or => filter.filters.iter().any(|filter| filter.matches(attributes)),
            },
            Self::Comparison(filter) => {
                let Some(actual) = attributes.get(&filter.key) else {
                    return false;
                };
                match (&filter.operator, &filter.value) {
                    (ComparisonOperator::Eq, FilterValue::Scalar(value)) => actual == value,
                    (ComparisonOperator::Ne, FilterValue::Scalar(value)) => same_type(actual, value) && actual != value,
                    (ComparisonOperator::In, FilterValue::List(values)) => values.contains(actual),
                    (ComparisonOperator::Nin, FilterValue::List(values)) => !values.contains(actual),
                    (operator, FilterValue::Scalar(value)) => {
                        let ordering = match (actual, value) {
                            (AttributeValue::Number(left), AttributeValue::Number(right)) => left.partial_cmp(right),
                            (AttributeValue::String(left), AttributeValue::String(right)) => Some(left.cmp(right)),
                            _ => None,
                        };
                        ordering.is_some_and(|ordering| match operator {
                            ComparisonOperator::Gt => ordering.is_gt(),
                            ComparisonOperator::Gte => !ordering.is_lt(),
                            ComparisonOperator::Lt => ordering.is_lt(),
                            ComparisonOperator::Lte => !ordering.is_gt(),
                            _ => false,
                        })
                    }
                    _ => false,
                }
            }
        }
    }
}

fn same_type(left: &AttributeValue, right: &AttributeValue) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right)
}
