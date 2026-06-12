//! Agentic loop orchestrator.
//!
//! Composes `execute()` (LLM inference) with `dispatch_tools()` (tool routing)
//! in a loop that continues until the model stops producing tool calls.
//!
//! Architecture:
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │ execute_loop                                             │
//! │                                                         │
//! │  for each iteration:                                    │
//! │    1. execute(request) → ResponsePayload                │
//! │    2. dispatch_tools(output) → LoopDecision             │
//! │    3. if Continue: inject results, goto 1               │
//! │       if Done/Incomplete: return payload                │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! Timeout budget per iteration:
//! - LLM inference: `exec_ctx.streaming_timeout` (default 30s)
//! - Each tool call: `tool_ctx.tool_timeout` (default 30s)
//! - Hard loop cap: `MAX_LOOP_GUARD` (128 iterations)
//! - Soft tool cap: `tool_ctx.max_iterations` (default 10)

use std::sync::Arc;

use either::Either;
use tracing::debug;

use crate::executor::dispatch::{LoopDecision, dispatch_tools};
use crate::executor::engine::execute;
use crate::executor::error::ExecutorResult;
use crate::executor::request::ExecutionContext;
use crate::executor::tool_context::ToolContext;
use crate::types::io::ResponsesInput;
use crate::types::request_response::{RequestPayload, ResponsePayload};

/// Defense-in-depth hard cap, independent of `tool_ctx.max_iterations`.
/// Prevents infinite loops even if dispatch logic has a bug.
/// Set high enough to never trigger in normal operation (`max_iterations`=10
/// would stop far earlier), but low enough to catch runaway loops quickly.
const MAX_LOOP_GUARD: usize = 128;

