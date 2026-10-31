//! Typed Messages MCP connector declarations and public content projections.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::GatewayToolResult;

#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MessagesMcpServer {
    #[serde(rename = "type")]
    pub kind: MessagesMcpServerType,
    pub url: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_token: Option<String>,
}

impl fmt::Debug for MessagesMcpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessagesMcpServer")
            .field("name", &self.name)
            .field(
                "authorization_token",
                &self.authorization_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum MessagesMcpServerType {
    Url,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct McpToolConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct McpToolsetConfig {
    pub default_config: McpToolConfig,
    pub configs: HashMap<String, McpToolConfig>,
}

impl McpToolsetConfig {
    pub(crate) fn effective(&self, name: &str) -> (bool, bool) {
        let config = self.configs.get(name);
        (
            config
                .and_then(|c| c.enabled)
                .or(self.default_config.enabled)
                .unwrap_or(true),
            config
                .and_then(|c| c.defer_loading)
                .or(self.default_config.defer_loading)
                .unwrap_or(false),
        )
    }
}

/// A supported MCP toolset declaration. It deliberately has no function name.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MessagesMcpToolset {
    pub mcp_server_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_config: Option<McpToolConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configs: Option<HashMap<String, McpToolConfig>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// A pending connector call carried by a mixed-turn continuation.
/// Object arguments are retained verbatim; execution identity is resolved against
/// the current request registry before this call can be dispatched.
#[derive(Debug, Deserialize)]
pub(crate) struct PendingMcpCall {
    pub id: String,
    pub name: String,
    pub server_name: String,
    pub input: Map<String, Value>,
}

/// Public Messages projections; execution and call IDs stay in the shared tool path.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpContentBlock {
    McpToolUse {
        id: String,
        name: String,
        server_name: String,
        input: Value,
    },
    McpToolResult {
        tool_use_id: String,
        is_error: bool,
        content: Vec<McpTextBlock>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct McpTextBlock {
    #[serde(rename = "type")]
    kind: McpTextKind,
    text: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum McpTextKind {
    Text,
}

impl McpContentBlock {
    #[must_use]
    pub fn input_json(&self) -> Option<String> {
        match self {
            Self::McpToolUse { input, .. } => Some(input.to_string()),
            Self::McpToolResult { .. } => None,
        }
    }

    #[must_use]
    pub fn with_empty_input(mut self) -> Self {
        if let Self::McpToolUse { input, .. } = &mut self {
            *input = serde_json::json!({});
        }
        self
    }

    #[must_use]
    pub fn result(result: &GatewayToolResult) -> Self {
        Self::McpToolResult {
            tool_use_id: result.tool_use_id.clone(),
            is_error: result.is_error,
            content: vec![McpTextBlock {
                kind: McpTextKind::Text,
                text: result.content.clone(),
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::mcp::messages::connector_tools;
    use crate::types::messages::ToolParam;
    use serde_json::json;

    #[test]
    fn tool_settings_merge_each_field_independently() {
        let tool: ToolParam = serde_json::from_value(json!({"type":"mcp_toolset", "mcp_server_name":"s",
            "default_config":{"enabled":false, "defer_loading":true},
            "configs":{"enabled":{"enabled":true}, "eager":{"enabled":true,"defer_loading":false}, "disabled":{"defer_loading":false}}
        })).unwrap();
        let ToolParam::McpToolset(tool) = tool else {
            panic!("expected MCP toolset")
        };
        let config = McpToolsetConfig {
            default_config: tool.default_config.unwrap(),
            configs: tool.configs.unwrap(),
        };
        assert_eq!(config.effective("unknown"), (false, true));
        assert_eq!(config.effective("enabled"), (true, true));
        assert_eq!(config.effective("eager"), (true, false));
        assert_eq!(config.effective("disabled"), (false, false));
        assert_eq!(McpToolsetConfig::default().effective("any"), (true, false));
    }

    #[test]
    fn connector_rejects_ambiguous_and_unused_servers() {
        let server: MessagesMcpServer =
            serde_json::from_value(json!({"type":"url", "name":"s", "url":"https://example.com/mcp"})).unwrap();
        let tool: ToolParam = serde_json::from_value(json!({"type":"mcp_toolset", "mcp_server_name":"s"})).unwrap();
        assert!(connector_tools(std::slice::from_ref(&server), std::slice::from_ref(&tool)).is_ok());
        assert!(connector_tools(&[server.clone(), server.clone()], std::slice::from_ref(&tool)).is_err());
        assert!(connector_tools(std::slice::from_ref(&server), &[tool.clone(), tool.clone()]).is_err());
        assert!(connector_tools(std::slice::from_ref(&server), &[]).is_err());
        assert!(connector_tools(&[], std::slice::from_ref(&tool)).is_err());
        let mut invalid = server;
        invalid.url = "http://example.com/mcp".to_owned();
        assert!(connector_tools(&[invalid], &[tool]).is_err());
    }
}
