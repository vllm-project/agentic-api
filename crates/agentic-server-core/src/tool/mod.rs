//! Tool framework — registry, handler trait, and normalization pipeline.
//!
//! Wire format types (`ResponsesTool`, param structs) live in [`crate::types::tools`].
//! This module owns the behavioral layer: routing, handler interface, and normalization.

pub mod async_execution;
pub(crate) mod catalog;
pub mod code_interpreter;
pub mod codex;
pub mod custom;
pub mod declaration;
pub(crate) mod domain_policy;
pub mod executors;
pub mod file_search;
pub mod function;
pub mod handler;
pub mod mcp;
pub mod normalize;
pub mod ownership;
pub mod registry;
pub mod shell;
pub mod tool_search;
pub mod web_fetch;
pub mod web_search;

pub use code_interpreter::CodeInterpreterHandler;
pub use codex::{CodexNamespaceHandler, NamespaceMap, model_visible_namespace_member_name};
pub use custom::CustomHandler;
pub use declaration::{
    ToolDeclaration, declares_async, record_discovered_mcp_tools, registry_tools, responses_declarations,
};
pub use executors::{GatewayExecutorRegistration, GatewayExecutors};
pub use function::FunctionHandler;
pub use handler::{GatewayExecutor, GatewayToolEventPlan, ToolError, ToolHandler, ToolOutput, ToolOutputStatus};
pub use mcp::{McpClient, McpClientPool, McpDiscoveredHandler, McpError, McpHandler, McpOperation, McpServerEntry};
pub use ownership::{GatewayBinding, ToolOwnership};
pub use registry::{GatewayDispatchResult, ToolEntry, ToolRegistry, ToolType};
pub use shell::ShellHandler;
pub(crate) use tool_search::ToolSearchMetadata;
pub use tool_search::{ToolSearchHandler, ToolSearchState};
pub use web_fetch::WebFetchHandler;
pub use web_search::WebSearchHandler;
pub use web_search::searxng::{SEARXNG_BASE_URL_HINT, validate_searxng_base_url};

/// Validate a `web_search` `tool_choice` against the effective declarations.
///
/// A client function named `web_search` is a [`ToolDeclaration::Function`], not
/// a built-in, so forcing the `web_search` selector against it would return a
/// client `function_call` instead of running gateway search. Mirrors the
/// `file_search` declaration check and lives here rather than in the already
/// large `web_search` module to stay within the production-line limit.
pub(crate) fn validate_web_search_tool_choice(
    declarations: Option<&[ToolDeclaration]>,
    choice: &crate::types::io::ToolChoice,
) -> Result<(), ToolError> {
    use crate::types::io::ToolChoice;
    let selected = match choice {
        ToolChoice::WebSearch => true,
        ToolChoice::AllowedTools { tools, .. } => tools.iter().any(|tool| tool.type_.as_str() == "web_search"),
        _ => false,
    };
    if selected
        && !declarations.is_some_and(|tools| tools.iter().any(|tool| matches!(tool, ToolDeclaration::WebSearch(_))))
    {
        return Err(ToolError::Config(
            "web_search tool_choice requires a declared web_search tool".to_owned(),
        ));
    }
    Ok(())
}
