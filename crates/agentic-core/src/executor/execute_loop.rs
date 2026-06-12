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

/// Hard cap on loop iterations — defense-in-depth independent of `tool_ctx.max_iterations`.
const MAX_LOOP_GUARD: usize = 128;

/// Run the agentic loop: execute → dispatch tools → re-enter until done.
///
/// Non-streaming MVP: each iteration calls the LLM in blocking mode,
/// inspects output for tool calls, executes them, and re-enters.
///
/// Returns the final `ResponsePayload` once `dispatch_tools` returns
/// `Done` or `Incomplete`.
///
/// Performance note: `request` is cloned each iteration because `execute()`
/// takes ownership. The input grows with each tool result injection. This is
/// acceptable for MVP but should be optimized for long tool chains.
///
/// # Errors
///
/// Returns `ExecutorError` if any step (execute, dispatch) fails, or if
/// the loop guard is breached.
pub async fn execute_loop(
    mut request: RequestPayload,
    exec_ctx: Arc<ExecutionContext>,
    tool_ctx: &ToolContext,
) -> ExecutorResult<ResponsePayload> {
    // Capture original previous_response_id before the loop mutates it.
    // Restored on the final payload to maintain correct response chain.
    let original_previous_response_id = request.previous_response_id.clone();

    for iteration in 0_usize.. {
        if iteration >= MAX_LOOP_GUARD {
            return Err(crate::executor::ExecutorError::InvalidRequest(format!(
                "execute_loop exceeded hard iteration cap ({MAX_LOOP_GUARD})"
            )));
        }

        debug!(iteration, "execute_loop iteration");

        let result = execute(request.clone(), Arc::clone(&exec_ctx)).await?;

        let mut payload = match result {
            Either::Left(payload) => payload,
            Either::Right(_stream) => {
                return Err(crate::executor::ExecutorError::InvalidRequest(
                    "execute_loop does not support streaming yet — set stream=false".into(),
                ));
            }
        };

        let decision = dispatch_tools(&payload.output, tool_ctx, iteration).await?;

        match decision {
            LoopDecision::Done => {
                // Restore original previous_response_id for correct chain persistence.
                payload.previous_response_id = original_previous_response_id;
                // Persistence is handled by the caller (server handler) which has
                // the full RequestContext. execute_loop returns the payload only.
                return Ok(payload);
            }
            LoopDecision::Incomplete(reason) => {
                debug!(iteration, %reason, "loop incomplete");
                // Mark the payload as incomplete so the caller/client knows.
                payload.status = "incomplete".to_string();
                payload.previous_response_id = original_previous_response_id;
                return Ok(payload);
            }
            LoopDecision::Continue(tool_results) => {
                debug!(
                    iteration,
                    results = tool_results.len(),
                    "tool results received, re-entering"
                );
                let mut items = match &request.input {
                    ResponsesInput::Items(existing) => existing.clone(),
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
                items.extend(tool_results);
                request.input = ResponsesInput::Items(items);
                // Clear previous_response_id — we're managing context internally.
                request.previous_response_id = None;
                // Suppress persistence for intermediate iterations — only the
                // final response should be stored.
                request.store = false;
            }
        }
    }

    unreachable!("loop exits via return")
}
