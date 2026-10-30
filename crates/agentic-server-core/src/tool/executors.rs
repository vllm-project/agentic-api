use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use super::code_interpreter::CodeInterpreterHandler;
use super::declaration::ToolDeclaration;
use super::mcp::handler::McpServerToolSet;
use super::mcp::{McpClientPool, McpDiscoveredHandler, McpHandler};
use super::normalize::code_interpreter_unavailable_error;
use super::web_fetch::{WebFetchExecutor, WebFetchHandler};
use super::web_search::{WebSearchExecutor, WebSearchHandler};
use super::{GatewayExecutor, ToolError, ToolHandler};
use crate::config::{DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS, ToolRuntimeConfig, WebFetchConfig};
use crate::types::tools::{McpToolParam, ResponsesTool};

use super::code_interpreter::CodeInterpreterExecutor;

/// The error for a native `web_fetch` declaration on a gateway whose operator
/// disabled the executor.
const WEB_FETCH_UNAVAILABLE: &str = "web_fetch is disabled by operator configuration";

pub enum GatewayExecutorRegistration {
    WebSearch(Arc<WebSearchExecutor>),
    WebFetch(Arc<WebFetchExecutor>),
    Mcp {
        server_label: String,
        handlers: Vec<McpDiscoveredHandler>,
    },
}

impl<T> From<Arc<T>> for GatewayExecutorRegistration
where
    T: GatewayExecutor<
            ToolParams = crate::types::tools::WebSearchToolParam,
            ExecutionParams = crate::types::tools::WebSearchToolParam,
        >,
{
    fn from(executor: Arc<T>) -> Self {
        Self::WebSearch(executor)
    }
}

impl From<Arc<WebSearchExecutor>> for GatewayExecutorRegistration {
    fn from(executor: Arc<WebSearchExecutor>) -> Self {
        Self::WebSearch(executor)
    }
}

/// Shared, per-server registry of gateway-owned tool executors.
///
/// Built once at startup ([`GatewayExecutors::from_env`]) and reused across
/// every request. Configured and request-declared MCP servers are discovered
/// lazily unless handlers were registered with [`Self::insert`].
#[derive(Clone, Default)]
pub struct GatewayExecutors {
    mcp: HashMap<String, Vec<McpDiscoveredHandler>>,
    mcp_configs: HashMap<String, super::mcp::McpServerEntry>,
    mcp_clients: Arc<RwLock<HashMap<String, Arc<super::mcp::McpClient>>>>,
    mcp_discovered: Arc<RwLock<HashMap<String, Vec<McpDiscoveredHandler>>>>,
    mcp_allowed_hosts: Vec<String>,
    web_search: Option<Arc<WebSearchExecutor>>,
    /// Present unless the operator disabled the built-in `web_fetch` executor.
    web_fetch: Option<Arc<WebFetchExecutor>>,
    /// Present only after an opted-in provider passes startup readiness checks.
    code_interpreter: Option<Arc<CodeInterpreterExecutor>>,
}

impl GatewayExecutors {
    #[must_use]
    pub fn from_env(client: Arc<reqwest::Client>) -> Self {
        Self {
            mcp: HashMap::new(),
            mcp_configs: HashMap::new(),
            mcp_clients: Arc::new(RwLock::new(HashMap::new())),
            mcp_discovered: Arc::new(RwLock::new(HashMap::new())),
            mcp_allowed_hosts: super::mcp::pool::allowed_hosts_from_env(),
            web_search: Some(Arc::new(WebSearchHandler::from_env(client))),
            web_fetch: Some(Arc::new(WebFetchHandler::from_config(
                &WebFetchConfig::default(),
                DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS,
            ))),
            code_interpreter: None,
        }
    }

