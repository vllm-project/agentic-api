use std::future::Future;
use std::pin::Pin;

use crate::executor::ExecutorError;

/// Execute a tool call via the Model Context Protocol.
///
/// Implementations connect to an MCP server and invoke the named tool
/// with the provided arguments. The result is returned as a serialized
/// JSON string suitable for injection into `FunctionToolResultMessage.output`.
pub trait McpToolExecutor: Send + Sync {
    fn execute(
        &self,
        tool_name: &str,
        arguments: &str,
        server_config: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ExecutorError>> + Send + '_>>;
}
