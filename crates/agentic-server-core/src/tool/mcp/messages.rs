//! Adapt Messages connector declarations to protocol-neutral tools.
use crate::tool::{ToolDeclaration, ToolError};
use crate::types::messages::ToolParam;
use crate::types::messages::mcp::{McpToolsetConfig, MessagesMcpServer};
use crate::types::tools::McpToolParam;
use std::collections::HashSet;

/// Resolve toolsets to the same typed declarations used by Responses discovery.
///
/// # Errors
/// Rejects ambiguous server identities and missing or duplicate toolsets before I/O.
pub fn connector_tools(servers: &[MessagesMcpServer], tools: &[ToolParam]) -> Result<Vec<ToolDeclaration>, ToolError> {
    let mut names = HashSet::new();
    for server in servers {
        if server.name.is_empty() || !names.insert(server.name.as_str()) {
            return Err(ToolError::Config(
                "MCP server names must be non-empty and unique".to_owned(),
            ));
        }
        if !server.url.starts_with("https://") {
            return Err(ToolError::Config("Messages MCP server URLs must use HTTPS".to_owned()));
        }
    }
    let mut used = HashSet::new();
    let mut resolved = Vec::new();
    for tool in tools.iter().filter(|t| t.type_.as_deref() == Some("mcp_toolset")) {
        let server = tool
            .mcp_server_name
            .as_deref()
            .and_then(|name| servers.iter().find(|server| server.name == name))
            .ok_or_else(|| ToolError::Config("mcp_toolset must reference a declared MCP server".to_owned()))?;
        if !used.insert(server.name.as_str()) {
            return Err(ToolError::Config(
                "each MCP server must have exactly one mcp_toolset".to_owned(),
            ));
        }
        resolved.push(ToolDeclaration::Mcp(McpToolParam {
            server_label: server.name.clone(),
            server_url: Some(server.url.clone()),
            authorization: server.authorization_token.clone(),
            connector_id: None,
            headers: None,
            allowed_tools: None,
            require_approval: Some("never".to_owned()),
            defer_loading: None,
            discovered_tools: Vec::new(),
            messages_config: Some(McpToolsetConfig {
                default_config: tool.default_config.clone().unwrap_or_default(),
                configs: tool.configs.clone().unwrap_or_default(),
            }),
        }));
    }
    if used.len() != servers.len() {
        return Err(ToolError::Config(
            "each MCP server must have exactly one mcp_toolset".to_owned(),
        ));
    }
    Ok(resolved)
}
