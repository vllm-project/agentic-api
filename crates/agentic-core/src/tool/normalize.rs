use crate::types::io::FunctionTool;
use crate::types::io::input::FunctionToolResultMessage;
use crate::types::tools::ResponsesTool;

use super::handler::ToolOutput;

impl ResponsesTool {
    /// Normalise this tool declaration to the `FunctionTool` wire format that vLLM understands.
    ///
    /// - `Function` variants convert via [`From<&FunctionToolParam>`] for `FunctionTool`.
    ///   Returns `None` and logs at `debug` level if the name is empty.
    /// - All other variants (`Mcp`, `WebSearch`, `FileSearch`, `CodeInterpreter`) return
    ///   `None` and emit a `tracing::debug!` — their full handlers have not landed yet.
    ///
    /// This is the entry point called by `RequestPayload::to_upstream_request()` so that
    /// vLLM always receives a `Vec<FunctionTool>`, never a raw `ResponsesTool` enum.
    #[must_use]
    pub fn to_function_tool(&self) -> Option<FunctionTool> {
        match self {
            // name is NonEmptyToolName — empty names are rejected by serde at
            // deserialization time, so no runtime check is needed here.
            ResponsesTool::Function(p) => Some(FunctionTool::from(p)),
            ResponsesTool::Mcp(p) => {
                tracing::debug!(
                    server_label = %p.server_label,
                    "MCP tool skipped in normalize — handler not yet registered"
                );
                None
            }
            ResponsesTool::WebSearch(_) => {
                tracing::debug!("web_search tool skipped in normalize — handler not yet registered");
                None
            }
            ResponsesTool::FileSearch(_) => {
                tracing::debug!("file_search tool skipped in normalize — handler not yet registered");
                None
            }
            ResponsesTool::CodeInterpreter(_) => {
                tracing::debug!("code_interpreter tool skipped in normalize — handler not yet registered");
                None
            }
        }
    }
}

impl From<ToolOutput> for FunctionToolResultMessage {
    fn from(o: ToolOutput) -> Self {
        Self {
            call_id: o.call_id,
            output: o.output,
        }
    }
}
