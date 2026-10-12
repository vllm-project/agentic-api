//! Gateway tool dispatch shared by the Messages loops.
//!
//! The JSON and SSE loops read a model turn differently, but both hand its
//! gateway-owned `tool_use` blocks to [`execute_gateway_calls`], so admission,
//! execution, and the fed-back `tool_result`s cannot differ between them.

use std::time::Duration;

use futures::future::join_all;
use serde_json::Value;

use crate::executor::messages_context::MessagesRequestContext;
use crate::executor::messages_request::{
    web_fetch_budget_exhausted_result, web_fetch_refused_result, web_search_budget_exhausted_result,
};
use crate::tool::web_fetch::{WebFetchArguments, WebFetchErrorCode};
use crate::tool::web_search::args::requested_searches;
use crate::tool::{ToolRegistry, ToolType};
use crate::types::io::output::FunctionToolCall;
use crate::types::messages::{GatewayToolResult, tool_seam};

/// Per gateway-tool-call timeout — a hung tool becomes an error `tool_result`
/// fed back to the model, never a whole-request failure (edge E5). Matches the
/// Responses loop's `gateway::GATEWAY_TOOL_TIMEOUT`.
const GATEWAY_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// One gateway-owned `tool_use` block of a model turn.
pub(super) struct GatewayToolUse<'a> {
    pub(super) id: &'a str,
    pub(super) name: &'a str,
    /// The call's JSON-object arguments, or why they cannot be dispatched.
    pub(super) input: Result<Value, String>,
}

/// A call after admission: cleared to run, or already answered.
enum Admission<'a> {
    Run { call: FunctionToolCall, name: &'a str },
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

/// Execute one round's gateway calls concurrently, each bounded by the per-call
/// timeout. A failure or timeout becomes an error `tool_result` (E5).
///
/// Returns one `tool_result` block per call, in model order, fed back next
/// round. (The model's own `tool_use` block is carried forward via the preserved
/// assistant content, not reconstructed here — see
/// [`MessagesRequestContext::append_round`].)
pub(super) async fn execute_gateway_calls(
    tool_uses: Vec<GatewayToolUse<'_>>,
    ctx: &mut MessagesRequestContext,
    registry: &ToolRegistry,
    gateway_map: &tool_seam::GatewayToolMap,
) -> Vec<GatewayToolResult> {
    // Admission is sequential and in model order, before any call starts: the
    // budgets belong to the whole request, so they cannot be decided while
    // calls run concurrently.
    let admissions: Vec<_> = tool_uses
        .into_iter()
        .map(|tool_use| admit(tool_use, ctx, gateway_map))
        .collect();
    join_all(admissions.into_iter().map(|admission| run(admission, registry))).await
}

/// Decide whether one call may run. `max_uses` limits uses, not calls: a
/// `web_search` call is charged for every query it batches, a `web_fetch`
/// call for its one page, and each runs only when the remaining budget covers
/// it. A refused call, and a call whose arguments cannot be dispatched or
/// parsed, leave the budget untouched.
fn admit<'a>(
    tool_use: GatewayToolUse<'a>,
    ctx: &mut MessagesRequestContext,
    gateway_map: &tool_seam::GatewayToolMap,
) -> Admission<'a> {
    let GatewayToolUse { id, name, input } = tool_use;
    let input = match input {
        Ok(input) => input,
        // F4: never dispatch with arguments the model did not supply.
        Err(reason) => {
            return Admission::Refused(tool_seam::tool_result_block(
                id,
                format!("{reason}; tool was not run"),
                true,
            ));
        }
    };
    let call = tool_seam::tool_use_to_call(id, name, &input, gateway_map);
    let admitted = match call.name.as_str() {
        tool_seam::WEB_SEARCH_EXECUTOR => {
            if ctx.admit_searches(requested_searches(&call.arguments)) {
                Ok(())
            } else {
                Err(web_search_budget_exhausted_result(id))
            }
        }
        tool_seam::WEB_FETCH_EXECUTOR => admit_fetch(id, &call.arguments, ctx),
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

async fn run(admission: Admission<'_>, registry: &ToolRegistry) -> GatewayToolResult {
    let (call, name) = match admission {
        Admission::Run { call, name } => (call, name),
        Admission::Refused(result) => return result,
    };
    let (output, is_error) = match tokio::time::timeout(GATEWAY_TOOL_TIMEOUT, registry.dispatch(&call)).await {
        Ok(Some(result)) => match result.output {
            // A documented failure is the tool's answer, carried with the
            // output's status so the model knows the call did not succeed.
            Ok(tool_output) => {
                let failed = tool_output.is_failure();
                (tool_output.output, failed)
            }
            Err(e) => (format!("tool execution failed: {e}"), true),
        },
        Ok(None) => (format!("no handler for tool '{name}'"), true),
        Err(_) => (
            format!("gateway tool '{name}' timed out after {GATEWAY_TOOL_TIMEOUT:?}"),
            true,
        ),
    };
    tool_seam::tool_result_block(&call.call_id, output, is_error)
}

/// Resume only connector calls validated by request preparation, before inference.
/// Both response modes use the same dispatch and history mutation.
pub(super) async fn resume_pending_mcp(
    ctx: &mut MessagesRequestContext,
    registry: &ToolRegistry,
    map: &tool_seam::GatewayToolMap,
) -> crate::executor::error::ExecutorResult<Vec<GatewayToolResult>> {
    let mut pending = ctx.take_pending_mcp();
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let calls = pending
        .iter_mut()
        .map(|call| GatewayToolUse {
            id: &call.id,
            name: &call.name,
            input: Ok(Value::Object(std::mem::take(&mut call.input))),
        })
        .collect();
    let results = execute_gateway_calls(calls, ctx, registry, map).await;
    ctx.complete_pending_mcp(&results)?;
    Ok(results)
}
