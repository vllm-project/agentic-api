use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::types::io::FunctionTool;

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub call_id: String,
    pub output: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("execution failed: {0}")]
    Execution(String),
    #[error("invalid tool config: {0}")]
    Config(String),
}

/// Trait implemented by each tool type (function, MCP, `web_search`, …).
///
/// Every tool type normalises itself to vLLM-compatible `FunctionTool` definitions
/// and, when gateway-owned, executes via `execute()`. Function tools skip
/// execution and return `requires_action` to the client.
///
/// Implementations must be `Send + Sync` so they can be stored behind `Arc<dyn
/// ToolHandler>` and used across async task boundaries.
///
/// ## Note on `async fn` in traits
///
/// Native `async fn` in traits (Rust 1.75+) is not yet `dyn`-compatible. Since
/// PR B will store handlers as `Arc<dyn ToolHandler>`, we use explicit
/// `Pin<Box<dyn Future>>` return types. If `dyn` dispatch is not needed in your
/// context, consider `#[trait_variant::make]` or `#[async_trait]`.
pub trait ToolHandler: Send + Sync {
    #[must_use]
    fn tool_type(&self) -> super::registry::ToolType;

    /// Validate the tool param JSON.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] for obviously invalid configurations.
    fn validate(&self, param: &Value) -> Result<(), ToolError>;

    /// Normalise this tool declaration into vLLM-compatible `FunctionTool` entries.
    #[must_use]
    fn normalize(&self, param: &Value) -> Vec<FunctionTool>;

    /// Execute a tool call and return the result.
    ///
    /// This method is **never called** for `ToolType::Function` — function tools are
    /// client-owned and the gateway returns `requires_action` to the caller instead.
    ///
    /// ## `config` parameter
    ///
    /// `config` is the serialised **server-level** tool param (i.e. the `*ToolParam`
    /// struct stored in [`super::registry::ToolEntry::config`]). It is **not** the
    /// per-tool parameter schema.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] if the tool call fails.
    fn execute(
        &self,
        tool_name: &str,
        arguments: &str,
        config: &Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>>;
}
