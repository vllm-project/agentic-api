use std::sync::Arc;

use crate::executor::ExecutorError;
use crate::tools::{McpToolExecutor, VectorStoreClient, WebSearchProvider};
use crate::types::io::{FunctionToolCall, FunctionToolResultMessage, InputItem};

/// Holds references to tool execution providers.
///
/// Passed into [`dispatch_tools`](super::dispatch::dispatch_tools) to resolve
/// and execute tool calls. Each provider is optional — calls to unconfigured
/// providers produce an error result (not a panic).
pub struct ToolContext {
    pub mcp: Option<Arc<dyn McpToolExecutor>>,
    pub web_search: Option<Arc<dyn WebSearchProvider>>,
    pub vector_store: Option<Arc<dyn VectorStoreClient>>,
    pub max_iterations: usize,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            mcp: None,
            web_search: None,
            vector_store: None,
            max_iterations: 10,
        }
    }
}

impl ToolContext {
    /// Execute all tool calls concurrently via `join_all`.
    ///
    /// Concurrency note: futures run on the tokio runtime's thread pool.
    /// True parallelism depends on the runtime being multi-threaded.
    ///
    /// Failure model: individual failures produce an error JSON string as the
    /// tool output for that `call_id` — the dispatch does NOT fail as a whole.
    /// The model sees the error and decides whether to retry (next iteration),
    /// try a different approach, or answer without that result.
    ///
    /// Retry policy: this layer does NOT retry. Providers handle their own
    /// retries internally (transient network errors, 503s, etc.). By the time
    /// an error reaches here, the provider already exhausted its retry budget.
    /// The agentic loop itself serves as a higher-level retry — the model can
    /// re-call a failed tool on the next iteration if it chooses to.
    pub async fn execute_all(&self, calls: &[&FunctionToolCall]) -> Vec<InputItem> {
        let futures: Vec<_> = calls.iter().map(|call| self.execute_one(call)).collect();
        futures::future::join_all(futures).await
    }

    async fn execute_one(&self, call: &FunctionToolCall) -> InputItem {
        let result = self.route_call(call).await;

        let output = match result {
            Ok(s) => s,
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        };

        InputItem::FunctionCallOutput(FunctionToolResultMessage {
            call_id: call.call_id.clone(),
            output,
        })
    }

    /// MVP routing: tries providers in order (MCP → `web_search` → `vector_store`).
    /// Future: route based on tool type from the request's tools array.
    async fn route_call(&self, call: &FunctionToolCall) -> Result<String, ExecutorError> {
        if let Some(mcp) = &self.mcp {
            return mcp.execute(&call.name, &call.arguments, &serde_json::Value::Null).await;
        }

        if let Some(web) = &self.web_search {
            return web.search(&call.arguments, "medium").await;
        }

        if let Some(vs) = &self.vector_store {
            let results = vs.search("", &call.arguments, 5).await?;
            return Ok(serde_json::to_string(&results).expect("Vec<Value> is always serializable"));
        }

        Err(ExecutorError::InvalidRequest(format!(
            "no tool provider configured for '{}'",
            call.name
        )))
    }
}
