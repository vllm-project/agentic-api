pub mod ogx;

use async_trait::async_trait;

use crate::types::SearchResult;

#[async_trait]
pub trait ResponseStore: Send + Sync {
    async fn get_response(&self, id: &str) -> Result<serde_json::Value, crate::error::Error>;
    async fn list_input_items(&self, response_id: &str) -> Result<Vec<serde_json::Value>, crate::error::Error>;
}

#[async_trait]
pub trait VectorSearch: Send + Sync {
    async fn search(&self, store_id: &str, query: &str) -> Result<Vec<SearchResult>, crate::error::Error>;
}
