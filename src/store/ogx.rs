use async_trait::async_trait;
use tracing::debug;

use crate::error::Error;
use crate::types::{SearchResponse, SearchResult};

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
impl super::ResponseStore for OgxStore {
    async fn get_response(&self, id: &str) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/responses/{id}", self.base_url);
        debug!(%url, "fetching response from OGx");

        let resp = self.client.get(&url).send().await.map_err(Error::Store)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::StoreResponse {
                status: status.as_u16(),
                body,
            });
        }
        resp.json().await.map_err(Error::Store)
    }

    async fn list_input_items(&self, response_id: &str) -> Result<Vec<serde_json::Value>, Error> {
        let url = format!("{}/v1/responses/{response_id}/input_items", self.base_url);
        debug!(%url, "listing input items from OGx");

        let resp = self
            .client
            .get(&url)
            .query(&[("order", "asc"), ("limit", "100")])
            .send()
            .await
            .map_err(Error::Store)?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::StoreResponse {
                status: status.as_u16(),
                body,
            });
        }

        let body: serde_json::Value = resp.json().await.map_err(Error::Store)?;
        let items = body
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(items)
    }
}

#[async_trait]
impl super::VectorSearch for OgxStore {
    async fn search(&self, store_id: &str, query: &str) -> Result<Vec<SearchResult>, Error> {
        let url = format!("{}/v1/vector_stores/{store_id}/search", self.base_url);
        debug!(%url, %query, "searching vector store via OGx");

        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "query": query,
                "max_num_results": 10
            }))
            .send()
            .await
            .map_err(Error::Store)?;

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
