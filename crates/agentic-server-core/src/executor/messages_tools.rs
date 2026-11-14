//! Gateway tool dispatch shared by the Messages loops.
//!
//! The JSON and SSE loops read a model turn differently, but both hand its
//! gateway-owned `tool_use` blocks to [`execute_gateway_calls`], so admission,
//! execution, the fed-back `tool_result`s, and the result blocks a client sees
//! for its native server tool calls cannot differ between them.

use std::time::Duration;

use futures::future::join_all;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::executor::error::ExecutorResult;
use crate::executor::gateway::GatewaySchedulerPolicy;
use crate::executor::messages_context::MessagesRequestContext;
use crate::executor::messages_request::{
    web_fetch_budget_exhausted_result, web_fetch_refused_result, web_search_budget_exhausted_result,
};
use crate::executor::messages_server_tools::sealing::MessagesResultKey;
use crate::executor::messages_server_tools::{
    NativeOutcome, NativeServerTools, NativeTool, ProjectedResult, Refusal, ServerToolView, fetch_error_message,
    project_native_call,
};
use crate::executor::messages_usage::ServerToolUses;
use crate::executor::telemetry::FailureCategory;
use crate::executor::telemetry::metrics::{ExecutorMetrics, Stage};
use crate::tool::web_fetch::{WebFetchArguments, WebFetchErrorCode};
use crate::tool::web_search::args::{WebSearchArguments, requested_searches};
use crate::tool::{ToolOwnership, ToolRegistry, ToolType};
use crate::types::io::output::FunctionToolCall;
use crate::types::messages::tool_call::MessagesToolCall;
use crate::types::messages::{GatewayToolResult, ServerToolResultBlock, tool_seam};

#[cfg(test)]
mod tests;

/// Per gateway-tool-call timeout — a hung tool becomes an error `tool_result`
/// fed back to the model, never a whole-request failure (edge E5). Matches the
/// Responses loop's `gateway::GATEWAY_TOOL_TIMEOUT`.
const GATEWAY_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// One executed gateway call: the `tool_result` fed back to the model and, for
/// a native server tool call, the result block the client sees.
#[derive(Debug)]
pub(super) struct ExecutedCall {
    pub(super) tool_result: GatewayToolResult,
    pub(super) server_result: Option<ServerToolResultBlock>,
    pub(super) uses: ServerToolUses,
}

impl ExecutedCall {
    fn hidden(tool_result: GatewayToolResult) -> Self {
        Self {
            tool_result,
            server_result: None,
            uses: ServerToolUses::default(),
        }
    }
}

/// A native call as admission saw it: the tool and the input it carries.
struct NativeCall {
    tool: NativeTool,
    input: Value,
}

/// A call after admission: cleared to run, or already answered — a hidden call
/// by its `tool_result`, a native call by the reason it was refused.
enum Admission {
    Run {
        call: FunctionToolCall,
        name: String,
        native: Option<NativeCall>,
    },
    Answered(GatewayToolResult),
    Refused {
        id: String,
        native: NativeCall,
        refusal: Refusal,
    },
}

impl Admission {
    fn refused(id: String, native: Option<NativeCall>, refusal: Refusal, result: GatewayToolResult) -> Self {
        match native {
            Some(native) => Self::Refused { id, native, refusal },
            None => Self::Answered(result),
        }
    }
}

