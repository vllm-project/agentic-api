// Test-only single-round composition of native tool execution and deferred delivery.
// Not a replacement engine, terminal policy, continuation loop or persistence path.

#![cfg(test)]

use crate::executor::error::ExecutorResult;
use crate::executor::gateway::{
    BuiltInToolCallBudget, GatewayScheduler, GatewaySchedulerPolicy, emit_gateway_start_events,
    execute_and_emit_output_calls, public_output_items, relay_round_events,
};
use crate::executor::relay::{Release, StreamRelay};
use crate::executor::request::RequestContext;
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::tool::ToolRegistry;
use crate::types::io::InputItem;
use crate::types::request_response::ResponsePayload;

#[derive(serde::Serialize)]
pub(in super::super) struct RoundOutcome {
    pub deferred_before_tools: bool,
    pub deferred_after_tools: bool,
    pub retained_bytes_after_ingestion: usize,
    pub retained_bytes_after_tools: usize,
    pub tool_outputs: Vec<InputItem>,
}

pub(super) async fn complete(
    payload: &mut ResponsePayload,
    registry: &ToolRegistry,
    context: &RequestContext,
    relay: &mut StreamRelay,
    budget: &ExecutorResponseBudget,
) -> ExecutorResult<RoundOutcome> {
    let deferred_before_tools = relay.has_deferred();
    let retained_bytes_after_ingestion = budget.used();
    // Mirror the native engine's failed-upstream policy: release, never execute.
    let tool_outputs = if matches!(payload.status.as_str(), "error" | "failed") {
        relay.release_deferred(Release::All, context).await?;
        Vec::new()
    } else {
        let mut call_budget = BuiltInToolCallBudget::new(None);
        let policy = GatewaySchedulerPolicy::default();
        let results = if deferred_before_tools {
            let mut scheduler =
                GatewayScheduler::plan_with_budget(&payload.output, registry, 0, policy, &mut call_budget);
            let initial = scheduler.initial_event_run_len(&payload.output, registry);
            emit_gateway_start_events(scheduler.event_plans().take(initial), relay).await?;
            let results = scheduler.execute_with_budget(budget).await?;
            relay_round_events(&scheduler, &results, payload.output.len(), relay, context, initial).await?;
            results
        } else {
            execute_and_emit_output_calls(&payload.output, registry, 0, policy, budget, &mut call_budget, relay).await?
        };
        payload.output = public_output_items(&payload.output, registry, &results)?;
        results.into_iter().map(|result| result.input_item).collect()
    };
    Ok(RoundOutcome {
        deferred_before_tools,
        deferred_after_tools: relay.has_deferred(),
        retained_bytes_after_ingestion,
        retained_bytes_after_tools: budget.used(),
        tool_outputs,
    })
}
