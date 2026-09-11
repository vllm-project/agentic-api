//! Deployment-selected models and bounded retrieval settings. No request controls endpoints or secrets.
use super::file_search::{FileSearchError, SearchMode, invalid};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VectorStoresConfig {
    pub providers: BTreeMap<String, ModelProvider>,
    pub default_provider_id: Option<String>,
    pub default_embedding_model: Option<QualifiedModel>,
    pub default_reranker_model: Option<QualifiedModel>,
    pub file_ingestion_params: FileIngestionParams,
    pub chunk_retrieval_params: ChunkRetrievalParams,
    pub contextual_retrieval_params: ContextualRetrievalParams,
    pub rewrite_query_params: Option<RewriteQueryParams>,
    pub file_batch_params: FileBatchParams,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualifiedModel {
    pub provider_id: String,
    pub model_id: String,
    pub embedding_dimensions: Option<usize>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProvider {
    pub base_url: String,
    pub models: Vec<String>,
    pub api_key_env: Option<String>,
    #[serde(skip)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub protocol: ModelProtocol,
    #[serde(default)]
    pub score_interpretation: ScoreInterpretation,
}
impl fmt::Debug for ModelProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelProvider")
            .field("models", &self.models)
            .field("protocol", &self.protocol)
            .field("score_interpretation", &self.score_interpretation)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProtocol {
    #[default]
    Vllm,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreInterpretation {
    #[default]
    Probability,
    Logit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum Ranker {
    Auto,
    None,
    #[default]
    Rrf,
    Weighted,
    Normalized,
    Neural,
    Classifier,
    #[serde(rename = "default-2024-11-15")]
    Default20241115,
    #[serde(rename = "default-2024-08-21", alias = "default_2024_08_21")]
    Default20240821,
}
impl Ranker {
    #[must_use]
    pub const fn uses_model(self) -> bool {
        matches!(
            self,
            Self::Neural | Self::Classifier | Self::Default20241115 | Self::Default20240821
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChunkRetrievalParams {
    pub chunk_multiplier: usize,
    pub max_tokens_in_context: usize,
    pub default_reranker_strategy: Ranker,
    pub rrf_impact_factor: f64,
    pub weighted_search_alpha: f64,
    pub default_search_mode: Option<SearchMode>,
}
impl Default for ChunkRetrievalParams {
    fn default() -> Self {
        Self {
            chunk_multiplier: 5,
            max_tokens_in_context: 4000,
            default_reranker_strategy: Ranker::Rrf,
            rrf_impact_factor: 60.0,
            weighted_search_alpha: 0.5,
            default_search_mode: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileIngestionParams {
    pub default_chunk_size_tokens: usize,
    pub default_chunk_overlap_tokens: usize,
}
impl Default for FileIngestionParams {
    fn default() -> Self {
        Self {
            default_chunk_size_tokens: 800,
            default_chunk_overlap_tokens: 400,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextualRetrievalParams {
    pub model: Option<QualifiedModel>,
    pub default_timeout_seconds: u64,
    pub default_max_concurrency: usize,
    pub max_document_tokens: usize,
}
impl Default for ContextualRetrievalParams {
    fn default() -> Self {
        Self {
            model: None,
            default_timeout_seconds: 120,
            default_max_concurrency: 3,
            max_document_tokens: 100_000,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RewriteQueryParams {
    pub model: Option<QualifiedModel>,
    pub max_tokens: usize,
    pub temperature: f64,
    pub prompt: String,
}
impl Default for RewriteQueryParams {
    fn default() -> Self {
        Self{model:None,max_tokens:100,temperature:0.3,prompt:"Expand this query with relevant synonyms and related terms. Return only the improved query, no explanations:\n\n{query}\n\nImproved query:".into()}
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileBatchParams {
    pub max_concurrent_files_per_batch: usize,
    pub file_batch_chunk_size: usize,
    pub cleanup_interval_seconds: u64,
}
impl Default for FileBatchParams {
    fn default() -> Self {
        Self {
            max_concurrent_files_per_batch: 3,
            file_batch_chunk_size: 10,
            cleanup_interval_seconds: 86400,
        }
    }
}

impl VectorStoresConfig {
    /// Checks all deployment bounds and registered model references before serving traffic.
    /// # Errors
    /// Returns an invalid-request configuration error.
    pub fn validate(&self) -> Result<(), FileSearchError> {
        if self.providers.len() > 32 {
            return invalid("at most 32 model providers may be configured");
        }
        for (id, provider) in &self.providers {
            if id.is_empty()
                || id.len() > 128
                || id.contains('/')
                || provider.models.is_empty()
                || provider.models.len() > 128
                || provider
                    .models
                    .iter()
                    .any(|model| model.trim().is_empty() || model.len() > 256)
            {
                return invalid("provider IDs and allowlisted models must be nonempty and bounded");
            }
            provider.endpoint("chat/completions")?;
            if provider.api_key_env.as_ref().is_some_and(|key| {
                key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            }) {
                return invalid("provider api_key_env must be an environment variable name");
            }
        }
        if self
            .default_provider_id
            .as_ref()
            .is_some_and(|id| !self.providers.contains_key(id))
        {
            return invalid("default_provider_id is not configured");
        }
        for model in [
            self.default_embedding_model.as_ref(),
            self.default_reranker_model.as_ref(),
            self.contextual_retrieval_params.model.as_ref(),
            self.rewrite_query_params
                .as_ref()
                .and_then(|rewrite| rewrite.model.as_ref()),
        ]
        .into_iter()
        .flatten()
        {
            self.resolve(None, Some(model))?;
            if model.embedding_dimensions.is_some_and(|n| !(1..=4096).contains(&n)) {
                return invalid("embedding_dimensions must be 1 to 4096");
            }
        }
        let retrieval = &self.chunk_retrieval_params;
        if !(1..=20).contains(&retrieval.chunk_multiplier)
            || !(1..=32768).contains(&retrieval.max_tokens_in_context)
            || !retrieval.rrf_impact_factor.is_finite()
            || !(0.0..=10000.0).contains(&retrieval.rrf_impact_factor)
            || !retrieval.weighted_search_alpha.is_finite()
            || !(0.0..=1.0).contains(&retrieval.weighted_search_alpha)
        {
            return invalid("invalid chunk retrieval bounds, alpha, or RRF impact factor");
        }
        let ingestion = &self.file_ingestion_params;
        if !(100..=4096).contains(&ingestion.default_chunk_size_tokens)
            || ingestion.default_chunk_overlap_tokens > ingestion.default_chunk_size_tokens / 2
        {
            return invalid("ingestion chunk size must be 100 to 4096 and overlap at most half");
        }
        let context = &self.contextual_retrieval_params;
        if !(10..=600).contains(&context.default_timeout_seconds)
            || !(1..=32).contains(&context.default_max_concurrency)
            || !(1000..=1_000_000).contains(&context.max_document_tokens)
        {
            return invalid("invalid contextual timeout, concurrency, or document bound");
        }
        if let Some(rewrite) = &self.rewrite_query_params {
            if !(1..=4096).contains(&rewrite.max_tokens)
                || !rewrite.temperature.is_finite()
                || !(0.0..=2.0).contains(&rewrite.temperature)
                || !rewrite.prompt.contains("{query}")
                || rewrite.prompt.len() > 16384
            {
                return invalid("invalid query rewrite token limit, temperature, or prompt (requires {query})");
            }
        }
        let batch = &self.file_batch_params;
        if !(1..=32).contains(&batch.max_concurrent_files_per_batch)
            || !(1..=1000).contains(&batch.file_batch_chunk_size)
            || !(1..=604_800).contains(&batch.cleanup_interval_seconds)
        {
            return invalid("invalid file batch concurrency, chunk size, or cleanup interval");
        }
        Ok(())
    }
    /// Resolves a registered provider and its allowlisted provider-local model.
    /// # Errors
    /// Fails closed on unknown providers/models or absent defaults.
    pub fn resolve<'a>(
        &'a self,
        selection: Option<&'a str>,
        default: Option<&'a QualifiedModel>,
    ) -> Result<(&'a ModelProvider, &'a str), FileSearchError> {
        let (provider_id, model_id) = if let Some(selection) = selection {
            match selection.split_once('/') {
                Some(parts) => parts,
                None => (
                    self.default_provider_id.as_deref().ok_or_else(|| {
                        FileSearchError::InvalidRequest("unqualified model requires default_provider_id".into())
                    })?,
                    selection,
                ),
            }
        } else if let Some(default) = default {
            (default.provider_id.as_str(), default.model_id.as_str())
        } else {
            return invalid("requested model operation requires a configured default model or model selector");
        };
        let provider = self
            .providers
            .get(provider_id)
            .ok_or_else(|| FileSearchError::InvalidRequest("model provider is not configured".into()))?;
        if !provider.models.iter().any(|model| model == model_id) {
            return invalid("model is not in the provider allowlist");
        }
        Ok((provider, model_id))
    }
}
impl ModelProvider {
    /// Constructs a validated provider endpoint without routing request data as URLs.
    /// # Errors
    /// Returns invalid-request for malformed or credential-bearing base URLs.
    pub fn endpoint(&self, operation: &str) -> Result<reqwest::Url, FileSearchError> {
        let mut url = reqwest::Url::parse(&self.base_url)
            .map_err(|_| FileSearchError::InvalidRequest("invalid model provider base_url".into()))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return invalid("model provider base_url must be HTTP(S) without credentials, query, or fragment");
        }
        let base = url.path().trim_end_matches('/');
        let base = if operation == "rerank" {
            base.strip_suffix("/v1").unwrap_or(base)
        } else {
            base
        };
        url.set_path(&format!("{base}/{operation}"));
        Ok(url)
    }
}

impl std::str::FromStr for Ranker {
    type Err = FileSearchError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "none" => Ok(Self::None),
            "rrf" => Ok(Self::Rrf),
            "weighted" => Ok(Self::Weighted),
            "normalized" => Ok(Self::Normalized),
            "neural" => Ok(Self::Neural),
            "classifier" => Ok(Self::Classifier),
            "default-2024-11-15" => Ok(Self::Default20241115),
            "default-2024-08-21" | "default_2024_08_21" => Ok(Self::Default20240821),
            _ => invalid("unsupported file search ranker"),
        }
    }
}
