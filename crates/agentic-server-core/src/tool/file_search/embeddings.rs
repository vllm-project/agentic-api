//! Bounded OpenAI-compatible embedding calls with strict response validation.

use std::{sync::Arc, time::Duration};

#[cfg(test)]
use crate::types::retrieval_models::EmbeddingData;
use crate::types::retrieval_models::{EmbeddingRequest, EmbeddingResponse};
use futures::StreamExt;

use crate::types::file_search::{FileSearchConfig, FileSearchError};

const MAX_DIMENSIONS: usize = 4096;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const BATCH_SIZE: usize = 32;

#[derive(Clone)]
pub(super) struct Embeddings {
    client: Arc<reqwest::Client>,
    endpoint: reqwest::Url,
    model: String,
    api_key: Option<String>,
    dimensions: Option<usize>,
}

impl Embeddings {
    pub(super) fn from_config(
        client: Arc<reqwest::Client>,
        config: &FileSearchConfig,
    ) -> Result<Option<Self>, FileSearchError> {
        if let Some(model) = &config.vector_stores.default_embedding_model {
            let (provider, name) = config.vector_stores.resolve(None, Some(model))?;
            if config.embedding_base_url.is_some()
                || config.embedding_model.is_some()
                || config.embedding_api_key.is_some()
            {
                return Err(FileSearchError::InvalidRequest(
                    "configure either grouped or legacy embeddings, not both".into(),
                ));
            }
            return Ok(Some(Self {
                client,
                endpoint: provider.endpoint("embeddings")?,
                model: name.into(),
                api_key: provider.api_key.clone(),
                dimensions: model.embedding_dimensions,
            }));
        }
        let (Some(base_url), Some(model)) = (&config.embedding_base_url, &config.embedding_model) else {
            if config.embedding_base_url.is_some()
                || config.embedding_model.is_some()
                || config.embedding_api_key.is_some()
            {
                return Err(FileSearchError::InvalidRequest(
                    "Both embedding_base_url and embedding_model are required when configuring embeddings".into(),
                ));
            }
            return Ok(None);
        };
        let mut endpoint = reqwest::Url::parse(base_url)
            .map_err(|_| FileSearchError::InvalidRequest("Invalid embedding_base_url".into()))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(FileSearchError::InvalidRequest(
                "embedding_base_url must be an HTTP(S) URL without credentials, query, or fragment".into(),
            ));
        }
        if model.trim().is_empty() || model.len() > 256 {
            return Err(FileSearchError::InvalidRequest(
                "embedding_model must contain 1 to 256 bytes".into(),
            ));
        }
        endpoint.set_path(&format!("{}/embeddings", endpoint.path().trim_end_matches('/')));
        Ok(Some(Self {
            client,
            endpoint,
            model: model.clone(),
            api_key: config.embedding_api_key.clone(),
            dimensions: None,
        }))
    }

    pub(super) fn identity(&self) -> String {
        format!("{}\n{}", self.endpoint, self.model)
    }

    pub(super) async fn embed(
        &self,
        texts: &[String],
        expected_dimensions: Option<usize>,
    ) -> Result<Vec<Vec<f64>>, FileSearchError> {
        let mut embeddings = Vec::with_capacity(texts.len());
        let mut dimensions = expected_dimensions.or(self.dimensions);
        if self.dimensions.is_some_and(|configured| dimensions != Some(configured)) {
            return Err(FileSearchError::InvalidRequest(
                "stored and configured embedding dimensions differ".into(),
            ));
        }
        for input in texts.chunks(BATCH_SIZE) {
            let mut request = self
                .client
                .post(self.endpoint.clone())
                .timeout(Duration::from_secs(45))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(serde_json::to_vec(&EmbeddingRequest {
                    model: &self.model,
                    input,
                    encoding_format: "float",
                    dimensions: self.dimensions,
                })?);
            if let Some(key) = &self.api_key {
                request = request.bearer_auth(key);
            }
            let response = request
                .send()
                .await
                .map_err(|error| FileSearchError::Provider(error.without_url()))?
                .error_for_status()
                .map_err(|error| FileSearchError::Provider(error.without_url()))?;
            if response
                .content_length()
                .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
            {
                return Err(FileSearchError::ProviderProtocol);
            }
            let mut bytes = Vec::new();
            let mut body = response.bytes_stream();
            while let Some(part) = body.next().await {
                let part = part.map_err(|error| FileSearchError::Provider(error.without_url()))?;
                if bytes.len().saturating_add(part.len()) > MAX_RESPONSE_BYTES {
                    return Err(FileSearchError::ProviderProtocol);
                }
                bytes.extend_from_slice(&part);
            }
            let output: EmbeddingResponse = serde_json::from_slice(&bytes).map_err(FileSearchError::ProviderDecode)?;
            let batch = validate_response(output, &self.model, input.len(), dimensions)?;
            dimensions = batch.first().map(Vec::len);
            if dimensions.is_some_and(|dimension| texts.len().saturating_mul(dimension) > 4 * 1024 * 1024) {
                return Err(FileSearchError::InvalidRequest(
                    "Embedding output exceeds 32 MiB per ingestion; use a smaller file or larger chunks".into(),
                ));
            }
            embeddings.extend(batch);
        }
        Ok(embeddings)
    }
}

fn validate_response(
    mut response: EmbeddingResponse,
    model: &str,
    count: usize,
    dimensions: Option<usize>,
) -> Result<Vec<Vec<f64>>, FileSearchError> {
    if response.model != model || response.data.len() != count {
        return Err(FileSearchError::ProviderProtocol);
    }
    response.data.sort_unstable_by_key(|item| item.index);
    let dimension = dimensions
        .or_else(|| response.data.first().map(|item| item.embedding.len()))
        .unwrap_or(0);
    if !(1..=MAX_DIMENSIONS).contains(&dimension) {
        return Err(FileSearchError::ProviderProtocol);
    }
    for (index, item) in response.data.iter().enumerate() {
        if item.index != index
            || item.embedding.len() != dimension
            || item.embedding.iter().any(|value| !value.is_finite())
            || item.embedding.iter().all(|value| *value == 0.0)
        {
            return Err(FileSearchError::ProviderProtocol);
        }
    }
    Ok(response.data.into_iter().map(|item| item.embedding).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(indexes: &[usize], vectors: &[Vec<f64>]) -> EmbeddingResponse {
        EmbeddingResponse {
            model: "fixture".into(),
            data: indexes
                .iter()
                .zip(vectors)
                .map(|(index, embedding)| EmbeddingData {
                    index: *index,
                    embedding: embedding.clone(),
                })
                .collect(),
        }
    }

    #[test]
    fn indexes_count_dimension_model_and_finite_values_are_validated() {
        let vectors = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
        assert_eq!(
            validate_response(output(&[1, 0], &vectors), "fixture", 2, Some(2)).unwrap(),
            vec![vectors[1].clone(), vectors[0].clone()]
        );
        for bad in [
            output(&[0, 0], &vectors),
            output(&[0, 2], &vectors),
            output(&[0], &vectors),
            output(&[0, 1], &[vec![1.0], vec![1.0, 2.0]]),
            output(&[0, 1], &[vec![f64::NAN, 1.0], vec![1.0, 2.0]]),
        ] {
            assert!(validate_response(bad, "fixture", 2, Some(2)).is_err());
        }
        assert!(validate_response(output(&[0, 1], &vectors), "different", 2, Some(2)).is_err());
        assert!(validate_response(output(&[0, 1], &vectors), "fixture", 2, Some(3)).is_err());
    }
}
