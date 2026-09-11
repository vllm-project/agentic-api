//! Cancellation-safe bounded text generation and vLLM reranking.
use crate::types::{
    file_search::{
        ContextualChunking, FileSearchError, ModelProvider, ScoreInterpretation, SearchResult, VectorStoresConfig,
        invalid,
    },
    retrieval_models::{
        RetrievalChatRequest, RetrievalChatResponse, RetrievalMessage, RetrievalRole, RetrievalText, RetrievalTextPart,
        TextRerankRequest, TextRerankResponse,
    },
};
use futures::{StreamExt, TryStreamExt};
use serde::{Serialize, de::DeserializeOwned};
use std::{sync::Arc, time::Duration};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub(super) struct Models {
    client: Arc<reqwest::Client>,
    config: Arc<VectorStoresConfig>,
    contextual_workers: Arc<tokio::sync::Semaphore>,
}
impl Models {
    pub(super) fn new(client: Arc<reqwest::Client>, config: Arc<VectorStoresConfig>) -> Self {
        let concurrency = config.contextual_retrieval_params.default_max_concurrency;
        Self {
            client,
            config,
            contextual_workers: Arc::new(tokio::sync::Semaphore::new(concurrency)),
        }
    }
    pub(super) async fn rewrite(&self, queries: &[String]) -> Result<String, FileSearchError> {
        let params = self.config.rewrite_query_params.as_ref().ok_or_else(|| {
            FileSearchError::InvalidRequest("rewrite_query requires configured rewrite_query_params".into())
        })?;
        let (provider, model) = self.config.resolve(None, params.model.as_ref())?;
        let prompt = params.prompt.replace("{query}", &queries.join(" "));
        self.chat(
            provider,
            &RetrievalChatRequest {
                model,
                messages: vec![RetrievalMessage {
                    role: RetrievalRole::User,
                    content: &prompt,
                }],
                stream: false,
                temperature: params.temperature,
                max_tokens: params.max_tokens,
            },
            45,
            4096,
        )
        .await
    }
    pub(super) async fn contextualize(
        &self,
        document: &str,
        chunks: &[String],
        params: &ContextualChunking,
    ) -> Result<Vec<String>, FileSearchError> {
        let defaults = &self.config.contextual_retrieval_params;
        let (provider, model) = self
            .config
            .resolve(params.model_id.as_deref(), defaults.model.as_ref())?;
        if document.chars().count() / 4 > defaults.max_document_tokens {
            return invalid("contextual document exceeds max_document_tokens");
        }
        let (prefix, suffix) = params
            .context_prompt
            .split_once("{{CHUNK_CONTENT}}")
            .ok_or_else(|| FileSearchError::InvalidRequest("contextual chunk placeholder is missing".into()))?;
        let prefix = prefix.replace("{{WHOLE_DOCUMENT}}", document);
        let concurrency = params
            .max_concurrency
            .unwrap_or(defaults.default_max_concurrency)
            .min(defaults.default_max_concurrency);
        let prefix = prefix.as_str();
        // Buffered futures are dropped on cancellation or any failed chunk; no detached workers or partial publication.
        futures::stream::iter((0..chunks.len()).map(|index| async move {
            let chunk = &chunks[index];
            let _permit = self
                .contextual_workers
                .acquire()
                .await
                .map_err(|_| FileSearchError::Unavailable("contextual retrieval is shutting down".into()))?;
            let user_message = format!("{chunk}{suffix}");
            let context = self
                .chat(
                    provider,
                    &RetrievalChatRequest {
                        model,
                        messages: vec![
                            RetrievalMessage {
                                role: RetrievalRole::System,
                                content: prefix.trim_end(),
                            },
                            RetrievalMessage {
                                role: RetrievalRole::User,
                                content: &user_message,
                            },
                        ],
                        stream: false,
                        temperature: 0.0,
                        max_tokens: 256,
                    },
                    params.timeout_seconds.unwrap_or(defaults.default_timeout_seconds),
                    8192,
                )
                .await?;
            Ok(format!("{context}\n\n{chunk}"))
        }))
        .buffered(concurrency)
        .try_collect()
        .await
    }
    async fn chat(
        &self,
        provider: &ModelProvider,
        request: &RetrievalChatRequest<'_>,
        timeout: u64,
        max_text: usize,
    ) -> Result<String, FileSearchError> {
        let response: RetrievalChatResponse = self.post(provider, "chat/completions", request, timeout).await?;
        if response.choices.len() != 1 {
            return Err(FileSearchError::ProviderProtocol);
        }
        let text = match response
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
        {
            Some(RetrievalText::Text(text)) => text,
            Some(RetrievalText::Parts(parts)) => parts
                .into_iter()
                .map(|RetrievalTextPart::Text { text }| text)
                .collect::<Vec<_>>()
                .join(" "),
            None => return Err(FileSearchError::ProviderProtocol),
        };
        let text = text.trim();
        if text.is_empty() || text.len() > max_text {
            return Err(FileSearchError::ProviderProtocol);
        }
        Ok(text.into())
    }
    pub(super) async fn rerank(
        &self,
        query: &str,
        mut candidates: Vec<SearchResult>,
        selection: Option<&str>,
    ) -> Result<Vec<SearchResult>, FileSearchError> {
        let (provider, model) = self
            .config
            .resolve(selection, self.config.default_reranker_model.as_ref())?;
        if candidates.is_empty() {
            return Ok(candidates);
        }
        let documents = candidates
            .iter()
            .map(|result| result.content[0].text.as_str())
            .collect();
        // Score every bounded candidate, then validate the complete permutation before publishing any result.
        let request = TextRerankRequest {
            model,
            query,
            documents,
            top_n: candidates.len(),
        };
        let response: TextRerankResponse = self.post(provider, "rerank", &request, 45).await?;
        if response.results.len() != candidates.len() {
            return Err(FileSearchError::ProviderProtocol);
        }
        let mut seen = vec![false; candidates.len()];
        for result in response.results {
            if result.index >= candidates.len() || seen[result.index] || !result.relevance_score.is_finite() {
                return Err(FileSearchError::ProviderProtocol);
            }
            seen[result.index] = true;
            let score = match provider.score_interpretation {
                ScoreInterpretation::Probability if (0.0..=1.0).contains(&result.relevance_score) => {
                    result.relevance_score
                }
                ScoreInterpretation::Probability => return Err(FileSearchError::ProviderProtocol),
                ScoreInterpretation::Logit if result.relevance_score >= 0.0 => {
                    1.0 / (1.0 + (-result.relevance_score).exp())
                }
                ScoreInterpretation::Logit => {
                    let exp = result.relevance_score.exp();
                    exp / (1.0 + exp)
                }
            };
            candidates[result.index].score = score;
        }
        candidates.sort_by(|left, right| right.score.total_cmp(&left.score));
        Ok(candidates)
    }
    async fn post<T: Serialize, R: DeserializeOwned>(
        &self,
        provider: &ModelProvider,
        operation: &str,
        request: &T,
        timeout: u64,
    ) -> Result<R, FileSearchError> {
        let bytes = serde_json::to_vec(request)?;
        if bytes.len() > 32 * 1024 * 1024 {
            return invalid("model request exceeds 32 MiB");
        }
        let mut request = self
            .client
            .post(provider.endpoint(operation)?)
            .timeout(Duration::from_secs(timeout))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes);
        if let Some(key) = &provider.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|error| FileSearchError::Provider(error.without_url()))?
            .error_for_status()
            .map_err(|error| FileSearchError::Provider(error.without_url()))?;
        if response.content_length().is_some_and(|n| n > MAX_RESPONSE_BYTES as u64) {
            return Err(FileSearchError::ProviderProtocol);
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(part) = stream.next().await {
            let part = part.map_err(|error| FileSearchError::Provider(error.without_url()))?;
            if bytes.len().saturating_add(part.len()) > MAX_RESPONSE_BYTES {
                return Err(FileSearchError::ProviderProtocol);
            }
            bytes.extend_from_slice(&part);
        }
        serde_json::from_slice(&bytes).map_err(FileSearchError::ProviderDecode)
    }
}
