//! Wire format types for the tool framework.
//!
//! Behavioral logic (registry, handler trait, normalization) lives in [`crate::tool`].

/// This module contains validated model-call argument types.
pub mod code_interpreter;
/// Domain lists shared by the web tools' declarations.
pub mod domain;
pub mod mcp;
/// This module contains only serde shapes (serialization/deserialization types).
pub mod params;
/// Declaration parameters of the Messages-only `web_fetch` tool.
pub mod web_fetch;

pub use code_interpreter::{CodeInterpreterCallArguments, CodeInterpreterCallArgumentsError};
pub use domain::DomainFilters;
pub use params::{
    CodeInterpreterToolParam, CodexNamespaceMember, CodexNamespaceToolParam, CustomToolParam, EmptyToolNameError,
    FileSearchToolParam, FunctionToolParam, LocalShellEnvironment, McpDiscoveredToolParam, McpToolParam,
    NonEmptyToolName, ResponsesTool, ShellEnvironment, ShellToolParam, ToolSearchExecution, ToolSearchStatus,
    ToolSearchToolParam, WebSearchContextSize, WebSearchToolParam, WebSearchUserLocation,
};
pub use web_fetch::WebFetchToolParam;
