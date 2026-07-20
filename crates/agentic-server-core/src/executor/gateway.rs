use std::time::Duration;

use futures::StreamExt;
use futures::stream as futures_stream;
use tokio::sync::mpsc;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::request::RequestContext;
use crate::tool::{GatewayDispatchResult, ToolError, ToolOutput, ToolRegistry, ToolType};
use crate::types::io::output::{FunctionToolCall, GatewayCallStatus};
use crate::types::io::{InputItem, OutputItem, ResponsesInput};
use crate::utils::common::serialize_to_string;

/// Max gateway tool calls executing at once within a round. A sliding window:
/// as one call finishes, the next is admitted, so a round with N calls never
/// runs more than this many concurrently but still drains all N. Bounds
/// outbound fan-out without a hard per-round count cap.
///
/// The call count is bounded upstream by the model's output size — there is no
/// unbounded in-memory materialisation from the model emitting arbitrarily many
/// tool calls. The window + per-call timeout bound outbound HTTP and latency.
const MAX_CONCURRENT_GATEWAY_CALLS: usize = 5;

/// Per-call wall-clock budget. A tool exceeding this yields an error output fed
/// back to the model (never a whole-request failure). `Duration::ZERO` disables
/// the timeout — for providers that manage their own.
///
/// Note: this bounds a single call, not the whole request. Worst-case request
/// latency scales with rounds and fan-out; an outer request-level deadline
/// would be the place to cap total time end-to-end.
const GATEWAY_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// Outcome of inspecting one inference turn's output, deciding whether the
/// gateway tool loop should run another round, stop, or surface a partial result.
///
/// `#[non_exhaustive]` so downstream variants can be added without breaking
/// existing match arms.
#[derive(Debug)]
#[non_exhaustive]
pub(super) enum LoopDecision {
    /// Gateway tools were dispatched this round; loop again with their outputs
    /// appended to the conversation.
    Continue,
    /// No gateway work remains — the turn is final and the loop terminates.
    Done,
    /// One or more calls are client-owned (plain `function` or Codex
    /// `namespace` tools); hand the turn back to the caller to execute.
    RequiresClientAction,
    /// The round cap was hit before the model stopped requesting tools. The
    /// response is returned with `status: "incomplete"` rather than as an error.
    Incomplete(String),
}

/// Classify one turn's output into a [`LoopDecision`].
///
/// Order matters: client-owned calls take precedence (they must be handed back
/// even when gateway calls are also present in the same turn), then a
/// no-gateway-work turn is `Done`. Otherwise gateway tools ran — the loop would
/// continue, unless this was the last permitted round, in which case the budget
/// is exhausted and the turn is `Incomplete`.
///
/// `round` is zero-based; `max_rounds` is the total budget.
pub(super) fn classify_round(
    has_client_owned_calls: bool,
    gateway_results: &[GatewayCallResult],
    round: usize,
    max_rounds: usize,
) -> LoopDecision {
    if has_client_owned_calls {
        LoopDecision::RequiresClientAction
    } else if gateway_results.is_empty() {
        LoopDecision::Done
    } else if round + 1 >= max_rounds {
        LoopDecision::Incomplete(format!("gateway tool execution exceeded {max_rounds} rounds"))
    } else {
        LoopDecision::Continue
    }
}

#[derive(Clone)]
pub(super) struct GatewayCallResult {
    pub(super) call: FunctionToolCall,
    pub(super) input_item: InputItem,
    pub(super) public_output: Option<OutputItem>,
}

struct GatewayCallEventPlan {
    call_id: String,
    output_index: u32,
    started_output: Option<OutputItem>,
}

