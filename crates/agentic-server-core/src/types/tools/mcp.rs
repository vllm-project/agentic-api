use crate::types::messages::mcp::McpToolsetConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Parameters for a gateway MCP built-in tool declaration.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct McpToolParam {
    /// Messages connector selection, applied before registration.
    #[serde(skip)]
    #[cfg_attr(feature = "openapi", schema(ignore))]
    pub(crate) messages_config: Option<McpToolsetConfig>,
    pub server_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_approval: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    /// `async` applies only to function and custom tools; captured so declaration validation can reject it.
    #[serde(default, rename = "async", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(ignore))]
    pub unsupported_async: Option<bool>,
    /// Catalog text for a deferred server; hosted tool search returns it as the
    /// description of the server's `mcp_<server_label>` namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_description: Option<String>,
    /// Request-scoped `tools/list` results used by MCP normalization. This
    /// field is populated internally and ignored on the public request wire.
    #[serde(
        rename = "_agentic_discovered_tools",
        default,
        skip_deserializing,
        skip_serializing_if = "Vec::is_empty"
    )]
    #[cfg_attr(feature = "openapi", schema(ignore))]
    pub(crate) discovered_tools: Vec<McpDiscoveredToolParam>,
    /// For a server deferred behind hosted tool search, the discovered tools
    /// loaded so far; upstream normalization exposes only these. `None` for a
    /// server that is not deferred. Internal and never on the wire.
    #[serde(skip)]
    #[cfg_attr(feature = "openapi", schema(ignore))]
    pub(crate) loaded_tool_names: Option<Vec<String>>,
}

/// Parameters for a discovered MCP (Model Context Protocol) server tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpDiscoveredToolParam {
    pub server_label: String,
    pub tool_name: String,
    pub internal_name: String,
    pub tool: rmcp::model::Tool,
}

impl std::fmt::Debug for McpToolParam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolParam")
            .field("server_label", &self.server_label)
            .field("authorization", &self.authorization.as_ref().map(|_| "[REDACTED]"))
            .finish_non_exhaustive()
    }
}
