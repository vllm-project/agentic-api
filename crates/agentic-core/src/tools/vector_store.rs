use std::future::Future;
use std::pin::Pin;

use crate::executor::ExecutorError;

/// Search a vector store (e.g. OGX) and return matching documents.
///
/// Results are returned as a JSON array of document objects. The caller
/// serializes them into `FunctionToolResultMessage.output`.
pub trait VectorStoreClient: Send + Sync {
    fn search(
        &self,
        store_id: &str,
        query: &str,
        max_results: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<serde_json::Value>, ExecutorError>> + Send + '_>>;
}
