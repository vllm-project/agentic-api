use crate::types::io::FunctionTool;
use crate::types::io::input::FunctionToolResultMessage;
use crate::types::tools::ResponsesTool;

use super::code_interpreter::CodeInterpreterHandler;
use super::codex::CodexNamespaceHandler;
use super::custom::CustomHandler;
use super::function::FunctionHandler;
use super::handler::{ToolError, ToolHandler, ToolOutput};
use super::mcp::McpHandler;
use super::registry::ToolType;
use super::shell::ShellHandler;
use super::tool_search::ToolSearchHandler;
use super::web_search::web_search_function_tool;

#[cfg(not(feature = "embedded-code-interpreter"))]
const CODE_INTERPRETER_UNAVAILABLE: &str =
    "code_interpreter is disabled; rebuild with the embedded-code-interpreter feature";

#[cfg(feature = "embedded-code-interpreter")]
const CODE_INTERPRETER_UNAVAILABLE: &str =
    "code_interpreter is disabled by operator configuration or its embedded runtime is not ready";

pub(crate) fn code_interpreter_unavailable_error() -> ToolError {
    ToolError::Config(CODE_INTERPRETER_UNAVAILABLE.to_owned())
}

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
            Self::WebSearch(_) | Self::FileSearch(_) | Self::Unknown => Ok(()),
            // The declaration shape is validated in every build. A default build
            // has no Eryx dependency and cannot normalize or register it, so
            // feature-enabled availability is decided by the Responses registry
            // after startup readiness.
            Self::CodeInterpreter(param) => CodeInterpreterHandler.validate(param),
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
    /// - The default build keeps `CodeInterpreter` out of model-visible
    ///   normalization. Feature-enabled builds expose its fixed function
    ///   contract for typed contract tests, but request validation remains
    ///   fail-closed until a ready runtime exists.
    /// - Unimplemented `FileSearch` variants return an empty list and emit a
    ///   `tracing::debug!`.
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
            Self::FileSearch(_) => {
                tracing::debug!("file_search tool skipped in normalize - handler not yet registered");
                vec![]
            }
            #[cfg(feature = "embedded-code-interpreter")]
            Self::CodeInterpreter(param) => CodeInterpreterHandler.normalize(param),
            #[cfg(not(feature = "embedded-code-interpreter"))]
            Self::CodeInterpreter(_) => {
                tracing::debug!("code_interpreter tool cannot normalize without an available handler");
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
mod tests {
    use super::*;

    #[cfg(not(feature = "embedded-code-interpreter"))]
    #[test]
    fn unavailable_code_interpreter_does_not_create_a_model_visible_function() {
        let tool: ResponsesTool = serde_json::from_value(serde_json::json!({
            "type": "code_interpreter",
            "execution": "gateway"
        }))
        .expect("tool parses");

        assert!(tool.to_function_tools().is_empty());
    }

    #[cfg(feature = "embedded-code-interpreter")]
    #[test]
    fn enabled_code_interpreter_exposes_its_fixed_function_contract() {
        let tool: ResponsesTool = serde_json::from_value(serde_json::json!({
            "type": "code_interpreter",
            "execution": "gateway"
        }))
        .expect("tool parses");

        let [function] = tool
            .to_function_tools()
            .try_into()
            .expect("feature-enabled normalization emits exactly one function");
        assert_eq!(function.name, "code_interpreter");
        assert_eq!(function.strict, Some(true));
    }
}