/// What dispatching an admitted call produced.
enum Dispatched {
    /// The executor's output and whether it reports a failure.
    Output { text: String, failed: bool },
    /// The call could not run; the message says why.
    Failed(String),
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
/// Returns one executed call per call, in model order: its `tool_result` is
/// fed back next round, and a native call's result block follows the round in
/// the client's view. (The model's own `tool_use` block is carried forward via
/// the preserved assistant content, not reconstructed here — see
/// [`MessagesRequestContext::append_round`].)
pub(super) async fn execute_gateway_calls(
    tool_uses: Vec<MessagesToolCall>,
    ctx: &mut MessagesRequestContext,
    registry: &ToolRegistry,
    gateway_map: &tool_seam::GatewayToolMap,
    view: ServerToolView<'_>,
    policy: &GatewaySchedulerPolicy,
    metrics: &ExecutorMetrics,
) -> Vec<ExecutedCall> {
    // Admission is sequential and in model order, before any call starts: the
    // budgets belong to the whole request, so they cannot be decided while
    // calls run concurrently.
    let admissions: Vec<_> = tool_uses
        .into_iter()
        .map(|tool_use| admit(tool_use, ctx, gateway_map, view.native))
        .collect();
    let execution_slots = policy.new_round_execution_slots();
    join_all(
        admissions
            .into_iter()
            .map(|admission| run(admission, registry, view.key, policy, &execution_slots, metrics)),
    )
    .await
}

/// Decide whether one call may run. `max_uses` limits uses, not calls: a
/// `web_search` call is charged for every query it batches, a `web_fetch`
/// call for its one page, and each runs only when the remaining budget covers
/// it. A refused call, and a call whose arguments cannot be dispatched or
/// parsed, leave the budget untouched. A native search whose arguments do not
/// parse is answered as invalid input without running.
fn admit(
    tool_use: MessagesToolCall,
    ctx: &mut MessagesRequestContext,
    gateway_map: &tool_seam::GatewayToolMap,
    native_tools: NativeServerTools,
) -> Admission {
    let MessagesToolCall { id, name, input } = tool_use;
    let native_tool = native_tools.tool(&name);
    let input = match input {
        Ok(input) => input,
        // F4: never dispatch with arguments the model did not supply.
        Err(reason) => {
            let native = native_tool.map(|tool| NativeCall {
                tool,
                input: Value::Null,
            });
            let result = tool_seam::tool_result_block(&id, format!("{reason}; tool was not run"), true);
            return Admission::refused(id, native, Refusal::InvalidInput, result);
        }
    };
    let input = Value::Object(input);
    let call = tool_seam::tool_use_to_call(&id, &name, &input, gateway_map);
    let native = match native_tool.map(|tool| NativeCall { tool, input }) {
        Some(native)
            if native.tool == NativeTool::WebSearch && WebSearchArguments::from_json(&call.arguments).is_err() =>
        {
            return Admission::Refused {
                id,
                native,
                refusal: Refusal::InvalidInput,
            };
        }
        native => native,
    };
    let admitted = match call.name.as_str() {
        tool_seam::WEB_SEARCH_EXECUTOR => {
            if ctx.admit_searches(requested_searches(&call.arguments)) {
                Ok(())
            } else {
                Err((Refusal::MaxUsesExceeded, web_search_budget_exhausted_result(&id)))
            }
        }
        tool_seam::WEB_FETCH_EXECUTOR => admit_fetch(&id, &call.arguments, ctx),
        _ => Ok(()),
    };
    match admitted {
        Ok(()) => Admission::Run { call, name, native },
        Err((refusal, result)) => Admission::refused(id, native, refusal, result),
    }
}

/// A fetch is charged one use as soon as its arguments name a URL, whatever
/// happens next: a failed fetch counts, as Anthropic documents; arguments
/// without a URL fetch nothing and cost nothing (the handler answers them as
/// invalid input). The conversation rule is applied here because only the
/// loop holds the conversation — a URL that never appeared in a user message
/// or a tool result is refused before any network activity, and that refusal
/// counts.
fn admit_fetch(
    id: &str,
    arguments: &str,
    ctx: &mut MessagesRequestContext,
) -> Result<(), (Refusal, GatewayToolResult)> {
    let args = WebFetchArguments::from_json(arguments).ok();
    if !ctx.admit_fetches(usize::from(args.is_some())) {
        return Err((Refusal::MaxUsesExceeded, web_fetch_budget_exhausted_result(id)));
    }
    match args {
        Some(args) if !ctx.url_in_prior_context(&args.url) => {
            let code = WebFetchErrorCode::UrlNotInPriorContext;
            Err((
                Refusal::UrlNotInPriorContext,
                web_fetch_refused_result(id, code, fetch_error_message(code)),
            ))
        }
        _ => Ok(()),
    }
}

async fn run(
    admission: Admission,
    registry: &ToolRegistry,
    key: &MessagesResultKey,
    policy: &GatewaySchedulerPolicy,
    execution_slots: &Semaphore,
    metrics: &ExecutorMetrics,
) -> ExecutedCall {
    let (call, name, native) = match admission {
        Admission::Run { call, name, native } => (call, name, native),
        // Answered without running: there is no tool stage to record.
        Admission::Answered(result) => return ExecutedCall::hidden(result),
        Admission::Refused {
            id,
            native: NativeCall { tool, input },
            refusal,
        } => {
            return shown(project_native_call(
                tool,
                &id,
                &input,
                NativeOutcome::Refused(refusal),
                key,
            ));
        }
    };
    // Typed by the registry entry that serves the call, never by the name
    // the model supplied.
    let tool_type = registry.tool_type(&call.name);
    let timer = metrics.stage(Stage::Tool(tool_type));
    let dispatched = dispatch(&call, &name, registry, tool_type, policy, execution_slots).await;
    // An error result is a failed stage even though the loop continues.
    let is_error = matches!(
        dispatched,
        Dispatched::Failed(_) | Dispatched::Output { failed: true, .. }
    );
    timer.finish(is_error.then_some(FailureCategory::Tool));
    match (native, dispatched) {
        (Some(NativeCall { tool, input }), dispatched) => {
            let outcome = match dispatched {
                Dispatched::Output { text, failed } => NativeOutcome::Output { text, failed },
                Dispatched::Failed(reason) => {
                    tracing::warn!(tool = name, %reason, "native server tool call failed");
                    NativeOutcome::Failed
                }
            };
            shown(project_native_call(tool, &call.call_id, &input, outcome, key))
        }
        (None, Dispatched::Output { text, failed }) => {
            ExecutedCall::hidden(tool_seam::tool_result_block(&call.call_id, text, failed))
        }
        (None, Dispatched::Failed(reason)) => {
            ExecutedCall::hidden(tool_seam::tool_result_block(&call.call_id, reason, true))
        }
    }
}

fn shown(projected: ProjectedResult) -> ExecutedCall {
    ExecutedCall {
        tool_result: projected.tool_result,
        server_result: Some(projected.block),
        uses: projected.uses,
    }
}

/// Run one admitted call under the scheduling policy. A call without a
/// handler acquires no permits.
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
) -> Dispatched {
    let binding = registry.lookup(&call.name).and_then(|entry| match &entry.ownership {
        ToolOwnership::Gateway(binding) => binding.as_ref(),
        ToolOwnership::Client => None,
    });
    let Some(binding) = binding else {
        return Dispatched::Failed(format!("no handler for tool '{name}'"));
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
            Dispatched::Output {
                text: tool_output.output,
                failed,
            }
        }
        Ok(Err(e)) => Dispatched::Failed(format!("tool execution failed: {e}")),
        Err(_) => Dispatched::Failed(format!(
            "gateway tool '{name}' timed out after {GATEWAY_TOOL_TIMEOUT:?}"
        )),
    }
}

/// Resume only connector calls validated by request preparation, before inference.
/// Both response modes use the same dispatch and history mutation.
pub(super) async fn resume_pending_mcp(
    ctx: &mut MessagesRequestContext,
    registry: &ToolRegistry,
    map: &tool_seam::GatewayToolMap,
    view: ServerToolView<'_>,
    policy: &GatewaySchedulerPolicy,
    metrics: &ExecutorMetrics,
) -> ExecutorResult<Vec<GatewayToolResult>> {
    let pending = ctx.take_pending_mcp();
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let calls = pending
        .into_iter()
        .map(|call| MessagesToolCall {
            id: call.id,
            name: call.name,
            input: Ok(call.input),
        })
        .collect();
    let executed = execute_gateway_calls(calls, ctx, registry, map, view, policy, metrics).await;
    let results = executed.into_iter().map(|call| call.tool_result).collect::<Vec<_>>();
    ctx.complete_pending_mcp(&results)?;
    Ok(results)
}
