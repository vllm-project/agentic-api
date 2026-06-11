use std::future::Future;
use std::pin::Pin;

use crate::executor::ExecutorError;

/// Perform a web search and return results as a serialized string.
///
/// `context_size` controls result verbosity: `"low"`, `"medium"`, or `"high"`.
pub trait WebSearchProvider: Send + Sync {
    fn search(
        &self,
        query: &str,
        context_size: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, ExecutorError>> + Send + '_>>;
}
