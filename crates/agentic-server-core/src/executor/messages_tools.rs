//! Gateway tool dispatch shared by the Messages loops.
//!
//! The JSON and SSE loops read a model turn differently, but both hand its
//! gateway-owned `tool_use` blocks to [`execute_gateway_calls`], so admission,
//! execution, and the fed-back `tool_result`s cannot differ between them.

use std::time::Duration;

use futures::future::join_all;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::executor::gateway::GatewaySchedulerPolicy;
use crate::executor::messages_context::MessagesRequestContext;
use crate::executor::messages_request::{
    web_fetch_budget_exhausted_result, web_fetch_refused_result, web_search_budget_exhausted_result,
};
use crate::executor::telemetry::FailureCategory;
use crate::executor::telemetry::metrics::{ExecutorMetrics, Stage};
use crate::tool::web_fetch::{WebFetchArguments, WebFetchErrorCode};
use crate::tool::web_search::args::requested_searches;
use crate::tool::{ToolOwnership, ToolRegistry, ToolType};
use crate::types::io::output::FunctionToolCall;
use crate::types::messages::tool_call::MessagesToolCall;
use crate::types::messages::{GatewayToolResult, tool_seam};

#[cfg(test)]
mod tests;

/// Per gateway-tool-call timeout — a hung tool becomes an error `tool_result`
/// fed back to the model, never a whole-request failure (edge E5). Matches the
/// Responses loop's `gateway::GATEWAY_TOOL_TIMEOUT`.
const GATEWAY_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// A call after admission: cleared to run, or already answered.
enum Admission {
    Run { call: FunctionToolCall, name: String },
    Refused(GatewayToolResult),
}

/// The ownership map for one request: the operator's aliases plus the gateway
/// tools the request-scoped registry resolved. `web_fetch` is gateway-owned
/// exactly when the registry bound it to the gateway executor, so the loop's
/// classification cannot drift from the registry.
pub(super) fn request_gateway_map(
    operator: &tool_seam::GatewayToolMap,
    registry: &ToolRegistry,
) -> tool_seam::GatewayToolMap {
    let web_fetch_owned = registry
        .lookup(tool_seam::WEB_FETCH_EXECUTOR)
        .is_some_and(|entry| entry.tool_type == ToolType::WebFetch && entry.ownership.is_gateway());
    operator.clone().with_web_fetch_owned(web_fetch_owned)
}

/// Execute one round's gateway calls under the shared scheduling policy. The
/// per-call timeout starts after permit acquisition. A failure or timeout
/// becomes an error `tool_result` (E5). Each call that runs is traced and
/// timed as a `tool` stage; a refused call is not.
///
/// Returns one `tool_result` block per call, in model order, fed back next
/// round. (The model's own `tool_use` block is carried forward via the preserved
/// assistant content, not reconstructed here — see
/// [`MessagesRequestContext::append_round`].)
pub(super) async fn execute_gateway_calls(
    tool_uses: Vec<MessagesToolCall>,
    ctx: &mut MessagesRequestContext,
    registry: &ToolRegistry,
    gateway_map: &tool_seam::GatewayToolMap,
    policy: &GatewaySchedulerPolicy,
    metrics: &ExecutorMetrics,
) -> Vec<GatewayToolResult> {
    // Admission is sequential and in model order, before any call starts: the
    // budgets belong to the whole request, so they cannot be decided while
    // calls run concurrently.
    let admissions: Vec<_> = tool_uses
        .into_iter()
        .map(|tool_use| admit(tool_use, ctx, gateway_map))
        .collect();
    let execution_slots = policy.new_round_execution_slots();
    join_all(
        admissions
            .into_iter()
            .map(|admission| run(admission, registry, policy, &execution_slots, metrics)),
    )
    .await
}