    /// Builds the shared executors without contacting configured MCP servers.
    ///
    /// Configured `allowed_tools` are applied during discovery, so the stored
    /// handler set is the maximum set that a request may use.
    ///
    /// # Errors
    ///
    /// Invalid policy configuration is returned as an error. Connection and
    /// discovery happen when a configured server is requested.
    pub fn from_config(client: Arc<reqwest::Client>, config: &ToolRuntimeConfig) -> Result<Self, ToolError> {
        config
            .code_interpreter
            .validate()
            .map_err(|error| ToolError::Config(error.to_string()))?;
        #[cfg(feature = "embedded-code-interpreter")]
        let code_interpreter = config
            .code_interpreter
            .enabled
            .then(|| CodeInterpreterExecutor::from_config(config.code_interpreter))
            .transpose()
            .map(|executor| executor.map(Arc::new))
            .map_err(|error| ToolError::Config(error.to_string()))?;
        #[cfg(not(feature = "embedded-code-interpreter"))]
        let code_interpreter = None;
        let executors = Self {
            mcp: HashMap::new(),
            mcp_configs: config.mcp_servers.clone(),
            mcp_clients: Arc::new(RwLock::new(HashMap::new())),
            mcp_discovered: Arc::new(RwLock::new(HashMap::new())),
            mcp_allowed_hosts: if config.mcp_allowed_hosts.is_empty() {
                super::mcp::pool::allowed_hosts_from_env()
            } else {
                config.mcp_allowed_hosts.clone()
            },
            web_search: Some(Arc::new(WebSearchHandler::from_config(
                client,
                &config.web_search,
                config.max_concurrent_gateway_calls,
            ))),
            web_fetch: config.web_fetch.enabled.then(|| {
                Arc::new(WebFetchHandler::from_config(
                    &config.web_fetch,
                    config.max_concurrent_gateway_calls,
                )) as Arc<WebFetchExecutor>
            }),
            code_interpreter,
        };
        if config.mcp_servers.is_empty() {
            return Ok(executors);
        }

        for (server_label, entry) in &config.mcp_servers {
            if entry.require_approval() != Some("never") {
                return Err(ToolError::Config(format!(
                    "configured MCP server '{server_label}' must set require_approval to 'never'"
                )));
            }
        }

        Ok(executors)
    }

    pub fn insert(&mut self, registration: impl Into<GatewayExecutorRegistration>) {
        match registration.into() {
            GatewayExecutorRegistration::WebSearch(executor) => self.web_search = Some(executor),
            GatewayExecutorRegistration::WebFetch(executor) => self.web_fetch = Some(executor),
            GatewayExecutorRegistration::Mcp { server_label, handlers } => {
                if handlers.is_empty() {
                    tracing::debug!(server_label, "empty MCP discovered handler registration skipped");
                    return;
                }
                if self.mcp.insert(server_label.clone(), handlers).is_some() {
                    tracing::debug!(server_label, "replaced MCP discovered handler registration");
                }
            }
        }
    }

    /// Always returns a real handler — falls back to [`WebSearchHandler::spec_only`]
    /// when no provider was configured, so callers never need to handle a
    /// missing gateway-owned `web_search` handler themselves.
    #[must_use]
    pub fn web_search_handler(&self) -> Arc<WebSearchExecutor> {
        self.web_search
            .clone()
            .unwrap_or_else(|| Arc::new(WebSearchHandler::spec_only()))
    }

    /// The `web_fetch` executor. The one owner of the availability policy: a
    /// gateway whose operator disabled the executor refuses a native
    /// declaration here, whether the caller is building a registry or counting
    /// tokens.
    ///
    /// # Errors
    ///
    /// [`ToolError::Config`] when the operator disabled the executor.
    pub fn require_web_fetch(&self) -> Result<Arc<WebFetchExecutor>, ToolError> {
        self.web_fetch
            .clone()
            .ok_or_else(|| ToolError::Config(WEB_FETCH_UNAVAILABLE.to_owned()))
    }

    #[must_use]
    pub(crate) fn request_scoped(&self) -> Self {
        self.clone()
    }

