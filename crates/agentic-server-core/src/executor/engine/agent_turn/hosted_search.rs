//! Hosted tool search inside a turn: the failed MCP listing that ends a response before it starts, and the tools a
//! round's searches loaded for the next round.

use super::AgentTurn;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway::GatewayCallResult;
use crate::tool::ToolRegistry;
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
        self.pipeline.apply_hosted_search(loaded)?;
        let tools = self
            .pipeline
            .request
            .enriched_request
            .tools
            .as_deref()
            .unwrap_or_default();
        self.registry.refresh_client_tools(tools).await?;
        Ok(())
    }
}
