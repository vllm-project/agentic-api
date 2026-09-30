//! Gateway tool dispatch shared by the Messages loops.
//!
//! The JSON and SSE loops read a model turn differently, but both hand its
//! gateway-owned `tool_use` blocks to [`execute_gateway_calls`], so admission,
//! execution, and the fed-back `tool_result`s cannot differ between them.

use std::time::Duration;

use futures::future::join_all;
use serde_json::Value;

use crate::executor::messages_context::MessagesRequestContext;
use crate::executor::messages_request::web_search_budget_exhausted_result;
use crate::tool::ToolRegistry;
use crate::tool::web_search::args::requested_searches;
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
    // search budget belongs to the whole request, so it cannot be decided while
    // calls run concurrently.
    let admissions: Vec<_> = tool_uses
        .into_iter()
        .map(|tool_use| admit(tool_use, ctx, gateway_map))
        .collect();
    join_all(admissions.into_iter().map(|admission| run(admission, registry))).await
}

/// Decide whether one call may run. `max_uses` limits searches, not calls, so a
/// call is charged for every query it batches and runs only when the remaining
/// budget covers all of them. A refused call, and a call whose arguments cannot
/// be dispatched or parsed, leave the budget untouched.
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
    let searches = if call.name == tool_seam::WEB_SEARCH_EXECUTOR {
        requested_searches(&call.arguments)
    } else {
        0
    };
    if ctx.admit_searches(searches) {
        Admission::Run { call, name }
    } else {
        Admission::Refused(web_search_budget_exhausted_result(id))
    }
}

async fn run(admission: Admission<'_>, registry: &ToolRegistry) -> GatewayToolResult {
    let (call, name) = match admission {
        Admission::Run { call, name } => (call, name),
        Admission::Refused(result) => return result,
    };
    let (output, is_error) = match tokio::time::timeout(GATEWAY_TOOL_TIMEOUT, registry.dispatch(&call)).await {
        Ok(Some(result)) => match result.output {
            Ok(tool_output) => (tool_output.output, false),
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
