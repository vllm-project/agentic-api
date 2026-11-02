use std::collections::HashMap;
use std::sync::Arc;

use reqwest::Url;
use serde::{Deserialize, Serialize};

use super::client::McpClient;
use crate::types::tools::McpToolParam;

// Hostnames configured here are a trust boundary. The HTTP client resolves a
// configured name once per connection and pins all returned addresses for that
// transport. Only add names whose DNS records are controlled by a trusted
// administrator.
const MCP_ALLOWED_HOSTS_ENV: &str = "AGENTIC_MCP_ALLOWED_HOSTS";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum McpServerEntry {
    Http {
        url: String,
        #[serde(default, alias = "http_headers", skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        allowed_tools: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        require_approval: Option<String>,
        /// Catalog text for hosted tool search when a request omits `server_description`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Defers this server's tools behind hosted tool search unless a request sets `defer_loading`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
    },
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<HashMap<String, String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        allowed_tools: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        require_approval: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
    },
}

impl McpServerEntry {
    #[must_use]
    pub fn allowed_tools(&self) -> Option<&[String]> {
        match self {
            Self::Http { allowed_tools, .. } | Self::Stdio { allowed_tools, .. } => allowed_tools.as_deref(),
        }
    }

    #[must_use]
    pub fn require_approval(&self) -> Option<&str> {
        match self {
            Self::Http { require_approval, .. } | Self::Stdio { require_approval, .. } => require_approval.as_deref(),
        }
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        match self {
            Self::Http { description, .. } | Self::Stdio { description, .. } => description.as_deref(),
        }
    }

    #[must_use]
    pub const fn defer_loading(&self) -> Option<bool> {
        match self {
            Self::Http { defer_loading, .. } | Self::Stdio { defer_loading, .. } => *defer_loading,
        }
    }
}

#[derive(Default)]
pub struct McpClientPool {
    clients: HashMap<String, Arc<McpClient>>,
    connection_errors: HashMap<String, String>,
}

impl McpClientPool {
    pub async fn from_params(params: &[McpToolParam]) -> Self {
        Self::from_params_with_allowed_hosts(params, &allowed_hosts_from_env()).await
    }

    pub async fn from_params_with_allowed_hosts(params: &[McpToolParam], allowed_hosts: &[String]) -> Self {
        let servers: HashMap<String, McpServerEntry> = params
            .iter()
            .filter_map(|param| server_entry_from_param(param, allowed_hosts))
            .collect();
        Self::from_request_entries(servers).await
    }

    /// Connect validated request-declared servers without trace context or baggage.
    pub(crate) async fn from_request_entries(servers: HashMap<String, McpServerEntry>) -> Self {
        Self::connect_all(servers, false).await
    }

    /// Connect the servers an operator configured. They are trusted with the
    /// caller's trace context and baggage.
    pub async fn from_config(servers: HashMap<String, McpServerEntry>) -> Self {
        Self::connect_all(servers, true).await
    }

    async fn connect_all(servers: HashMap<String, McpServerEntry>, propagate_trace_context: bool) -> Self {
        let mut clients = HashMap::with_capacity(servers.len());
        let mut connection_errors = HashMap::new();

        for (server_label, entry) in servers {
            let result = match entry {
                McpServerEntry::Http { url, headers, .. } => McpClient::connect(&url, headers).await,
                McpServerEntry::Stdio {
                    command,
                    args,
                    env,
                    cwd,
                    ..
                } => McpClient::connect_stdio(&command, &args, env.as_ref(), cwd.as_deref()).await,
            };

            let result = result.map(|client| {
                if propagate_trace_context {
                    client.with_trace_context_propagation()
                } else {
                    client
                }
            });
            match result {
                Ok(client) => {
                    clients.insert(server_label, Arc::new(client));
                }
                Err(error) => {
                    let error_message = error.to_string();
                    tracing::warn!(
                        server_label = %server_label,
                        error = %error_message,
                        "failed to connect MCP server from config"
                    );
                    connection_errors.insert(server_label, error_message);
                }
            }
        }

        Self {
            clients,
            connection_errors,
        }
    }

    #[must_use]
    pub fn get(&self, server_label: &str) -> Option<&Arc<McpClient>> {
        self.clients.get(server_label)
    }

    #[must_use]
    pub fn connection_error(&self, server_label: &str) -> Option<&str> {
        self.connection_errors.get(server_label).map(String::as_str)
    }
}

pub(crate) fn server_entry_from_param(
    param: &McpToolParam,
    allowed_hosts: &[String],
) -> Option<(String, McpServerEntry)> {
    let Some(server_label) = clean_string(Some(&param.server_label)) else {
        tracing::debug!("MCP tool param has no server_label");
        return None;
    };

    if let Some(url) = clean_string(param.server_url.as_deref()) {
        let url = match validate_request_server_url_with_allowed_hosts(&url, allowed_hosts) {
            Ok(url) => url,
            Err(reason) => {
                tracing::warn!(server_label, url, reason, "MCP tool param server_url rejected");
                return None;
            }
        };

        return Some((
            server_label,
            McpServerEntry::Http {
                url,
                headers: request_headers(param),
                allowed_tools: None,
                require_approval: None,
                description: None,
                defer_loading: None,
            },
        ));
    }

    tracing::warn!(server_label, "MCP tool param has no server_url");
    None
}

fn request_headers(param: &McpToolParam) -> Option<HashMap<String, String>> {
    let mut headers = param.headers.clone().unwrap_or_default();
    if let Some(authorization) = clean_string(param.authorization.as_deref()) {
        headers.insert("Authorization".to_owned(), format!("Bearer {authorization}"));
    }
    (!headers.is_empty()).then_some(headers)
}