/// Run the agentic loop: execute → dispatch tools → re-enter until done.
///
/// # Contract
///
/// - **Caller provides:** request, execution context (LLM + DB), tool context (providers)
/// - **This function returns:** the final `ResponsePayload` (caller persists it)
/// - **Persistence:** NOT done here. Caller (server handler) owns persistence because
///   it has the full `RequestContext` with correct `new_input_items`. We set
///   `request.store = false` for intermediate iterations to suppress the internal
///   `execute()` from persisting partial state.
/// - **Response chain:** `previous_response_id` on the returned payload reflects the
///   ORIGINAL caller-supplied value, not the internal mutations.
///
/// # Timeouts
///
/// - Each `execute()` call is wrapped in `tokio::time::timeout(exec_ctx.streaming_timeout)`
/// - Each tool call is wrapped in `tokio::time::timeout(tool_ctx.tool_timeout)`
/// - `Duration::ZERO` on either disables that timeout (provider manages its own)
///
/// # Known Limitations (MVP)
///
/// - Non-streaming only. `stream=true` returns an immediate error.
/// - `request.clone()` every iteration is O(n) in accumulated input size.
/// - `InputItem` lacks a `FunctionCall` variant, so the assistant's tool-call
///   items are not injected into context (the model doesn't see its own calls).
///   Follow-up PR needed to add the variant.
///
/// # Errors
///
/// Returns `ExecutorError` if:
/// - LLM inference fails or times out
/// - Tool dispatch encounters a fatal error (individual tool failures are NOT fatal)
/// - `stream=true` is passed
/// - Hard loop guard is breached
pub async fn execute_loop(
    mut request: RequestPayload,
    exec_ctx: Arc<ExecutionContext>,
    tool_ctx: &ToolContext,
) -> ExecutorResult<ResponsePayload> {
    // Capture before the loop mutates the request. Restored on final payload
    // so the response chain (previous_response_id linkage) remains correct
    // for the caller's persistence logic.
    let original_previous_response_id = request.previous_response_id.clone();

    // Suppress persistence for ALL iterations inside the loop. The caller
    // (server handler) owns final persistence with the correct RequestContext.
    // Without this, the first iteration would persist a partial response
    // (containing only the tool-call output, not the final answer) to the DB.
    request.store = false;

    for iteration in 0_usize.. {
        // Defense-in-depth: even if dispatch_tools has a bug that never returns
        // Incomplete, we won't loop forever.
        if iteration >= MAX_LOOP_GUARD {
            return Err(crate::executor::ExecutorError::InvalidRequest(format!(
                "execute_loop exceeded hard iteration cap ({MAX_LOOP_GUARD})"
            )));
        }

        debug!(iteration, "execute_loop iteration");

        // --- Step 1: Call the LLM ---
        // Timeout prevents hanging on unresponsive LLM backends.
        // Duration::ZERO = no timeout (provider/reqwest manages its own).
        let inference_timeout = exec_ctx.streaming_timeout;
        let result = if inference_timeout.is_zero() {
            execute(request.clone(), Arc::clone(&exec_ctx)).await?
        } else {
            tokio::time::timeout(inference_timeout, execute(request.clone(), Arc::clone(&exec_ctx)))
                .await
                .map_err(|_| {
                    crate::executor::ExecutorError::StreamError(format!(
                        "LLM inference timed out after {inference_timeout:?} on iteration {iteration}"
                    ))
                })??
        };

        // execute() returns Either<ResponsePayload, BoxStream>.
        // We only support non-streaming in execute_loop (streaming requires StreamTee).
        let mut payload = match result {
            Either::Left(payload) => payload,
            Either::Right(_stream) => {
                return Err(crate::executor::ExecutorError::InvalidRequest(
                    "execute_loop does not support streaming yet — set stream=false".into(),
                ));
            }
        };

        // --- Step 2: Inspect output for tool calls ---
        // dispatch_tools filters FunctionCall items, executes them via ToolContext,
        // and returns a decision: Continue (with results), Done, or Incomplete.
        let decision = dispatch_tools(&payload.output, tool_ctx, iteration).await?;

        match decision {
            // No tool calls (or only client-side functions) — we're done.
            LoopDecision::Done => {
                payload.previous_response_id = original_previous_response_id;
                return Ok(payload);
            }
            // Hit max_iterations — stop looping, mark as incomplete.
            // The model may have wanted to call more tools, but we're cutting it off.
            // Attach the reason to incomplete_details so the client knows why.
            LoopDecision::Incomplete(reason) => {
                debug!(iteration, %reason, "loop incomplete");
                payload.status = "incomplete".to_string();
                payload.incomplete_details =
                    Some(crate::types::request_response::IncompleteDetails { reason: Some(reason) });
                payload.previous_response_id = original_previous_response_id;
                return Ok(payload);
            }
            // Tools were executed — inject results and re-enter inference.
            LoopDecision::Continue(tool_results) => {
                debug!(
                    iteration,
                    results = tool_results.len(),
                    "tool results received, re-entering"
                );

                // Build the input for the next iteration:
                // existing context + tool results appended.
                let mut items = match &request.input {
                    // Already structured items — clone and extend.
                    ResponsesInput::Items(existing) => existing.clone(),
                    // First iteration with plain text — convert to structured item.
                    ResponsesInput::Text(t) => {
                        vec![crate::types::io::InputItem::Message(crate::types::io::InputMessage {
                            role: "user".into(),
                            content: crate::types::io::InputMessageContent::Text(t.clone()),
                        })]
                    }
                };

                // TODO: Append assistant's FunctionCall output items to context.
                // The Responses API wire format includes `type: "function_call"` in
                // input, but InputItem doesn't have that variant yet. Adding it
                // requires a type system change (follow-up PR). For now the model
                // only sees function_call_output results, not its own tool requests.
                // This may cause the model to re-call tools or behave unexpectedly
                // in complex multi-hop scenarios.

                // Append tool execution results (function_call_output items).
                items.extend(tool_results);
                request.input = ResponsesInput::Items(items);

                // Clear previous_response_id — on re-entry, we don't want execute()
                // to rehydrate from DB (we're managing context in-memory via items).
                request.previous_response_id = None;
                // request.store already set to false before the loop — no need
                // to re-set here.
            }
        }
    }

    unreachable!("loop exits via return in Done/Incomplete/guard arms")
}
