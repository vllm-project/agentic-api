//! Typed contracts for durable files, vector stores, and retrieval.

use std::{collections::BTreeMap, fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

pub use super::vector_stores::*;

/// Deployment-controlled embedding connection. An absent connection selects keyword retrieval.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSearchConfig {
    #[serde(default)]
    pub vector_stores: VectorStoresConfig,
    #[serde(default)]
    pub backend: FileSearchBackend,
    pub files_storage_dir: Option<PathBuf>,
    pub embedding_base_url: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_api_key: Option<String>,
}

impl fmt::Debug for FileSearchConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSearchConfig")
            .field("backend", &self.backend)
            .field("vector_stores", &self.vector_stores)
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

/// Deployment-selected retrieval storage. Exact SQL remains portable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileSearchBackend {
    #[default]
    Exact,
    Pgvector {
        dimensions: u16,
        index: PgvectorIndex,
        candidate_limit: u16,
    },
}

/// Cosine ANN index construction and transaction-local search settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

impl FileSearchBackend {
    /// Validates bounded pgvector dimensions and index/search parameters.
    ///
    /// # Errors
    /// Returns an actionable configuration error for invalid settings.
    pub fn validate(&self) -> Result<(), FileSearchError> {
        if let Self::Pgvector {
            dimensions,
            index,
            candidate_limit,
        } = self
        {
            if !(1..=2000).contains(dimensions) || !(50..=1000).contains(candidate_limit) {
                return invalid("pgvector dimensions must be 1 to 2000 and candidate_limit 50 to 1000");
            }
            let valid = match index {
                PgvectorIndex::Hnsw {
                    m,
                    ef_construction,
                    ef_search,
                } => {
                    (2..=100).contains(m)
                        && (4..=1000).contains(ef_construction)
                        && *ef_construction >= 2 * m
                        && (1..=1000).contains(ef_search)
                }
                PgvectorIndex::Ivfflat { lists, probes } => {
                    (2..=32768).contains(lists) && *probes > 0 && probes < lists
                }
            };
            if !valid {
                return invalid("invalid pgvector index construction or search parameters");
            }
        }
        Ok(())
    }
}

