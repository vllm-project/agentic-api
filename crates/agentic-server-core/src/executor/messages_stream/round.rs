//! Completed round handling for Messages streams. Ingestion owns the buffered
//! blocks, the runner owns client delivery, and this stage decides and applies
//! the round's history, persistence, or tool execution.

use serde_json::Value;

use super::ingestion::Frame;
use super::{BufferedBlock, MessagesStreamAccumulator};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::messages_connector::mixed_mcp_continuation;
use crate::executor::messages_context::MessagesRequestContext;
use crate::executor::messages_round::{MessagesRoundDecision, decide_round};
use crate::executor::messages_server_tools::ServerToolView;
use crate::executor::messages_threads::ThreadRetention;
use crate::executor::messages_tools::execute_gateway_calls;
use crate::executor::request::ExecutionContext;
use crate::tool::ToolRegistry;
use crate::types::messages::tool_call::MessagesToolCall;
use crate::types::messages::tool_seam;

/// Publish only after ingestion has established a completed round, before the
/// runner releases the public terminal. Assembly remains owned by ingestion.
pub(super) async fn complete_round(
    ctx: &mut MessagesRequestContext,
    acc: &mut MessagesStreamAccumulator,
    decision: MessagesRoundDecision,
) -> ExecutorResult<()> {
    if ctx.thread_message_id().is_some() {
        if !acc.has_completed_round() {
            return Err(ExecutorError::StreamError(
                "upstream Messages stream ended before message_stop".to_owned(),
            ));
        }
        let stop_reason = decision
            .visible_stop_reason(acc.stop_reason())
            .filter(|_| !acc.fallback);
        let has_tool_call = acc.blocks.values().any(|block| block.block["type"] == "tool_use");
        let retention = ThreadRetention::for_response(stop_reason, has_tool_call);
        let content = acc.terminal_content()?;
        ctx.complete_thread(&content, retention).await?;
    }
    Ok(())
}

/// Execute a completed round's gateway calls and append the round for the next
/// inference; returns the result blocks the client is shown.
pub(super) async fn execute_round(
    acc: &mut MessagesStreamAccumulator,
    ctx: &mut MessagesRequestContext,
    registry: &ToolRegistry,
    exec_ctx: &ExecutionContext,
    view: ServerToolView<'_>,
) -> ExecutorResult<Vec<Frame>> {
    // Reconstruct the FULL assistant turn (thinking/text/signature + gateway
    // tool_use, in order) for the next round's history — not just the gateway
    // tool_use (F3, streaming half). The gateway calls are derived from the
    // same buffered blocks for dispatch.
    let (assistant_content, calls) = acc.take_round();
    let executed = execute_gateway_calls(
        calls,
        ctx,
        registry,
        &acc.gateway_map,
        view,
        &exec_ctx.gateway_scheduler_policy,
        &exec_ctx.metrics,
    )
    .await;
    let (results, tool_results) = acc.record_executed(executed, &assistant_content);
    ctx.append_round(&assistant_content, tool_results)?;
    Ok(results)
}

impl MessagesStreamAccumulator {
    /// Retain the terminal round and any mixed MCP calls awaiting client results.
    /// Executed rounds already belong to the context's model-facing conversation.
    pub(super) fn terminal_content(&mut self) -> ExecutorResult<Value> {
        let include_pending = mixed_mcp_continuation(self.has_client_tool_use, self.stop_reason());
        let blocks = self
            .blocks
            .values_mut()
            .filter(|block| include_pending || !block.is_gateway_tool)
            .map(BufferedBlock::completed_block)
            .collect::<Vec<_>>();
        let blocks = if include_pending {
            tool_seam::project_pending_mcp(&blocks, &self.gateway_map)?
        } else {
            blocks
        };
        Ok(Value::Array(blocks))
    }

    /// Consume this round's buffered blocks, returning (full assistant content in
    /// order, gateway calls to dispatch). The assistant content preserves
    /// `thinking`/`text`/`signature` and the gateway `tool_use` blocks (F3); the
    /// calls are the gateway `tool_use` blocks reconstructed for dispatch.
    pub(super) fn take_round(&mut self) -> (Vec<Value>, Vec<MessagesToolCall>) {
        // This round's terminal is suppressed; its usage joins the final one's.
        self.usage.commit();
        let blocks = std::mem::take(&mut self.blocks);
        let mut assistant_content = Vec::with_capacity(blocks.len());
        let mut calls = Vec::new();
        for buffered in blocks.into_values() {
            let (block, call) = buffered.into_parts();
            assistant_content.push(block);
            if let Some(call) = call {
                calls.push(call);
            }
        }
        (assistant_content, calls)
    }

    /// Only ingestion's completed round may ask the shared execution policy.
    pub(super) fn round_decision(&self, ctx: &MessagesRequestContext) -> MessagesRoundDecision {
        if !self.has_completed_round() {
            return MessagesRoundDecision::Finish;
        }
        decide_round(
            ctx,
            self.stop_reason(),
            self.has_client_tool_use,
            self.blocks
                .values()
                .filter(|block| block.is_gateway_tool)
                .filter_map(|block| block.block["name"].as_str()),
        )
    }
}
