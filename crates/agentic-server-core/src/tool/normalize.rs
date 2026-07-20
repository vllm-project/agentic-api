use crate::types::io::FunctionTool;
use crate::types::io::input::FunctionToolResultMessage;
use crate::types::tools::ResponsesTool;
use crate::utils::common::serialize_to_value_or_custom_default;

use super::codex::CodexNamespaceHandler;
use super::function::FunctionHandler;
use super::handler::{ToolHandler, ToolOutput};
use super::mcp::{McpHandler, maybe_mcp_function};
use super::registry::ToolType;
use super::web_search::web_search_function_tool;

impl ResponsesTool {
    /// Return the gateway routing type this declaration would register as.
    #[must_use]
    pub fn tool_type(&self) -> Option<ToolType> {
        match self {
            Self::Function(p) => match maybe_mcp_function(p) {
                Some(params) if !params.is_empty() => Some(ToolType::Mcp),
                _ => Some(ToolType::Function),
            },
            Self::Mcp(_) => Some(ToolType::Mcp),
            Self::WebSearch(_) => Some(ToolType::WebSearch),
            Self::FileSearch(_) => Some(ToolType::FileSearch),
            Self::CodeInterpreter(_) => Some(ToolType::CodeInterpreter),
            Self::Namespace(_) => Some(ToolType::CodexNamespace),
            Self::Custom(_) | Self::Unknown => None,
        }
    }

    #[must_use]
    pub fn is_gateway_owned(&self) -> bool {
        self.tool_type().is_some_and(ToolType::is_gateway_owned)
    }

    /// Normalise function-like tool declarations to the `FunctionTool` wire format that vLLM understands.
    ///
    /// - `Function` variants convert via [`From<&FunctionToolParam>`] for `FunctionTool`.
    ///   Returns an empty list and logs at `debug` level if the name is empty.
    /// - `Mcp` variants convert gateway MCP built-ins to the function specs
    ///   vLLM can call.
    /// - `Custom` variants return no function tools because
    ///   `RequestPayload::to_upstream_request()` forwards their native
    ///   Responses declarations separately.
    /// - Unimplemented variants (`FileSearch`, `CodeInterpreter`) return
    ///   an empty list and emit a `tracing::debug!`.
    ///
    /// `RequestPayload::to_upstream_request()` uses this conversion for
    /// function-like tools while preserving native custom declarations in its
    /// heterogeneous upstream tool list.
    #[must_use]
    pub fn to_function_tools(&self) -> Vec<FunctionTool> {
        match self {
            // name is NonEmptyToolName — empty names are rejected by serde at
            // deserialization time, so no runtime check is needed here.
            Self::Function(p) => serialize_to_value_or_custom_default(
                p,
                "function tool config serialization failed",
                |param| FunctionHandler.normalize(&param).into_iter().take(1).collect(),
                vec![],
            ),
            Self::Mcp(p) => serialize_to_value_or_custom_default(
                p,
                "MCP tool config serialization failed",
                |param| McpHandler::spec_from_param(&param).normalize(&param),
                vec![],
            ),
            Self::WebSearch(_) => vec![web_search_function_tool()],
            Self::FileSearch(_) => {
                tracing::debug!("file_search tool skipped in normalize - handler not yet registered");
                vec![]
            }
            Self::CodeInterpreter(_) => {
                tracing::debug!("code_interpreter tool skipped in normalize - handler not yet registered");
                vec![]
            }
            Self::Namespace(p) => serialize_to_value_or_custom_default(
                p,
                "function tool config serialization failed",
                |param| CodexNamespaceHandler.normalize(&param),
                vec![],
            ),
            Self::Custom(p) => {
                tracing::debug!(name = %p.name, "custom tool retained for native upstream forwarding");
                vec![]
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
