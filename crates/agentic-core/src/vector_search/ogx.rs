use async_trait::async_trait;
use serde_json::{Map, Value};
use tracing::debug;

use super::types::{SearchOptions, SearchResponse, SearchResult};
use crate::error::Error;

pub struct OgxStore {
    base_url: String,
    client: reqwest::Client,
}

impl OgxStore {
    #[must_use]
    pub fn new(base_url: &str, client: reqwest::Client) -> Self {
        let base_url = base_url.trim_end_matches('/').to_owned();
        Self { base_url, client }
    }
}

#[async_trait]
impl super::VectorSearch for OgxStore {
    async fn search(&self, store_id: &str, query: &str, options: &SearchOptions) -> Result<Vec<SearchResult>, Error> {
        let url = format!("{}/v1/vector_stores/{store_id}/search", self.base_url);
        debug!(%url, "searching vector store via OGx");

        let mut body = Map::new();
        body.insert("query".to_owned(), Value::String(query.to_owned()));
        body.insert(
            "max_num_results".to_owned(),
            Value::from(options.max_num_results.unwrap_or(10)),
        );
        if let Some(filters) = &options.filters {
            body.insert("filters".to_owned(), filters.clone());
        }
        if let Some(ranking_options) = &options.ranking_options {
            body.insert("ranking_options".to_owned(), ranking_options.clone());
        }

        let resp = self.client.post(&url).json(&body).send().await.map_err(Error::Store)?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::StoreResponse {
                status: status.as_u16(),
                body,
            });
        }

        let search_resp: SearchResponse = resp.json().await.map_err(Error::Store)?;
        Ok(search_resp.data)
    }
}
