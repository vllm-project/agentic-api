use crate::executor::error::ExecutorResult;
use crate::executor::tool_context::ToolContext;
use crate::types::io::{InputItem, OutputItem};

/// Decision returned by [`dispatch_tools`] to drive the agentic loop.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoopDecision {
    /// Gateway-executed tools returned results. Inject into input and re-infer.
    Continue(Vec<InputItem>),
    /// No tool calls found, or only client-side functions. Return response to client.
    Done,
    /// Max iterations reached or unrecoverable tool failure.
    Incomplete(String),
}

/// Inspect executor output for function calls and dispatch them via [`ToolContext`].
///
/// Returns [`LoopDecision`] indicating whether the caller should re-enter
/// inference (Continue), return the response (Done), or mark incomplete.
///
/// For MVP, all `FunctionCall` items are treated as gateway-executable.
/// Client-side function routing (checking the request's `tools` array) is
/// deferred to a follow-up PR.
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
    let function_calls: Vec<_> = output
        .iter()
        .filter_map(|item| match item {
            OutputItem::FunctionCall(fc) => Some(fc),
            _ => None,
        })
        .collect();

    if function_calls.is_empty() {
        return Ok(LoopDecision::Done);
    }

    if iteration >= tool_ctx.max_iterations {
        return Ok(LoopDecision::Incomplete(format!(
            "max tool iterations reached ({iteration}/{})",
            tool_ctx.max_iterations
        )));
    }

    let results = tool_ctx.execute_all(&function_calls).await;
    Ok(LoopDecision::Continue(results))
}