fn function_calls(output_items: &[OutputItem]) -> Vec<FunctionToolCall> {
    output_items
        .iter()
        .filter_map(|item| match item {
            OutputItem::FunctionCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

fn is_gateway_owned_call(call: &FunctionToolCall, registry: &ToolRegistry) -> bool {
    registry
        .lookup(&call.name)
        .is_some_and(|entry| entry.tool_type.is_gateway_owned())
}

pub(super) fn has_client_owned_calls(output_items: &[OutputItem], registry: &ToolRegistry) -> bool {
    output_items.iter().any(|item| item.requires_client_action(registry))
}

fn execution_error_output(call: &FunctionToolCall, message: &str) -> ExecutorResult<ToolOutput> {
    let output = serialize_to_string(&serde_json::json!({ "error": message })).map_err(ExecutorError::JsonError)?;
    Ok(ToolOutput {
        call_id: call.call_id.clone(),
        output,
    })
}

async fn execute_gateway_call(call: FunctionToolCall, registry: &ToolRegistry) -> ExecutorResult<GatewayCallResult> {
    execute_gateway_call_with_timeout(call, registry, GATEWAY_TOOL_TIMEOUT).await
}

async fn execute_gateway_call_with_timeout(
    call: FunctionToolCall,
    registry: &ToolRegistry,
    timeout: Duration,
) -> ExecutorResult<GatewayCallResult> {
    // Resolve the tool type up front so a timeout (which yields no dispatch
    // result) can still shape the correct public output.
    let Some(tool_type) = registry.lookup(&call.name).map(|entry| entry.tool_type) else {
        return Err(ExecutorError::InvalidRequest(format!(
            "gateway tool '{}' was not dispatchable",
            call.name
        )));
    };

    // Per-call timeout: a hung tool becomes an error output fed back to the
    // model, never a whole-request failure. `Duration::ZERO` opts out.
    let dispatched = if timeout.is_zero() {
        registry.dispatch(&call).await
    } else {
        match tokio::time::timeout(timeout, registry.dispatch(&call)).await {
            Ok(dispatched) => dispatched,
            Err(_elapsed) => Some(GatewayDispatchResult {
                tool_type,
                output: Err(ToolError::Execution(format!(
                    "gateway tool '{}' timed out after {timeout:?}",
                    call.name
                ))),
            }),
        }
    };

    // An entry exists (the call was filtered to gateway-owned) but carries no
    // handler — this server was built without that tool's executor. Treat it
    // like the timeout path: surface an error output fed back to the model
    // rather than failing the whole request, keeping the "never a
    // whole-request failure" contract total.
    let dispatch = dispatched.unwrap_or_else(|| GatewayDispatchResult {
        tool_type,
        output: Err(ToolError::Execution(format!(
            "gateway tool '{}' has no registered handler",
            call.name
        ))),
    });
    let (output, status) = match dispatch.output {
        Ok(output) => (output, GatewayCallStatus::Completed),
        Err(ToolError::Execution(message) | ToolError::Config(message)) => {
            (execution_error_output(&call, &message)?, GatewayCallStatus::Failed)
        }
    };
    let public_output = gateway_public_output(dispatch.tool_type, &call, &output, status);
    Ok(GatewayCallResult {
        call,
        input_item: InputItem::FunctionCallOutput(output.into()),
        public_output,
    })
}

fn gateway_public_output(
    tool_type: ToolType,
    call: &FunctionToolCall,
    output: &ToolOutput,
    status: GatewayCallStatus,
) -> Option<OutputItem> {
    match tool_type {
        ToolType::WebSearch => Some(crate::tool::web_search::output_item(call, output, status)),
        ToolType::Mcp => Some(crate::tool::mcp::handler::output_item(call, output, status)),
        ToolType::Function | ToolType::CodexNamespace | ToolType::FileSearch | ToolType::CodeInterpreter => None,
    }
}

pub(super) async fn execute_output_calls(
    output_items: &[OutputItem],
    registry: &ToolRegistry,
) -> ExecutorResult<Vec<GatewayCallResult>> {
    let calls = function_calls(output_items);
    let gateway_calls = registry.gateway_owned(&calls);

    // Execute all gateway calls concurrently with a sliding window of
    // `MAX_CONCURRENT_GATEWAY_CALLS`: `buffered` admits the next call as soon as
    // one finishes, so arbitrary fan-out drains safely without a hard count cap.
    // Each call is individually timeout-bounded in `execute_gateway_call`.
    futures_stream::iter(
        gateway_calls
            .into_iter()
            .cloned()
            .map(|call| execute_gateway_call(call, registry)),
    )
    .buffered(MAX_CONCURRENT_GATEWAY_CALLS)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect()
}

pub(super) fn public_output_items(
    output_items: &[OutputItem],
    registry: &ToolRegistry,
    gateway_results: &[GatewayCallResult],
) -> Vec<OutputItem> {
    output_items
        .iter()
        .map(|item| match item {
            OutputItem::FunctionCall(call) if is_gateway_owned_call(call, registry) => gateway_results
                .iter()
                .find(|result| result.call.call_id == call.call_id)
                .and_then(|result| result.public_output.clone())
                .unwrap_or_else(|| OutputItem::FunctionCall(call.clone())),
            other => other.clone(),
        })
        .collect()
}

fn gateway_event_plans(
    output_items: &[OutputItem],
    registry: &ToolRegistry,
    output_offset: usize,
) -> Vec<GatewayCallEventPlan> {
    let mut output_index = output_offset;
    let mut plans = Vec::new();
    for item in output_items {
        if let OutputItem::FunctionCall(call) = item
            && let Some(entry) = registry.lookup(&call.name)
            && entry.tool_type.is_gateway_owned()
        {
            plans.push(GatewayCallEventPlan {
                call_id: call.call_id.clone(),
                output_index: u32::try_from(output_index).unwrap_or(u32::MAX),
                started_output: match entry.tool_type {
                    ToolType::WebSearch => Some(crate::tool::web_search::started_output_item(call)),
                    ToolType::Mcp => Some(crate::tool::mcp::handler::started_output_item(call)),
                    ToolType::Function
                    | ToolType::CodexNamespace
                    | ToolType::FileSearch
                    | ToolType::CodeInterpreter => None,
                },
            });
        }
        output_index = output_index.saturating_add(1);
    }
    plans
}

fn emit_sse_json(sender: &mpsc::UnboundedSender<String>, event: &serde_json::Value) -> ExecutorResult<()> {
    let event_json = serialize_to_string(&event).map_err(ExecutorError::JsonError)?;
    sender
        .send(format!("data: {event_json}\n\n"))
        .map_err(|_| ExecutorError::StreamError("stream receiver closed while emitting gateway event".to_owned()))
}

fn output_item_value(item: &OutputItem) -> ExecutorResult<serde_json::Value> {
    serde_json::to_value(item).map_err(ExecutorError::JsonError)
}

fn emit_gateway_start_events(
    plans: &[GatewayCallEventPlan],
    stream_events: Option<&mpsc::UnboundedSender<String>>,
) -> ExecutorResult<()> {
    let Some(sender) = stream_events else {
        return Ok(());
    };
    for plan in plans {
        let Some(output_item) = &plan.started_output else {
            continue;
        };
        let item = output_item_value(output_item)?;
        let added_event = serde_json::json!({
                "type": "response.output_item.added",
                "output_index": plan.output_index,
                "item": item
        });
        emit_sse_json(sender, &added_event)?;
        match output_item {
            OutputItem::WebSearchCall(web_search_call) => {
                let in_progress_event = serde_json::json!({
                        "type": "response.web_search_call.in_progress",
                        "item_id": web_search_call.id,
                        "output_index": plan.output_index
                });
                emit_sse_json(sender, &in_progress_event)?;
                let searching_event = serde_json::json!({
                        "type": "response.web_search_call.searching",
                        "item_id": web_search_call.id,
                        "output_index": plan.output_index
                });
                emit_sse_json(sender, &searching_event)?;
            }
            OutputItem::McpToolCall(mcp_tool_call) => {
                let in_progress_event = serde_json::json!({
                        "type": "response.mcp_tool_call.in_progress",
                        "item_id": mcp_tool_call.id,
                        "output_index": plan.output_index
                });
                emit_sse_json(sender, &in_progress_event)?;
            }
            OutputItem::Message(_)
            | OutputItem::FunctionCall(_)
            | OutputItem::CustomToolCall(_)
            | OutputItem::Reasoning(_)
            | OutputItem::Unknown => {}
        }
    }
    Ok(())
}

fn emit_gateway_completed_events(
    results: &[GatewayCallResult],
    plans: &[GatewayCallEventPlan],
    stream_events: Option<&mpsc::UnboundedSender<String>>,
) -> ExecutorResult<()> {
    let Some(sender) = stream_events else {
        return Ok(());
    };
    for result in results {
        let Some(public_output) = &result.public_output else {
            continue;
        };
        let output_index = plans
            .iter()
            .find(|plan| plan.call_id == result.call.call_id)
            .map_or(0, |plan| plan.output_index);
        let (event_type, item_id) = match public_output {
            OutputItem::WebSearchCall(web_search_call) => {
                ("response.web_search_call.completed", web_search_call.id.as_str())
            }
            OutputItem::McpToolCall(mcp_tool_call) => ("response.mcp_tool_call.completed", mcp_tool_call.id.as_str()),
            OutputItem::Message(_)
            | OutputItem::FunctionCall(_)
            | OutputItem::CustomToolCall(_)
            | OutputItem::Reasoning(_)
            | OutputItem::Unknown => continue,
        };
        let item = output_item_value(public_output)?;
        let completed_event = serde_json::json!({
                "type": event_type,
                "item_id": item_id,
                "output_index": output_index,
                "item": item.clone()
        });
        emit_sse_json(sender, &completed_event)?;
        let done_event = serde_json::json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item
        });
        emit_sse_json(sender, &done_event)?;
    }
    Ok(())
}

pub(super) async fn execute_and_emit_output_calls(
    output_items: &[OutputItem],
    registry: &ToolRegistry,
    output_offset: usize,
    stream_events: Option<&mpsc::UnboundedSender<String>>,
) -> ExecutorResult<Vec<GatewayCallResult>> {
    let event_plans = gateway_event_plans(output_items, registry, output_offset);
    emit_gateway_start_events(&event_plans, stream_events)?;
    let gateway_results = execute_output_calls(output_items, registry).await?;
    emit_gateway_completed_events(&gateway_results, &event_plans, stream_events)?;
    Ok(gateway_results)
}

pub(super) fn append_input_item(input: &mut ResponsesInput, item: InputItem) {
    match input {
        ResponsesInput::Items(items) => items.push(item),
        ResponsesInput::Text(text) => {
            let text_input = ResponsesInput::Text(std::mem::take(text));
            let mut items = Vec::<InputItem>::from(&text_input);
            items.push(item);
            *input = ResponsesInput::Items(items);
        }
    }
}

pub(super) fn append_output_items_to_input(input: &mut ResponsesInput, output_items: &[OutputItem]) {
    for input_item in output_items.iter().filter_map(OutputItem::to_input_item) {
        append_input_item(input, input_item);
    }
}

pub(super) fn append_tool_outputs(ctx: &mut RequestContext, tool_outputs: Vec<InputItem>) {
    for output in tool_outputs {
        ctx.new_input_items.push(output.clone());
        append_input_item(&mut ctx.enriched_request.input, output);
    }
}

pub(super) fn append_gateway_calls_to_new_input(
    ctx: &mut RequestContext,
    output_items: &[OutputItem],
    registry: &ToolRegistry,
) {
    ctx.new_input_items.extend(output_items.iter().filter_map(|item| {
        let OutputItem::FunctionCall(call) = item else {
            return None;
        };
        is_gateway_owned_call(call, registry).then(|| InputItem::FunctionCall(call.clone()))
    }));
}

#[cfg(test)]
mod tests {
    use super::{GatewayCallResult, LoopDecision, classify_round};
    use crate::types::io::InputItem;
    use crate::types::io::output::FunctionToolCall;

    const MAX: usize = 10;

    fn dummy_result() -> GatewayCallResult {
        let call = FunctionToolCall {
            id: "id".to_owned(),
            call_id: "call".to_owned(),
            name: "web_search".to_owned(),
            arguments: "{}".to_owned(),
            status: crate::types::event::MessageStatus::Completed,
            namespace: None,
        };
        GatewayCallResult {
            call,
            input_item: InputItem::FunctionCallOutput(
                crate::tool::ToolOutput {
                    call_id: "call".to_owned(),
                    output: "{}".to_owned(),
                }
                .into(),
            ),
            public_output: None,
        }
    }

    #[test]
    fn client_owned_calls_take_precedence_over_gateway_results() {
        // Even with gateway results present in the same turn, a client-owned call
        // must hand control back to the caller.
        let decision = classify_round(true, &[dummy_result()], 0, MAX);
        assert!(matches!(decision, LoopDecision::RequiresClientAction));
    }

    #[test]
    fn no_gateway_work_is_done() {
        let decision = classify_round(false, &[], 0, MAX);
        assert!(matches!(decision, LoopDecision::Done));
    }

    #[test]
    fn gateway_results_with_budget_remaining_continue() {
        let decision = classify_round(false, &[dummy_result()], 0, MAX);
        assert!(matches!(decision, LoopDecision::Continue));
    }

    #[test]
    fn gateway_results_on_final_round_are_incomplete() {
        // round is zero-based: round 9 is the 10th and last permitted round.
        let decision = classify_round(false, &[dummy_result()], MAX - 1, MAX);
        match decision {
            LoopDecision::Incomplete(reason) => assert!(reason.contains("exceeded 10 rounds")),
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[test]
    fn incomplete_only_fires_when_gateway_work_remains() {
        // On the final round with no gateway work, the turn is still Done — the
        // cap only matters when the model is still requesting tools.
        let decision = classify_round(false, &[], MAX - 1, MAX);
        assert!(matches!(decision, LoopDecision::Done));
    }

    use std::pin::Pin;
    use std::sync::Arc;

    use serde_json::Value;

    use super::execute_gateway_call_with_timeout;
    use crate::tool::{GatewayExecutor, GatewayExecutors, ToolError, ToolHandler, ToolOutput, ToolRegistry, ToolType};
    use crate::types::io::OutputItem;
    use crate::types::io::tools::FunctionTool;
    use crate::types::tools::ResponsesTool;

    /// A gateway executor that sleeps ~50ms — comfortably longer than the tiny
    /// timeout the test injects, forcing the timeout path without a paused clock.
    struct SlowExecutor;

    impl ToolHandler for SlowExecutor {
        fn tool_type(&self) -> ToolType {
            ToolType::WebSearch
        }
        fn validate(&self, _param: &Value) -> Result<(), ToolError> {
            Ok(())
        }
        fn normalize(&self, _param: &Value) -> Vec<FunctionTool> {
            Vec::new()
        }
    }

    impl GatewayExecutor for SlowExecutor {
        fn execute(
            &self,
            call_id: &str,
            _tool_name: &str,
            _arguments: &str,
            _config: &Value,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
            let call_id = call_id.to_owned();
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(ToolOutput {
                    call_id,
                    output: "unreachable".to_owned(),
                })
            })
        }
    }

    fn web_search_call(call_id: &str) -> FunctionToolCall {
        FunctionToolCall {
            id: format!("fc_{call_id}"),
            call_id: call_id.to_owned(),
            name: "web_search".to_owned(),
            arguments: "{}".to_owned(),
            status: crate::types::event::MessageStatus::Completed,
            namespace: None,
        }
    }

    #[tokio::test]
    async fn hung_gateway_call_times_out_into_error_output() {
        let web_search: ResponsesTool =
            serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).expect("web_search tool param");
        let mut executors = GatewayExecutors::default();
        executors.insert(Arc::new(SlowExecutor));
        let registry = ToolRegistry::build_with_handlers(&[web_search], &executors)
            .await
            .expect("registry builds");

        // 1ms budget vs a 50ms tool → the timeout fires. Must return (not hang):
        // the stuck call becomes an error output the loop can feed back.
        let result = execute_gateway_call_with_timeout(
            web_search_call("call_hang"),
            &registry,
            std::time::Duration::from_millis(1),
        )
        .await
        .expect("timeout is isolated as an error output, not a dispatch failure");

        assert_eq!(result.call.call_id, "call_hang");
        // A failed web_search still yields a public web_search_call item.
        assert!(matches!(result.public_output, Some(OutputItem::WebSearchCall(_))));
        // The fed-back tool output is an error JSON mentioning the timeout.
        let InputItem::FunctionCallOutput(msg) = &result.input_item else {
            panic!("expected a function_call_output");
        };
        let body = serde_json::to_string(msg).expect("serialize output");
        assert!(
            body.contains("timed out"),
            "error output should mention the timeout: {body}"
        );
    }

    #[tokio::test]
    async fn gateway_call_without_registered_handler_becomes_error_output() {
        // Declare web_search but build the registry with NO executor for it —
        // the entry exists and is gateway-owned, so the call is not filtered
        // out, but `dispatch` yields `None`. This must surface an error output,
        // not fail the whole request.
        let web_search: ResponsesTool =
            serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).expect("web_search tool param");
        let registry = ToolRegistry::build_with_handlers(&[web_search], &GatewayExecutors::default())
            .await
            .expect("registry builds");

        let result =
            execute_gateway_call_with_timeout(web_search_call("call_no_handler"), &registry, std::time::Duration::ZERO)
                .await
                .expect("a missing handler is isolated as an error output, not a dispatch failure");

        assert_eq!(result.call.call_id, "call_no_handler");
        assert!(matches!(result.public_output, Some(OutputItem::WebSearchCall(_))));
        let InputItem::FunctionCallOutput(msg) = &result.input_item else {
            panic!("expected a function_call_output");
        };
        let body = serde_json::to_string(msg).expect("serialize output");
        assert!(
            body.contains("no registered handler"),
            "error output should mention the missing handler: {body}"
        );
    }
}
