//! Tool execution context and provider routing.
//!
//! `ToolContext` is the runtime container for tool providers. It holds
//! references to MCP servers, web search backends, and vector stores.
//! When `dispatch_tools` decides to execute tool calls, it delegates here.
//!
//! Design opinions:
//! - Each provider is `Option<Arc<dyn Trait>>` — missing providers produce errors, not panics
//! - Execution is parallel via `join_all` — all tool calls in one response run concurrently
//! - Individual failures are isolated — one hung tool doesn't block others (each has a timeout)
//! - The error output format is JSON (`{"error": "..."}`) — the model can parse and react to it
//! - Routing is MVP priority-order, NOT by tool type. Real routing requires the request's
//!   tools array (follow-up PR changes `dispatch_tools` signature)

use std::sync::Arc;
use std::time::Duration;

use crate::executor::ExecutorError;
use crate::tools::{McpToolExecutor, VectorStoreClient, WebSearchProvider};
use crate::types::io::{FunctionToolCall, FunctionToolResultMessage, InputItem};

/// Runtime configuration for tool execution.
///
/// Constructed once at server startup (or per-request if tools vary)
/// and passed into `execute_loop` / `dispatch_tools`.
///
/// # Defaults
///
/// - `max_iterations`: 10 (soft cap checked by `dispatch_tools`)
/// - `tool_timeout`: 30s per individual tool call
/// - All providers: None (calls produce "no provider configured" errors)
pub struct ToolContext {
    /// MCP tool executor — connects to external MCP servers.
    /// Used for user-defined tools declared as `type: "mcp"` in the request.
    pub mcp: Option<Arc<dyn McpToolExecutor>>,

    /// Web search provider (e.g., Brave, Google).
    /// Used for the built-in `web_search` tool type.
    pub web_search: Option<Arc<dyn WebSearchProvider>>,

    /// Vector store client (e.g., OGX).
    /// Used for the built-in `file_search` tool type.
    pub vector_store: Option<Arc<dyn VectorStoreClient>>,

    /// Maximum number of tool dispatch iterations before returning Incomplete.
    /// This is a SOFT cap — `dispatch_tools` checks `iteration >= max_iterations`.
    /// The HARD cap is ``MAX_LOOP_GUARD`` in `execute_loop`.rs (128).
    pub max_iterations: usize,

    /// Per-tool-call timeout. If a provider takes longer than this, the call
    /// produces a timeout error string (not a total dispatch failure).
    /// `Duration::ZERO` disables the timeout — use when providers manage their own.
    pub tool_timeout: Duration,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            mcp: None,
            web_search: None,
            vector_store: None,
            max_iterations: 10,
            tool_timeout: Duration::from_secs(30),
        }
    }
}

impl ToolContext {
    /// Execute all tool calls concurrently via `futures::future::join_all`.
    ///
    /// # Concurrency Model
    ///
    /// All calls start immediately and run in parallel on tokio's thread pool.
    /// `join_all` awaits ALL futures before returning — wall-clock time is
    /// bounded by the slowest individual call (not sum of all calls).
    ///
    /// With `tool_timeout = 30s` and N calls, worst case is 30s total (not N×30s).
    ///
    /// # Failure Model
    ///
    /// Individual failures produce an error JSON string as the tool output for
    /// that `call_id`. The dispatch does NOT fail as a whole. This matches the
    /// Responses API behavior where partial tool results are acceptable.
    ///
    /// The model sees `{"error": "..."}` as the tool output and decides:
    /// - Retry the tool on the next iteration
    /// - Answer without that result
    /// - Try a different approach
    ///
    /// # Retry Policy
    ///
    /// This layer does NOT retry. Providers handle their own retries internally
    /// (transient network errors, 503s, etc.). By the time an error reaches here,
    /// the provider already exhausted its retry budget. The agentic loop itself
    /// serves as a higher-level retry — the model can re-call a failed tool on
    /// the next iteration if it chooses to.
    pub async fn execute_all(&self, calls: &[&FunctionToolCall]) -> Vec<InputItem> {
        futures::future::join_all(calls.iter().map(|call| self.execute_one(call))).await
    }

    /// Execute a single tool call with timeout protection.
    ///
    /// Always returns an `InputItem::FunctionCallOutput` — either with the real
    /// result or with an error JSON string. Never panics, never returns Err.
    async fn execute_one(&self, call: &FunctionToolCall) -> InputItem {
        // Apply per-call timeout. Duration::ZERO = no timeout (opt-out).
        let result = if self.tool_timeout.is_zero() {
            self.route_call(call).await
        } else {
            match tokio::time::timeout(self.tool_timeout, self.route_call(call)).await {
                Ok(r) => r,
                Err(_elapsed) => Err(ExecutorError::StreamError(format!(
                    "tool '{}' timed out after {:?}",
                    call.name, self.tool_timeout
                ))),
            }
        };

        // Convert Result<String, Error> → String (error becomes JSON).
        // Using serde_json::json! ensures proper escaping of error messages
        // that might contain quotes, newlines, or other special characters.
        let output = match result {
            Ok(s) => s,
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        };

        InputItem::FunctionCallOutput(FunctionToolResultMessage {
            call_id: call.call_id.clone(),
            output,
        })
    }

    /// Route a tool call to the appropriate provider.
    ///
    /// MVP: Priority-order routing. Tries MCP first (most general), then
    /// `web_search`, then `vector_store`. First configured provider wins.
    ///
    /// This is intentionally simple — real routing needs the request's `tools`
    /// array to distinguish `type: "function"` (client-side) from `type: "mcp"`
    /// (gateway-side). That requires changing `dispatch_tools`'s signature to
    /// accept the tools array, which is a follow-up PR.
    ///
    /// When no provider is configured, returns a clear error that the model sees.
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