    /// Validates request declarations against the executors that actually
    /// completed startup readiness checks.
    ///
    /// This runs before request state may be persisted, and again after
    /// conversation settings are rehydrated, so an inherited declaration
    /// cannot bypass operator gating.
    pub(crate) fn validate_declarations(&self, tools: Option<&[ToolDeclaration]>) -> Result<(), ToolError> {
        let Some(tools) = tools else {
            return Ok(());
        };
        CodeInterpreterHandler::validate_declarations(tools)?;
        for (index, tool) in tools.iter().enumerate() {
            if tool.declares_unsupported_async() {
                return Err(ToolError::UnknownParameter {
                    param: format!("tools[{index}].async"),
                });
            }
            tool.validate()?;
        }
        if tools
            .iter()
            .any(|tool| matches!(tool, ToolDeclaration::CodeInterpreter(_)))
        {
            let ready = self.code_interpreter.is_some();
            if !ready {
                return Err(code_interpreter_unavailable_error());
            }
        }
        Ok(())
    }

    /// Return the shared, eagerly-ready code interpreter when an operator
    /// enabled a registered provider.
    #[must_use]
    pub(crate) fn code_interpreter_executor(&self) -> Option<Arc<CodeInterpreterExecutor>> {
        self.code_interpreter.clone()
    }

    /// Returns the discovered handlers for one request-declared MCP server.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an invalid declaration, and an
    /// execution error when the server cannot connect.
    pub async fn mcp_handler(&mut self, param: &McpToolParam) -> Result<Vec<McpDiscoveredHandler>, ToolError> {
        Ok(self.mcp_server_tools(param).await?.discovered_handlers)
    }

    /// Applies operator defaults to request declarations of configured MCP
    /// servers: `defer_loading` and the catalog `description` fill fields the
    /// request leaves unset.
    pub(crate) fn apply_configured_mcp_defaults(&self, tools: Option<&mut [ResponsesTool]>) {
        for tool in tools.into_iter().flatten() {
            let ResponsesTool::Mcp(param) = tool else {
                continue;
            };
            let Some(entry) = self.mcp_configs.get(param.server_label.trim()) else {
                continue;
            };
            if param.defer_loading.is_none() {
                param.defer_loading = entry.defer_loading();
            }
            if param.server_description.is_none() {
                param.server_description = entry.description().map(str::to_owned);
            }
        }
    }