fn validate_request_server_url_with_allowed_hosts(value: &str, allowed_hosts: &[String]) -> Result<String, String> {
    let url = Url::parse(value).map_err(|error| format!("invalid URL: {error}"))?;
    match url.scheme() {
        "http" | "https" => {}
        _ => return Err("URL scheme must be http or https".to_owned()),
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL must not include credentials".to_owned());
    }

    let host = url.host().ok_or_else(|| "URL must include a host".to_owned())?;
    if is_allowed_request_host(&host, allowed_hosts) {
        return Ok(value.to_owned());
    }

    Err(format!(
        "MCP server_url host is not allowed; set {MCP_ALLOWED_HOSTS_ENV} to allow it"
    ))
}

fn is_allowed_request_host(host: &url::Host<&str>, allowed_hosts: &[String]) -> bool {
    match host {
        url::Host::Domain(host) => host.eq_ignore_ascii_case("localhost") || host_allowed(host, allowed_hosts),
        url::Host::Ipv4(address) => address.is_loopback() || host_allowed(&address.to_string(), allowed_hosts),
        url::Host::Ipv6(address) => address.is_loopback() || host_allowed(&address.to_string(), allowed_hosts),
    }
}

fn host_allowed(host: &str, allowed_hosts: &[String]) -> bool {
    allowed_hosts
        .iter()
        .any(|allowed_host| allowed_host.eq_ignore_ascii_case(host))
}

pub(crate) fn allowed_hosts_from_env() -> Vec<String> {
    parse_allowed_hosts(&std::env::var(MCP_ALLOWED_HOSTS_ENV).unwrap_or_default())
}

fn parse_allowed_hosts(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
        .collect()
}

fn clean_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{
        McpServerEntry, parse_allowed_hosts, server_entry_from_param, validate_request_server_url_with_allowed_hosts,
    };
    use crate::types::tools::McpToolParam;

    #[test]
    fn mcp_server_entry_deserializes_http_config() {
        let entry = serde_json::from_value::<McpServerEntry>(serde_json::json!({
            "url": "http://localhost:9000",
            "headers": {"Authorization": "Bearer token"},
            "allowed_tools": ["say_hello", "sum"],
            "require_approval": "never"
        }))
        .unwrap();

        match entry {
            McpServerEntry::Http {
                url,
                headers,
                allowed_tools,
                require_approval,
                ..
            } => {
                assert_eq!(url, "http://localhost:9000");
                assert_eq!(headers.unwrap()["Authorization"], "Bearer token");
                assert_eq!(allowed_tools.unwrap(), ["say_hello", "sum"]);
                assert_eq!(require_approval.as_deref(), Some("never"));
            }
            McpServerEntry::Stdio { .. } => panic!("expected HTTP MCP config"),
        }
    }

    #[test]
    fn mcp_server_entry_deserializes_stdio_config() {
        let entry = serde_json::from_value::<McpServerEntry>(serde_json::json!({
            "command": "python3",
            "args": ["/tmp/server.py"],
            "env": {"TOKEN": "secret"},
            "cwd": "/tmp"
        }))
        .unwrap();

        match entry {
            McpServerEntry::Stdio {
                command,
                args,
                env,
                cwd,
                allowed_tools,
                require_approval,
                ..
            } => {
                assert_eq!(command, "python3");
                assert_eq!(args, vec!["/tmp/server.py".to_owned()]);
                assert_eq!(env.unwrap()["TOKEN"], "secret");
                assert_eq!(cwd.as_deref(), Some("/tmp"));
                assert!(allowed_tools.is_none());
                assert!(require_approval.is_none());
            }
            McpServerEntry::Http { .. } => panic!("expected stdio MCP config"),
        }
    }

    #[test]
    fn request_server_url_allows_loopback_http() {
        let url = validate_request_server_url_with_allowed_hosts("http://127.0.0.1:8000/mcp", &[]).unwrap();
        assert_eq!(url, "http://127.0.0.1:8000/mcp");
    }

    #[test]
    fn request_server_url_allows_ipv6_loopback_http() {
        let url = validate_request_server_url_with_allowed_hosts("http://[::1]:8000/mcp", &[]).unwrap();
        assert_eq!(url, "http://[::1]:8000/mcp");
    }

    #[test]
    fn request_server_url_rejects_unallowlisted_host() {
        let error = validate_request_server_url_with_allowed_hosts("http://169.254.169.254/mcp", &[]).unwrap_err();
        assert!(error.contains("not allowed"));
    }

    #[test]
    fn request_server_url_uses_supplied_allowlist() {
        assert!(
            validate_request_server_url_with_allowed_hosts(
                "https://mcp.example.com/mcp",
                &["mcp.example.com".to_owned()]
            )
            .is_ok()
        );
        assert!(validate_request_server_url_with_allowed_hosts("https://mcp.example.com/mcp", &[]).is_err());
    }

    #[test]
    fn request_params_ignore_stdio_fields_without_configuring_transport() {
        let param = serde_json::from_value::<McpToolParam>(serde_json::json!({
            "server_label": "repo",
            "command": "python3",
            "args": ["/tmp/server.py"]
        }))
        .unwrap();

        assert!(server_entry_from_param(&param, &[]).is_none());
    }

    #[test]
    fn allowed_host_parser_trims_and_discards_empty_entries() {
        assert_eq!(
            parse_allowed_hosts(" Example.COM, ,api.test "),
            vec!["Example.COM", "api.test"]
        );
    }
}
