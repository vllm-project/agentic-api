use crate::types::io::FunctionTool;
use crate::types::io::input::FunctionToolResultMessage;
use crate::types::tools::ResponsesTool;

use super::code_interpreter::CodeInterpreterHandler;
use super::codex::CodexNamespaceHandler;
use super::custom::CustomHandler;
use super::declaration::{DeclaredTool, ToolDeclaration, ToolDeclarationRef};
use super::function::FunctionHandler;
use super::handler::{ToolError, ToolHandler, ToolOutput};
use super::mcp::McpHandler;
use super::registry::ToolType;
use super::shell::ShellHandler;
use super::tool_search::ToolSearchHandler;
use super::web_fetch::WebFetchHandler;
use super::web_search::{WebSearchHandler, web_search_function_tool};

#[cfg(not(feature = "embedded-code-interpreter"))]
const CODE_INTERPRETER_UNAVAILABLE: &str =
    "code_interpreter is disabled; rebuild with the embedded-code-interpreter feature";

#[cfg(feature = "embedded-code-interpreter")]
const CODE_INTERPRETER_UNAVAILABLE: &str =
    "code_interpreter is disabled by operator configuration or its embedded runtime is not ready";

pub(crate) fn code_interpreter_unavailable_error() -> ToolError {
    ToolError::Config(CODE_INTERPRETER_UNAVAILABLE.to_owned())
}

/// Declaration-level validation and normalization, implemented once on the
/// tool layer's view of a declaration and reached from both the owned
/// [`ToolDeclaration`] and the wire [`ResponsesTool`].
impl ToolDeclarationRef<'_> {
    /// Validate this declaration through its tool handler before normalization.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when the declaration cannot be safely
    /// represented by the corresponding model-visible tool.
    pub fn validate(self) -> Result<(), ToolError> {
        match self {
            Self::Function(param) => FunctionHandler.validate(param),
            Self::Mcp(param) => McpHandler::spec_from_param(param).validate(param),
            Self::ToolSearch(param) => ToolSearchHandler.validate(param),
            Self::WebSearch(param) => WebSearchHandler::spec_only().validate(param),
            Self::WebFetch(param) => WebFetchHandler::spec_only().validate(param),
            Self::FileSearch(_) | Self::Unsupported => Ok(()),
            // Runtime availability is checked before the request is normalized.
            Self::CodeInterpreter(param) => CodeInterpreterHandler.validate(param),
            Self::Shell(param) => ShellHandler.validate(param),
            Self::Namespace(param) => CodexNamespaceHandler.validate(param),
            Self::Custom(param) => CustomHandler.validate(param),
        }
    }

    /// Return the gateway routing type this declaration would register as.
    #[must_use]
    pub fn tool_type(self) -> Option<ToolType> {
        match self {
            Self::Function(_) => Some(ToolType::Function),
            Self::ToolSearch(_) => Some(ToolType::ToolSearch),
            Self::Mcp(_) => Some(ToolType::Mcp),
            Self::WebSearch(_) => Some(ToolType::WebSearch),
            Self::WebFetch(_) => Some(ToolType::WebFetch),
            Self::FileSearch(_) => Some(ToolType::FileSearch),
            Self::CodeInterpreter(_) => Some(ToolType::CodeInterpreter),
            Self::Namespace(_) => Some(ToolType::CodexNamespace),
            Self::Custom(_) => Some(ToolType::Custom),
            Self::Shell(_) => Some(ToolType::Shell),
            Self::Unsupported => None,
        }
    }

    #[must_use]
    pub fn is_gateway_owned(self) -> bool {
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
    /// - `CodeInterpreter` lowers to its fixed function contract in every build;
    ///   supported request flows reject an unavailable runtime before calling
    ///   this conversion.
    /// - Unimplemented `FileSearch` variants return an empty list and emit a
    ///   `tracing::debug!`.
    ///
    /// `RequestPayload::to_upstream_request()` uses this conversion for
    /// all model-visible tools.
    ///
    /// [`From<&FunctionToolParam>`]: crate::types::tools::FunctionToolParam
    #[must_use]
    pub fn to_function_tools(self) -> Vec<FunctionTool> {
        match self {
            // name is NonEmptyToolName — empty names are rejected by serde at
            // deserialization time, so no runtime check is needed here.
            Self::Function(param) => FunctionHandler.normalize(param).into_iter().take(1).collect(),
            Self::Mcp(param) => McpHandler::spec_from_param(param).normalize(param),
            Self::ToolSearch(param) => ToolSearchHandler.normalize(param).into_iter().take(1).collect(),
            Self::WebSearch(_) => vec![web_search_function_tool()],
            Self::WebFetch(param) => WebFetchHandler::spec_only().normalize(param),
            Self::FileSearch(_) => {
                tracing::debug!("file_search tool skipped in normalize - handler not yet registered");
                vec![]
            }
            Self::CodeInterpreter(param) => CodeInterpreterHandler.normalize(param),
            Self::Shell(param) => ShellHandler.normalize(param),
            Self::Namespace(param) => CodexNamespaceHandler.normalize(param),
            Self::Custom(param) => CustomHandler.normalize(param),
            Self::Unsupported => {
                tracing::debug!("unsupported tool skipped in normalize");
                vec![]
            }
        }
    }
}

