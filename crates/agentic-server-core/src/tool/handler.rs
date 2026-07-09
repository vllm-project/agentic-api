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

/// Trait implemented by every tool type — client-owned and gateway-owned alike.
///
/// Covers validation and normalization: the steps that apply to all tools
/// regardless of who executes them.
///
/// Implementations must be `Send + Sync` so they can be stored behind `Arc<dyn
/// ToolHandler>` and used across async task boundaries.
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
}

/// Extension of [`ToolHandler`] for tool types that are executed by the gateway.
///
/// Only gateway-owned tools (`Mcp`, `WebSearch`, `FileSearch`, `CodeInterpreter`)
/// implement this trait. Client-owned tools (`Function`) do not — the type system
/// makes it impossible to call `execute()` on them.
///
/// ## Note on `async fn` in traits
///
/// Native `async fn` in traits (Rust 1.75+) is not yet `dyn`-compatible. Since
/// PR B will store handlers as `Arc<dyn GatewayExecutor>`, we use explicit
/// `Pin<Box<dyn Future>>` return types.
pub trait GatewayExecutor: ToolHandler + 'static {
    /// Execute a tool call and return the result.
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
        call_id: &str,
        tool_name: &str,
        arguments: &str,
        config: &Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    // Compile-time check: Arc<dyn GatewayExecutor> must be constructable.
    // This fails to compile if GatewayExecutor ever becomes dyn-incompatible.
    fn _assert_gateway_executor_dyn_compatible(_: Arc<dyn GatewayExecutor>) {}
}
