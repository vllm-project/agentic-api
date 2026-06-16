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

use crate::executor::ExecutorError;
use crate::executor::dispatch::{LoopDecision, dispatch_tools};
use crate::executor::engine::execute;
use crate::executor::error::ExecutorResult;
use crate::executor::request::ExecutionContext;
use crate::executor::tool_context::ToolContext;
use crate::types::io::{InputItem, InputMessage, InputMessageContent, ResponsesInput};
use crate::types::request_response::{IncompleteDetails, RequestPayload, ResponsePayload};

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
///   it has the full `RequestContext` with correct `new_input_items`. We clear
///   all three persistence triggers (`store`, `previous_response_id`,
///   `conversation_id`) to suppress intermediate `execute()` calls from
///   persisting partial state (PR #56 persists when ANY of the three is set).
/// - **ID restoration:** Both `previous_response_id` and `conversation_id` on the
///   returned payload reflect the ORIGINAL caller-supplied values, not the
///   internal mutations. This is critical for the caller's persist step.
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
    let original_previous_response_id = request.previous_response_id.clone();
    let original_conversation_id = request.conversation_id.clone();

    // Clear all three persistence triggers so intermediate execute() calls
    // don't write partial tool-call-only responses to the store.
    request.store = false;
    request.previous_response_id = None;
    request.conversation_id = None;

    for iteration in 0_usize.. {
        // Defense-in-depth: even if dispatch_tools has a bug that never returns
        // Incomplete, we won't loop forever.
        if iteration >= MAX_LOOP_GUARD {
            return Err(ExecutorError::InvalidRequest(format!(
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
                    ExecutorError::StreamError(format!(
                        "LLM inference timed out after {inference_timeout:?} on iteration {iteration}"
                    ))
                })??
        };

        // execute() returns Either<ResponsePayload, BoxStream>.
        // We only support non-streaming in execute_loop (streaming requires StreamTee).
        let mut payload = match result {
            Either::Left(payload) => payload,
            Either::Right(_stream) => {
                return Err(ExecutorError::InvalidRequest(
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
                payload.conversation_id = original_conversation_id;
                return Ok(payload);
            }
            // Hit max_iterations — stop looping, mark as incomplete.
            // The model may have wanted to call more tools, but we're cutting it off.
            // Attach the reason to incomplete_details so the client knows why.
            LoopDecision::Incomplete(reason) => {
                debug!(iteration, %reason, "loop incomplete");
                payload.status = "incomplete".to_string();
                payload.incomplete_details = Some(IncompleteDetails { reason: Some(reason) });
                payload.previous_response_id = original_previous_response_id;
                payload.conversation_id = original_conversation_id;
                return Ok(payload);
            }
            // Tools were executed — inject results and re-enter inference.
            LoopDecision::Continue(tool_results) => {
                debug!(
                    iteration,
                    results = tool_results.len(),
                    "tool results received, re-entering"
                );

                // Convert text input to structured items on first tool call,
                // then extend in-place on subsequent iterations (no clone).
                if let ResponsesInput::Text(t) = &request.input {
                    let msg = InputItem::Message(InputMessage {
                        role: "user".into(),
                        content: InputMessageContent::Text(t.clone()),
                    });
                    request.input = ResponsesInput::Items(vec![msg]);
                }
                if let ResponsesInput::Items(ref mut items) = request.input {
                    // TODO: also inject assistant's FunctionCall output items here
                    // (requires InputItem::FunctionCall variant — follow-up PR)
                    items.reserve(tool_results.len());
                    items.extend(tool_results);
                }
            }
        }
    }

    unreachable!("loop exits via return in Done/Incomplete/guard arms")
}