    /// Returns the request-scoped tools and public discovery item for one MCP server.
    /// An `allowed_tools` filter that matches nothing yields an empty listing, as in the Responses API.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an invalid declaration, and an
    /// execution error when the server cannot connect.
    pub(crate) async fn mcp_server_tools(&mut self, param: &McpToolParam) -> Result<McpServerToolSet, ToolError> {
        let server_label = param.server_label.trim();
        if server_label.is_empty() {
            return Err(ToolError::Config(
                "MCP declaration requires a non-empty server_label".to_owned(),
            ));
        }
        let configured_handlers = self.mcp.get(server_label);
        let configured_server = self.mcp_configs.contains_key(server_label);
        // Request validation ran the declaration's own checks already; a caller that builds a registry directly
        // reaches discovery without it.
        McpHandler::spec_from_param(param).validate(param)?;
        validate_mcp_approval_source(param, configured_server || configured_handlers.is_some())?;
        if (configured_server || configured_handlers.is_some()) && param.server_url.is_some() {
            return Err(ToolError::Config(format!(
                "MCP server '{server_label}' is configured by the gateway; omit server_url from the request"
            )));
        }
        if let Some(configured_handlers) = configured_handlers {
            let discovered_handlers = filter_allowed_mcp_handlers(configured_handlers, param.allowed_tools.as_deref());
            return Ok(McpHandler::server_tool_set_from_handlers(
                server_label,
                discovered_handlers,
            ));
        }

        if configured_server {
            let Some(entry) = self.mcp_configs.get(server_label).cloned() else {
                return Err(ToolError::Config(format!(
                    "configured MCP server '{server_label}' is missing"
                )));
            };
            let cached_client = self.mcp_clients.read().await.get(server_label).cloned();
            let client = if let Some(client) = cached_client {
                client
            } else {
                let mut servers = HashMap::new();
                servers.insert(server_label.to_owned(), entry.clone());
                let pool = McpClientPool::from_config(servers).await;
                let Some(client) = pool.get(server_label).cloned() else {
                    return Err(ToolError::Execution(format!(
                        "configured MCP server '{server_label}' failed to connect: {}",
                        pool.connection_error(server_label)
                            .unwrap_or("unknown connection error")
                    )));
                };
                self.mcp_clients
                    .write()
                    .await
                    .insert(server_label.to_owned(), Arc::clone(&client));
                client
            };
            let discovered_handlers =
                if let Some(discovered_handlers) = self.mcp_discovered.read().await.get(server_label).cloned() {
                    discovered_handlers
                } else {
                    let tool_set = McpHandler::discover_tools(server_label, client, entry.allowed_tools()).await?;
                    let discovered_handlers = tool_set.discovered_handlers;
                    // An empty listing may be a server still starting up; list it again next time.
                    if !discovered_handlers.is_empty() {
                        self.mcp_discovered
                            .write()
                            .await
                            .insert(server_label.to_owned(), discovered_handlers.clone());
                    }
                    discovered_handlers
                };
            let discovered_handlers = filter_allowed_mcp_handlers(&discovered_handlers, param.allowed_tools.as_deref());
            return Ok(McpHandler::server_tool_set_from_handlers(
                server_label,
                discovered_handlers,
            ));
        }

        let pool =
            McpClientPool::from_params_with_allowed_hosts(std::slice::from_ref(param), &self.mcp_allowed_hosts).await;
        let Some(client) = pool.get(server_label).cloned() else {
            return Err(pool.connection_error(server_label).map_or_else(
                || {
                    ToolError::Config(format!(
                        "MCP server '{server_label}' has no valid request-declared configuration"
                    ))
                },
                |error| ToolError::Execution(format!("MCP server '{server_label}' failed to connect: {error}")),
            ));
        };
        let tool_set = McpHandler::discover_tools(server_label, client, param.allowed_tools.as_deref()).await?;
        let discovered_handlers = tool_set.discovered_handlers;
        self.mcp.insert(server_label.to_owned(), discovered_handlers.clone());
        Ok(McpHandler::server_tool_set_from_handlers(
            server_label,
            discovered_handlers,
        ))
    }
}

fn filter_allowed_mcp_handlers(
    handlers: &[McpDiscoveredHandler],
    allowed_tools: Option<&[String]>,
) -> Vec<McpDiscoveredHandler> {
    handlers
        .iter()
        .filter(|handler| {
            allowed_tools.is_none_or(|allowed| allowed.iter().any(|name| name == &handler.param.tool_name))
        })
        .cloned()
        .collect()
}

/// A server the gateway does not configure needs the request to set `require_approval`.
fn validate_mcp_approval_source(param: &McpToolParam, configured_server: bool) -> Result<(), ToolError> {
    if !configured_server && param.require_approval.is_none() {
        return Err(ToolError::Config(
            "MCP require_approval must be set to 'never' in gateway configuration or the request".to_owned(),
        ));
    }
    Ok(())
}

impl std::fmt::Debug for GatewayExecutors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("GatewayExecutors");
        debug
            .field("mcp_server_handlers", &self.mcp.len())
            .field("mcp_server_configs", &self.mcp_configs.len())
            .field("mcp_clients", &Arc::strong_count(&self.mcp_clients))
            .field("mcp_discovered", &Arc::strong_count(&self.mcp_discovered))
            .field("mcp_allowed_hosts", &self.mcp_allowed_hosts)
            .field("web_search", &self.web_search.is_some())
            .field("web_fetch", &self.web_fetch.is_some());
        debug.field("code_interpreter", &self.code_interpreter.is_some());
        debug.finish()
    }
}

#[cfg(test)]
mod tests;
