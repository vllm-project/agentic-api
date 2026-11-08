//! Request-scoped registry construction and connector admission.

use super::MessagesRequestContext;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::messages_connector::{normalize_connector, pending_mcp_calls, validate_connector_discovery};
use crate::executor::messages_tools::request_gateway_map;
use crate::tool::mcp::messages::connector_tools;
use crate::tool::{GatewayExecutors, ToolRegistry, registry_tools};
use crate::types::messages::{GatewayToolMap, ToolParam};

impl MessagesRequestContext {
    /// Discover connector tools through the shared typed registry and normalize the upstream view.
    ///
    /// # Errors
    /// Returns validation, discovery-budget, or serialization errors before inference begins.
    pub async fn prepare_registry(
        &mut self,
        map: &GatewayToolMap,
        executors: &mut GatewayExecutors,
    ) -> ExecutorResult<ToolRegistry> {
        let mut tools = registry_tools(self.tools(), map);
        // A thread can carry upstream-hosted MCP configuration independently
        // of the gateway connector. Only a local toolset opts that thread in.
        if self.thread.is_some()
            && !self
                .tools()
                .is_some_and(|tools| tools.iter().any(|tool| matches!(tool, ToolParam::McpToolset(_))))
        {
            return Ok(ToolRegistry::build_with_handlers(&mut tools, executors).await?);
        }
        tools.extend(connector_tools(
            self.typed.mcp_servers.as_deref().unwrap_or_default(),
            self.typed.tools.as_deref().unwrap_or_default(),
        )?);
        let registry = ToolRegistry::build_with_handlers(&mut tools, executors).await?;
        validate_connector_discovery(&registry)?;
        let mut pending = pending_mcp_calls(&self.raw, &request_gateway_map(map, &registry))?;
        let mut gateway_tools = map.clone();
        normalize_connector(&mut self.raw, &tools, &mut gateway_tools)?;
        for call in &mut pending {
            let name = gateway_tools
                .mcp_internal_name(&call.server_name, &call.name)
                .filter(|name| registry.lookup(name).is_some_and(|entry| entry.ownership.is_gateway()))
                .ok_or_else(|| {
                    ExecutorError::InvalidRequest(
                        "pending MCP tool must remain enabled in the continuation request".to_owned(),
                    )
                })?;
            call.name = name.to_owned();
        }
        self.pending_mcp = pending;
        self.gateway_tools = Some(gateway_tools);
        Ok(registry)
    }

    #[must_use]
    pub fn gateway_tools_or<'a>(&'a self, fallback: &'a GatewayToolMap) -> &'a GatewayToolMap {
        self.gateway_tools.as_ref().unwrap_or(fallback)
    }
}