/// Decide whether one call may run. `max_uses` limits uses, not calls: a
/// `web_search` call is charged for every query it batches, a `web_fetch`
/// call for its one page, and each runs only when the remaining budget covers
/// it. A refused call, and a call whose arguments cannot be dispatched or
/// parsed, leave the budget untouched.
fn admit(
    tool_use: MessagesToolCall,
    ctx: &mut MessagesRequestContext,
    gateway_map: &tool_seam::GatewayToolMap,
) -> Admission {
    let MessagesToolCall { id, name, input } = tool_use;
    let input = match input {
        Ok(input) => input,
        // F4: never dispatch with arguments the model did not supply.
        Err(reason) => {
            return Admission::Refused(tool_seam::tool_result_block(
                &id,
                format!("{reason}; tool was not run"),
                true,
            ));
        }
    };
    let call = tool_seam::tool_use_to_call(&id, &name, &Value::Object(input), gateway_map);
    let admitted = match call.name.as_str() {
        tool_seam::WEB_SEARCH_EXECUTOR => {
            if ctx.admit_searches(requested_searches(&call.arguments)) {
                Ok(())
            } else {
                Err(web_search_budget_exhausted_result(&id))
            }
        }
        tool_seam::WEB_FETCH_EXECUTOR => admit_fetch(&id, &call.arguments, ctx),
        _ => Ok(()),
    };
    match admitted {
        Ok(()) => Admission::Run { call, name },
        Err(result) => Admission::Refused(result),
    }
}

/// A fetch is charged one use as soon as its arguments name a URL, whatever
/// happens next: a failed fetch counts, as Anthropic documents; arguments
/// without a URL fetch nothing and cost nothing (the handler answers them as
/// invalid input). The conversation rule is applied here because only the
/// loop holds the conversation — a URL that never appeared in a user message
/// or a tool result is refused before any network activity, and that refusal
/// counts.
fn admit_fetch(id: &str, arguments: &str, ctx: &mut MessagesRequestContext) -> Result<(), GatewayToolResult> {
    let args = WebFetchArguments::from_json(arguments).ok();
    if !ctx.admit_fetches(usize::from(args.is_some())) {
        return Err(web_fetch_budget_exhausted_result(id));
    }
    match args {
        Some(args) if !ctx.url_in_prior_context(&args.url) => Err(web_fetch_refused_result(
            id,
            WebFetchErrorCode::UrlNotInPriorContext,
            "the url did not appear earlier in the conversation; only a url from a user message or a tool result \
             can be fetched",
        )),
        _ => Ok(()),
    }
}

async fn run(
    admission: Admission,
    registry: &ToolRegistry,
    policy: &GatewaySchedulerPolicy,
    execution_slots: &Semaphore,
    metrics: &ExecutorMetrics,
) -> GatewayToolResult {
    let (call, name) = match admission {
        Admission::Run { call, name } => (call, name),
        // Answered without running: there is no tool stage to record.
        Admission::Refused(result) => return result,
    };
    // Typed by the registry entry that serves the call, never by the name
    // the model supplied.
    let tool_type = registry.tool_type(&call.name);
    let timer = metrics.stage(Stage::Tool(tool_type));
    let (output, is_error) = dispatch(&call, &name, registry, tool_type, policy, execution_slots).await;
    // An error result is a failed stage even though the loop continues.
    timer.finish(is_error.then_some(FailureCategory::Tool));
    tool_seam::tool_result_block(&call.call_id, output, is_error)
}

/// Run one admitted call under the scheduling policy; returns its output and
/// whether it is an error. A call without a handler acquires no permits.
#[tracing::instrument(name = "agentic.tool.execute", skip_all, fields(
    agentic.tool.r#type = super::telemetry::stages::tool_type(tool_type)
))]
async fn dispatch(
    call: &FunctionToolCall,
    name: &str,
    registry: &ToolRegistry,
    tool_type: ToolType,
    policy: &GatewaySchedulerPolicy,
    execution_slots: &Semaphore,
) -> (String, bool) {
    let binding = registry.lookup(&call.name).and_then(|entry| match &entry.ownership {
        ToolOwnership::Gateway(binding) => binding.as_ref(),
        ToolOwnership::Client => None,
    });
    let Some(binding) = binding else {
        return (format!("no handler for tool '{name}'"), true);
    };
    let _permits = policy.acquire_execution_permits(binding, execution_slots).await;
    match tokio::time::timeout(
        GATEWAY_TOOL_TIMEOUT,
        binding.execute(&call.call_id, &call.name, &call.arguments),
    )
    .await
    {
        // A documented failure is the tool's answer, carried with the
        // output's status so the model knows the call did not succeed.
        Ok(Ok(tool_output)) => {
            let failed = tool_output.is_failure();
            (tool_output.output, failed)
        }
        Ok(Err(e)) => (format!("tool execution failed: {e}"), true),
        Err(_) => (
            format!("gateway tool '{name}' timed out after {GATEWAY_TOOL_TIMEOUT:?}"),
            true,
        ),
    }
}