impl ToolDeclaration {
    /// Validate this declaration through its tool handler before normalization.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when the declaration cannot be safely
    /// represented by the corresponding model-visible tool.
    pub fn validate(&self) -> Result<(), ToolError> {
        self.declaration().validate()
    }

    /// Return the gateway routing type this declaration would register as.
    #[must_use]
    pub fn tool_type(&self) -> Option<ToolType> {
        self.declaration().tool_type()
    }

    #[must_use]
    pub fn is_gateway_owned(&self) -> bool {
        self.declaration().is_gateway_owned()
    }

    /// The `FunctionTool`s vLLM sees for this declaration; see
    /// [`ToolDeclarationRef::to_function_tools`].
    #[must_use]
    pub fn to_function_tools(&self) -> Vec<FunctionTool> {
        self.declaration().to_function_tools()
    }
}

/// The wire declaration reaches the same handler contract through its view;
/// the Responses request path (`RequestPayload::to_upstream_request`) and
/// request validation call these.
impl ResponsesTool {
    /// Validate this declaration through its tool handler before normalization.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when the declaration cannot be safely
    /// represented by the corresponding model-visible tool.
    pub fn validate(&self) -> Result<(), ToolError> {
        self.declaration().validate()
    }

    /// Return the gateway routing type this declaration would register as.
    #[must_use]
    pub fn tool_type(&self) -> Option<ToolType> {
        self.declaration().tool_type()
    }

    #[must_use]
    pub fn is_gateway_owned(&self) -> bool {
        self.declaration().is_gateway_owned()
    }

    /// The `FunctionTool`s vLLM sees for this declaration; see
    /// [`ToolDeclarationRef::to_function_tools`].
    #[must_use]
    pub fn to_function_tools(&self) -> Vec<FunctionTool> {
        self.declaration().to_function_tools()
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

    #[test]
    fn code_interpreter_normalizes_to_its_fixed_function_contract() {
        let tool: ResponsesTool = serde_json::from_value(serde_json::json!({
            "type": "code_interpreter",
            "container": {"type": "auto"}
        }))
        .expect("tool parses");

        let [function] = tool
            .to_function_tools()
            .try_into()
            .expect("normalization emits exactly one function");
        assert_eq!(function.name, "code_interpreter");
        assert_eq!(function.strict, Some(true));
    }

    #[test]
    fn a_web_fetch_declaration_validates_and_normalizes_through_its_handler() {
        let declaration = ToolDeclaration::WebFetch(crate::types::tools::WebFetchToolParam::default());
        assert!(declaration.validate().is_ok());
        assert_eq!(declaration.tool_type(), Some(ToolType::WebFetch));
        assert!(declaration.is_gateway_owned());
        let [function] = declaration
            .to_function_tools()
            .try_into()
            .expect("normalization emits exactly one function");
        assert_eq!(function.name, "web_fetch");

        let invalid = ToolDeclaration::WebFetch(crate::types::tools::WebFetchToolParam {
            filters: Some(crate::types::tools::DomainFilters {
                allowed_domains: Some(vec![".".to_owned()]),
                blocked_domains: None,
            }),
            max_content_tokens: None,
        });
        let error = invalid.validate().unwrap_err();
        assert!(error.to_string().contains("is not a host name"), "{error}");
    }
}
