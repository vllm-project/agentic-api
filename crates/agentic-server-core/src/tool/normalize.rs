use crate::types::io::FunctionTool;
use crate::types::io::input::FunctionToolResultMessage;
use crate::types::tools::ResponsesTool;

use super::codex::CodexNamespaceHandler;
use super::custom::CustomHandler;
use super::file_search::handler::FileSearchHandler;
use super::function::FunctionHandler;
use super::handler::{ToolError, ToolHandler, ToolOutput};
use super::mcp::McpHandler;
use super::registry::ToolType;
use super::shell::ShellHandler;
use super::tool_search::ToolSearchHandler;
use super::web_search::web_search_function_tool;

impl ResponsesTool {
    /// Validate this declaration through its tool handler before normalization.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when the declaration cannot be safely
    /// represented by the corresponding model-visible tool.
    pub fn validate(&self) -> Result<(), ToolError> {
        match self {
            Self::Function(param) => FunctionHandler.validate(param),
            Self::Mcp(param) => McpHandler::spec_from_param(param).validate(param),
            Self::ToolSearch(param) => ToolSearchHandler.validate(param),
            Self::FileSearch(param) => FileSearchHandler::spec_only().validate(param),
            Self::WebSearch(_) | Self::CodeInterpreter(_) | Self::Unknown => Ok(()),
            Self::Shell(param) => ShellHandler.validate(param),
            Self::Namespace(param) => CodexNamespaceHandler.validate(param),
            Self::Custom(param) => CustomHandler.validate(param),
        }
    }

    /// Return the gateway routing type this declaration would register as.
    #[must_use]
    pub fn tool_type(&self) -> Option<ToolType> {
        match self {
            Self::Function(_) => Some(ToolType::Function),
            Self::ToolSearch(_) => Some(ToolType::ToolSearch),
            Self::Mcp(_) => Some(ToolType::Mcp),
            Self::WebSearch(_) => Some(ToolType::WebSearch),
            Self::FileSearch(_) => Some(ToolType::FileSearch),
            Self::CodeInterpreter(_) => Some(ToolType::CodeInterpreter),
            Self::Namespace(_) => Some(ToolType::CodexNamespace),
            Self::Custom(_) => Some(ToolType::Custom),
            Self::Shell(_) => Some(ToolType::Shell),
            Self::Unknown => None,
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
    /// - `ToolSearch` variants lower through [`ToolSearchHandler`] to the
    ///   synthetic client-executed function understood by vLLM.
    /// - `Mcp` variants convert gateway MCP built-ins to the function specs
    ///   vLLM can call.
    /// - Unformatted `Custom` variants become function tools with one string
    ///   `input` parameter; formatted declarations are rejected by the request
    ///   path because normalization cannot preserve constrained decoding.
    /// - Unimplemented variants (`CodeInterpreter`) return
    ///   an empty list and emit a `tracing::debug!`.
    ///
    /// `RequestPayload::to_upstream_request()` uses this conversion for
    /// all model-visible tools.
    #[must_use]
    pub fn to_function_tools(&self) -> Vec<FunctionTool> {
        match self {
            // name is NonEmptyToolName — empty names are rejected by serde at
            // deserialization time, so no runtime check is needed here.
            Self::Function(param) => FunctionHandler.normalize(param).into_iter().take(1).collect(),
            Self::Mcp(param) => McpHandler::spec_from_param(param).normalize(param),
            Self::ToolSearch(param) => ToolSearchHandler.normalize(param).into_iter().take(1).collect(),
            Self::WebSearch(_) => vec![web_search_function_tool()],
            Self::FileSearch(param) => FileSearchHandler::spec_only().normalize(param),
            Self::CodeInterpreter(_) => {
                tracing::debug!("code_interpreter tool skipped in normalize - handler not yet registered");
                vec![]
            }
            Self::Shell(param) => ShellHandler.normalize(param),
            Self::Namespace(param) => CodexNamespaceHandler.normalize(param),
            Self::Custom(param) => CustomHandler.normalize(param),
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
            output: o.output.into(),
        }
    }
}

#[cfg(test)]
mod file_search_tests {
    use crate::types::tools::ResponsesTool;

    #[test]
    fn file_search_normalizes_to_executable_function() {
        let tool: ResponsesTool = serde_json::from_value(serde_json::json!({
            "type": "file_search", "vector_store_ids": ["vs_documents"]
        }))
        .unwrap();
        tool.validate().unwrap();
        let functions = tool.to_function_tools();
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name, "file_search");
    }

    #[test]
    fn file_search_rejects_empty_stores_and_invalid_limits() {
        for declaration in [
            serde_json::json!({"type": "file_search"}),
            serde_json::json!({"type": "file_search", "vector_store_ids": []}),
            serde_json::json!({"type": "file_search", "vector_store_ids": [" "]}),
            serde_json::json!({"type": "file_search", "vector_store_ids": ["vs_1"], "max_num_results": 0}),
            serde_json::json!({"type": "file_search", "vector_store_ids": ["vs_1"], "max_num_results": 51}),
            serde_json::json!({"type": "file_search", "vector_store_ids": ["vs_1"], "ranking_options": {"ranker":"neural"}}),
            serde_json::json!({"type": "file_search", "vector_store_ids": ["vs_1"], "ranking_options": {"score_threshold":2.0}}),
        ] {
            let tool: ResponsesTool = serde_json::from_value(declaration).unwrap();
            assert!(tool.validate().is_err());
        }
    }
}
