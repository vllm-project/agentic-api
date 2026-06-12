//! Tool dispatch decision logic.
//!
//! This module is STATELESS — it inspects the model's output items, decides
//! whether to execute tools, and returns a decision. It does not manage loop
//! state, persistence, or re-entry. That's `execute_loop`'s job.
//!
//! Decision flow:
//! ```text
//! output items → filter FunctionCall → empty? → Done
//!                                     → iteration >= max? → Incomplete
//!                                     → execute all → Continue(results)
//! ```

use crate::executor::error::ExecutorResult;
use crate::executor::tool_context::ToolContext;
use crate::types::io::{InputItem, OutputItem};

/// Decision returned by [`dispatch_tools`] to drive the agentic loop.
///
/// `#[non_exhaustive]` allows adding variants (e.g. `Partial` for mixed
/// gateway + client tools) without breaking downstream match arms.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoopDecision {
    /// Gateway-executed tools returned results. Caller should inject these
    /// `InputItem::FunctionCallOutput` items into the request and re-infer.
    Continue(Vec<InputItem>),

    /// No tool calls found in output, OR only client-side functions present.
    /// Caller should return the response to the client as-is.
    /// If `FunctionCall` items exist in output with this decision, the response
    /// status should be `requires_action` (client executes them externally).
    Done,

    /// Max iterations reached. The model wanted to call more tools but we're
    /// cutting it off to prevent runaway loops. The reason string is included
    /// for logging/debugging. Caller should set `payload.status = "incomplete"`.
    Incomplete(String),
}

/// Inspect executor output for function calls and dispatch them via [`ToolContext`].
///
/// # Decision Logic
///
/// 1. Filter `OutputItem::FunctionCall` items from `output`
/// 2. If none found → `Done` (model produced only text/messages)
/// 3. If `iteration >= tool_ctx.max_iterations` → `Incomplete` (safety cap)
/// 4. Otherwise → execute all calls via `tool_ctx.execute_all()` → `Continue`
///
/// # MVP Routing
///
/// Currently ALL `FunctionCall` items are treated as gateway-executable (routed
/// to MCP/`web_search`/`vector_store` in priority order). The distinction between
/// client-side functions (`type: "function"` in the request) and gateway-executed
/// tools (`type: "mcp"`) requires access to the request's tools array — deferred
/// to a follow-up PR that changes this function's signature.
///
/// # Error Semantics
///
/// - Individual tool failures → error JSON string in the result (model sees it)
/// - This function only returns `Err` on internal/structural failures
/// - The function itself NEVER panics
///
/// # Errors
///
/// Returns `ExecutorError` only on internal failures (e.g. serialization).
/// Individual tool execution failures are captured as error output strings
/// in the returned `InputItem` list — they do NOT propagate as errors.
pub async fn dispatch_tools(
    output: &[OutputItem],
    tool_ctx: &ToolContext,
    iteration: usize,
) -> ExecutorResult<LoopDecision> {
    // Step 1: Extract FunctionCall items. Messages and Unknown are ignored.
    let function_calls: Vec<_> = output
        .iter()
        .filter_map(|item| match item {
            OutputItem::FunctionCall(fc) => Some(fc),
            _ => None,
        })
        .collect();

    // Step 2: No tool calls → model is done generating.
    if function_calls.is_empty() {
        return Ok(LoopDecision::Done);
    }

    // Step 3: Safety cap — prevent infinite tool loops.
    // This fires BEFORE execution, so no work is wasted on the capped iteration.
    if iteration >= tool_ctx.max_iterations {
        return Ok(LoopDecision::Incomplete(format!(
            "max tool iterations reached ({iteration}/{})",
            tool_ctx.max_iterations
        )));
    }

    // Step 4: Execute all tool calls in parallel and return results.
    // Individual failures produce error strings (not Err), so this always succeeds.
    let results = tool_ctx.execute_all(&function_calls).await;
    Ok(LoopDecision::Continue(results))
}