/// Failures retain diagnostic sources without returning provider or database details to callers.
#[derive(Debug, thiserror::Error)]
pub enum FileSearchError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("{0}")]
    UnsupportedFile(String),
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
    #[error("file search model provider request failed")]
    Provider(#[source] reqwest::Error),
    #[error("file search model provider returned an invalid response")]
    ProviderProtocol,
    #[error("file search model provider returned malformed JSON")]
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
            Self::InvalidRequest(_) | Self::UnsupportedFile(_) => 400,
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
            | Self::UnsupportedFile(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::Unavailable(message) => message.clone(),
            Self::Provider(_) | Self::ProviderProtocol | Self::ProviderDecode(_) => {
                "File search model service request failed".into()
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alpha: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact_factor: Option<f64>,
    pub score_threshold: Option<f64>,
    pub hybrid_search: Option<HybridSearchOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weights: Option<FusionWeights>,
}

/// Explicit vector/keyword fusion proportions. Neural scores replace fused scores.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FusionWeights {
    pub vector: f64,
    pub keyword: f64,
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

/// Files expiration policy measured from creation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FileExpiresAfter {
    pub anchor: FileExpirationAnchor,
    pub seconds: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum FileExpirationAnchor {
    CreatedAt,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    pub status: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum ChunkingStrategy {
    #[default]
    Auto,
    Contextual {
        contextual: ContextualChunking,
    },
    Static {
        #[serde(rename = "static")]
        config: StaticChunking,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ContextualChunking {
    pub model_id: Option<String>,
    pub max_chunk_size_tokens: usize,
    pub chunk_overlap_tokens: usize,
    pub timeout_seconds: Option<u64>,
    pub max_concurrency: Option<usize>,
    pub context_prompt: String,
}
impl Default for ContextualChunking {
    fn default() -> Self {
        Self {
        model_id: None, max_chunk_size_tokens: 700, chunk_overlap_tokens: 400,
        timeout_seconds: None, max_concurrency: None,
        context_prompt: "<document>\n{{WHOLE_DOCUMENT}}\n</document>\nHere is the chunk we want to situate within the whole document\n<chunk>\n{{CHUNK_CONTENT}}\n</chunk>\nPlease give a short succinct description to situate this chunk of text within the overall document for the purposes of improving search retrieval of the chunk. Answer only with the succinct description and nothing else.".into(),
    }
    }
}
impl ContextualChunking {
    /// # Errors
    /// Rejects unsupported bounds and malformed context templates.
    pub fn validate(&self) -> Result<(), FileSearchError> {
        if !(100..=4096).contains(&self.max_chunk_size_tokens)
            || self.chunk_overlap_tokens >= self.max_chunk_size_tokens
            || self.timeout_seconds.is_some_and(|n| !(1..=600).contains(&n))
            || self.max_concurrency.is_some_and(|n| !(1..=32).contains(&n))
            || self
                .model_id
                .as_ref()
                .is_some_and(|model| model.trim().is_empty() || model.len() > 385)
            || self.context_prompt.len() > 16384
        {
            return invalid("invalid contextual chunk size, overlap, model, timeout, or concurrency");
        }
        if self.context_prompt.matches("{{WHOLE_DOCUMENT}}").count() != 1
            || self.context_prompt.matches("{{CHUNK_CONTENT}}").count() != 1
        {
            return invalid("context_prompt requires each document and chunk placeholder exactly once");
        }
        match (
            self.context_prompt.find("{{WHOLE_DOCUMENT}}"),
            self.context_prompt.find("{{CHUNK_CONTENT}}"),
        ) {
            (Some(document), Some(chunk)) if document < chunk => Ok(()),
            _ => invalid("context_prompt requires {{WHOLE_DOCUMENT}} before {{CHUNK_CONTENT}}"),
        }
    }
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
    pub description: Option<String>,
    pub expires_after: Option<VectorStoreExpiresAfter>,
    #[serde(default)]
    pub file_ids: Vec<String>,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, String>>,
    pub chunking_strategy: Option<ChunkingStrategy>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AttachFileRequest {
    pub file_id: String,
    #[serde(default, deserialize_with = "null_default")]
    #[cfg_attr(feature = "openapi", schema(nullable = true))]
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
    pub status: VectorStoreStatus,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub last_active_at: Option<i64>,
    #[serde(default)]
    pub expires_after: Option<VectorStoreExpiresAfter>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub metadata: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct VectorStoreFileObject {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub vector_store_id: String,
    pub status: AttachmentStatus,
    pub usage_bytes: i64,
    pub attributes: FileAttributes,
    pub chunking_strategy: VectorStoreFileChunkingStrategy,
    pub last_error: Option<VectorStoreFileError>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct VectorStoreFileError {
    pub code: VectorStoreFileErrorCode,
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
    pub filter: Option<AttachmentStatus>,
    pub purpose: Option<String>,
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
        if let Some(ranker) = &self.ranker {
            ranker.parse::<Ranker>()?;
        }
        if self
            .model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty() || model.len() > 385)
        {
            return invalid("model must contain 1 to 385 bytes");
        }
        if self
            .alpha
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return invalid("alpha must be between 0 and 1");
        }
        if self
            .impact_factor
            .is_some_and(|value| !value.is_finite() || !(0.0..=10000.0).contains(&value))
        {
            return invalid("impact_factor must be between 0 and 10000");
        }
        if self
            .score_threshold
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return invalid("score_threshold must be between 0 and 1");
        }
        if let Some(weights) = &self.weights {
            if self.hybrid_search.is_some()
                || !weights.vector.is_finite()
                || !weights.keyword.is_finite()
                || weights.vector < 0.0
                || weights.keyword < 0.0
                || (weights.vector + weights.keyword - 1.0).abs() > 1e-6
            {
                return invalid(
                    "weights must be nonnegative vector/keyword proportions summing to 1; do not combine with hybrid_search",
                );
            }
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

/// A nullable patch distinguishes an omitted member from explicit JSON null.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NullablePatch<T>(pub Option<Option<T>>);

impl<T> Default for NullablePatch<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T> NullablePatch<T> {
    #[must_use]
    pub const fn is_missing(&self) -> bool {
        self.0.is_none()
    }
}

fn deserialize_patch<'de, D, T>(deserializer: D) -> Result<NullablePatch<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(|value| NullablePatch(Some(value)))
}

fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UpdateVectorStoreRequest {
    #[serde(
        default,
        deserialize_with = "deserialize_patch",
        skip_serializing_if = "NullablePatch::is_missing"
    )]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub name: NullablePatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_patch",
        skip_serializing_if = "NullablePatch::is_missing"
    )]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<BTreeMap<String, String>>))]
    pub metadata: NullablePatch<BTreeMap<String, String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_patch",
        skip_serializing_if = "NullablePatch::is_missing"
    )]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<VectorStoreExpiresAfter>))]
    pub expires_after: NullablePatch<VectorStoreExpiresAfter>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UpdateVectorStoreFileRequest {
    #[serde(deserialize_with = "null_default")]
    #[cfg_attr(feature = "openapi", schema(nullable = true))]
    pub attributes: FileAttributes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum VectorStoreExpirationAnchor {
    LastActiveAt,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct VectorStoreExpiresAfter {
    pub anchor: VectorStoreExpirationAnchor,
    pub days: u16,
}

impl VectorStoreExpiresAfter {
    /// # Errors
    /// Rejects policies outside the supported one to 365 day window.
    pub fn validate(&self) -> Result<(), FileSearchError> {
        if !(1..=365).contains(&self.days) {
            return invalid("expires_after.days must be between 1 and 365");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum VectorStoreStatus {
    InProgress,
    #[default]
    Completed,
    Expired,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum AttachmentStatus {
    InProgress,
    #[default]
    Completed,
    Cancelled,
    Failed,
}

impl AttachmentStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum VectorStoreFileErrorCode {
    ServerError,
    UnsupportedFile,
    InvalidFile,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct VectorStoreFileContentPage {
    pub object: String,
    pub data: Vec<ParsedFileContent>,
    pub has_more: bool,
    pub next_page: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum ParsedFileContent {
    Text { text: String },
}

/// Reported chunk boundaries, distinct from request-only auto/contextual configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum VectorStoreFileChunkingStrategy {
    Static {
        #[serde(rename = "static")]
        config: StaticChunking,
    },
    // Legacy rows retained request settings rather than resolved chunk boundaries.
    #[serde(alias = "auto", alias = "contextual")]
    Other,
}

/// A batch supplies either shared options with IDs or independent per-file options.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CreateFileBatchRequest {
    pub file_ids: Option<Vec<String>>,
    pub files: Option<Vec<AttachFileRequest>>,
    #[serde(default, deserialize_with = "null_default")]
    #[cfg_attr(feature = "openapi", schema(nullable = true))]
    pub attributes: FileAttributes,
    pub chunking_strategy: Option<ChunkingStrategy>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum BatchStatus {
    InProgress,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FileBatchObject {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub vector_store_id: String,
    pub status: BatchStatus,
    pub file_counts: FileCounts,
}
