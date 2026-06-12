use std::sync::Arc;

use either::Either;
use tracing::debug;

use crate::executor::dispatch::{LoopDecision, dispatch_tools};
use crate::executor::engine::{execute, persist_response};
use crate::executor::error::ExecutorResult;
use crate::executor::request::ExecutionContext;
use crate::executor::tool_context::ToolContext;
use crate::types::io::ResponsesInput;
use crate::types::request_response::{RequestPayload, ResponsePayload};

/// Run the agentic loop: execute → dispatch tools → re-enter until done.
///
/// Non-streaming MVP: each iteration calls the LLM in blocking mode,
/// inspects output for tool calls, executes them, and re-enters.
///
/// Returns the final `ResponsePayload` once `dispatch_tools` returns
/// `Done` or `Incomplete`.
///
/// # Errors
///
/// Returns `ExecutorError` if any step (execute, dispatch, persist) fails.
pub async fn execute_loop(
    mut request: RequestPayload,
    exec_ctx: Arc<ExecutionContext>,
    tool_ctx: &ToolContext,
) -> ExecutorResult<ResponsePayload> {
    for iteration in 0.. {
        debug!(iteration, "execute_loop iteration");

        let result = execute(request.clone(), Arc::clone(&exec_ctx)).await?;

        let payload = match result {
            Either::Left(payload) => payload,
            Either::Right(_stream) => {
                // Streaming + tool dispatch requires StreamTee (future PR).
                // For now, execute_loop only supports non-streaming.
                // Callers should set stream=false when using execute_loop.
                return Err(crate::executor::ExecutorError::InvalidRequest(
                    "execute_loop does not support streaming yet — set stream=false".into(),
                ));
            }
        };

        let decision = dispatch_tools(&payload.output, tool_ctx, iteration).await?;

        match decision {
            LoopDecision::Done | LoopDecision::Incomplete(_) => {
                if request.store {
                    let ch = exec_ctx.conv_handler.clone();
                    let rh = exec_ctx.resp_handler.clone();
                    let ctx = crate::executor::engine::rehydrate_conversation(request, &exec_ctx).await?;
                    if let Err(e) = persist_response(payload.clone(), ctx, ch, rh).await {
                        tracing::warn!("persist failed in execute_loop: {e}");
                    }
                }
                return Ok(payload);
            }
            LoopDecision::Continue(tool_results) => {
                debug!(
                    iteration,
                    results = tool_results.len(),
                    "tool results received, re-entering"
                );
                // Inject tool results into the request input for next iteration.
                // Use previous_response_id=None since we're managing state internally.
                let mut items = match &request.input {
                    ResponsesInput::Items(existing) => existing.clone(),
                    ResponsesInput::Text(t) => {
                        vec![crate::types::io::InputItem::Message(crate::types::io::InputMessage {
                            role: "user".into(),
                            content: crate::types::io::InputMessageContent::Text(t.clone()),
                        })]
                    }
                };
                items.extend(tool_results);
                request.input = ResponsesInput::Items(items);
                request.previous_response_id = None;
            }
        }
    }

    unreachable!("loop is infinite with break via return")
}
