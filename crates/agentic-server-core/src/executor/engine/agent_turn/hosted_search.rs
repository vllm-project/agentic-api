//! Hosted tool search inside a turn: the failed MCP listing that ends a response before it starts, and the tools a
//! round's searches loaded for the next round.

use super::AgentTurn;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway::GatewayCallResult;
use crate::executor::pipeline::AgentPipeline;
use crate::tool::{ToolError, ToolRegistry};
use crate::types::io::OutputItem;
use crate::types::tools::ResponsesTool;

/// The Responses API fails a response, before anything is emitted, when a declared MCP server cannot list its
/// tools. A mid-run refresh keeps the failed listing instead, since output may already be streaming.
pub(super) fn reject_failed_mcp_listing(registry: &ToolRegistry) -> ExecutorResult<()> {
    if let Some(failed) = registry.failed_mcp_list_tools() {
        let mut failed = failed.clone();
        failed.error = None;
        return Err(ExecutorError::McpListToolsFailed(Box::new(failed)));
    }
    Ok(())
}

/// With tool search active, the registry takes the request's catalog, which preparation built for this registry (a
/// rebuild follows a new preparation). It then checks availability and, for a hosted search, an MCP `tool_choice`
/// against the discovered tools.
pub(super) fn attach_tool_search(agent: &mut AgentPipeline, registry: &mut ToolRegistry) -> ExecutorResult<()> {
    if let Some(state) = agent.tool_search_state_mut().filter(|state| state.is_active()) {
        let synthetic = state.synthetic_tool_search().cloned();
        let hosted = state.is_hosted();
        let catalog = state
            .take_catalog()
            .ok_or_else(|| ToolError::Config("prepared tool-search catalog is missing for the registry".to_owned()))?;
        // A hosted search needs each deferred MCP server's discovered tools.
        let tools = agent.request.enriched_request.tools.as_deref().unwrap_or_default();
        registry.attach_tool_search(catalog, synthetic, hosted, tools);
        registry.validate_tool_search_availability()?;
        if hosted && let Some(catalog) = registry.tool_catalog() {
            catalog.validate_mcp_tool_choice(agent.request.enriched_request.tool_choice.as_ref(), tools)?;
        }
    }
    Ok(())
}

/// The tools hosted searches returned in this round, in output order.
pub(super) fn hosted_search_loads(results: &[GatewayCallResult]) -> Vec<ResponsesTool> {
    results
        .iter()
        .filter_map(|result| match &result.follow_up_output {
            Some(OutputItem::ToolSearchOutput(output)) => Some(output.tools.iter().cloned()),
            _ => None,
        })
        .flatten()
        .collect()
}

impl AgentTurn<'_> {
    /// Makes the tools this round's hosted searches loaded callable next round.
    pub(super) async fn load_hosted_tools(&mut self, loaded: &[ResponsesTool]) -> ExecutorResult<()> {
        if loaded.is_empty() {
            return Ok(());
        }
        let snapshot = self
            .registry
            .load_searched_tools(loaded, &mut self.pipeline.request.enriched_request)
            .await?;
        if let Some((effective_tools, loaded)) = snapshot {
            self.pipeline.record_tool_search_loads(effective_tools, loaded);
        }
        Ok(())
    }
}
