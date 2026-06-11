use std::sync::Arc;

use crate::executor::ExecutorError;
use crate::tools::{McpToolExecutor, VectorStoreClient, WebSearchProvider};
use crate::types::io::{FunctionToolCall, FunctionToolResultMessage, InputItem};

/// Holds references to tool execution providers.
///
/// Passed into [`dispatch_tools`](super::dispatch::dispatch_tools) to resolve
/// and execute tool calls. Each provider is optional — calls to unconfigured
/// providers produce an error result (not a panic).
#[derive(Default)]
pub struct ToolContext {
    pub mcp: Option<Arc<dyn McpToolExecutor>>,
    pub web_search: Option<Arc<dyn WebSearchProvider>>,
    pub vector_store: Option<Arc<dyn VectorStoreClient>>,
    pub max_iterations: usize,
}

impl ToolContext {
    /// Execute all tool calls in parallel.
    ///
    /// Individual failures produce an error string as the tool output for that
    /// `call_id` — the dispatch does NOT fail as a whole. This matches the
    /// Responses API behavior where partial tool results are acceptable.
    pub async fn execute_all(&self, calls: &[&FunctionToolCall]) -> Vec<InputItem> {
        let futures: Vec<_> = calls.iter().map(|call| self.execute_one(call)).collect();
        futures::future::join_all(futures).await
    }

    async fn execute_one(&self, call: &FunctionToolCall) -> InputItem {
        let result = self.route_call(call).await;

        let output = match result {
            Ok(s) => s,
            Err(e) => format!("{{\"error\": \"{e}\"}}"),
        };

        InputItem::FunctionCallOutput(FunctionToolResultMessage {
            call_id: call.call_id.clone(),
            output,
        })
    }

    async fn route_call(&self, call: &FunctionToolCall) -> Result<String, ExecutorError> {
        // MVP: route all calls to MCP executor.
        // Future: inspect request tools array to determine provider.
        if let Some(mcp) = &self.mcp {
            return mcp.execute(&call.name, &call.arguments, &serde_json::Value::Null).await;
        }

        if let Some(web) = &self.web_search {
            return web.search(&call.arguments, "medium").await;
        }

        if let Some(vs) = &self.vector_store {
            let results = vs.search("", &call.arguments, 5).await?;
            return Ok(serde_json::to_string(&results).unwrap_or_default());
        }

        Err(ExecutorError::InvalidRequest(format!(
            "no tool provider configured for '{}'",
            call.name
        )))
    }
}
