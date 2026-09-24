//! Wire format types for the tool framework.
//!
//! Behavioral logic (registry, handler trait, normalization) lives in [`crate::tool`].

/// This module contains validated model-call argument types.
pub mod code_interpreter;
/// This module contains only serde shapes (serialization/deserialization types).
pub mod params;

pub use code_interpreter::{CodeInterpreterCallArguments, CodeInterpreterCallArgumentsError};
pub use params::{
    CodeInterpreterExecution, CodeInterpreterToolParam, CodexNamespaceMember, CodexNamespaceToolParam, CustomToolParam,
    EmptyToolNameError, FileSearchToolParam, FunctionToolParam, LocalShellEnvironment, McpDiscoveredToolParam,
    McpToolParam, NonEmptyToolName, ResponsesTool, ShellEnvironment, ShellToolParam, ToolSearchExecution,
    ToolSearchStatus, ToolSearchToolParam, WebSearchContextSize, WebSearchFilters, WebSearchToolParam,
    WebSearchUserLocation,
};
