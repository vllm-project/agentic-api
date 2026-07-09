use crate::types::io::FunctionTool;
use crate::types::io::input::FunctionToolResultMessage;
use crate::types::tools::ResponsesTool;

use super::codex::CodexNamespaceHandler;
use super::handler::{ToolHandler, ToolOutput};
use super::web_search::web_search_function_tool;

impl ResponsesTool {
    /// Normalise this tool declaration to the `FunctionTool` wire format that vLLM understands.
    ///
    /// - `Function` variants convert via [`From<&FunctionToolParam>`] for `FunctionTool`.
    ///   Returns `None` and logs at `debug` level if the name is empty.
    ///
    /// This is the entry point called by `RequestPayload::to_upstream_request()` so that
    /// vLLM always receives a `Vec<FunctionTool>`, never a raw `ResponsesTool` enum.
    ///
    /// # Panics
    ///
    /// Panics if serializing a `CodexNamespaceToolParam` fails, which cannot happen
    /// for the derive-generated `Serialize` impl on that struct.
    #[must_use]
    pub fn to_function_tools(&self) -> Vec<FunctionTool> {
        match self {
            // name is NonEmptyToolName — empty names are rejected by serde at
            // deserialization time, so no runtime check is needed here.
            Self::Function(p) => vec![FunctionTool::from(p)],
            Self::Mcp(p) => {
                tracing::debug!(
                    server_label = %p.server_label,
                    "MCP tool skipped in normalize - handler not yet registered"
                );
                vec![]
            }
            Self::WebSearch(_) => vec![web_search_function_tool()],
            Self::FileSearch(_) => {
                tracing::debug!("file_search tool skipped in normalize - handler not yet registered");
                vec![]
            }
            Self::CodeInterpreter(_) => {
                tracing::debug!("code_interpreter tool skipped in normalize - handler not yet registered");
                vec![]
            }
            Self::Namespace(p) => {
                let param = serde_json::to_value(p).expect("serialization of known struct is infallible");
                CodexNamespaceHandler.normalize(&param)
            }
            Self::Unknown => {
                tracing::debug!("unknown tool skipped in normalize");
                vec![]
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
