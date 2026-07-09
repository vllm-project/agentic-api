use futures::StreamExt;
use futures::stream as futures_stream;
use tokio::sync::mpsc;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::request::RequestContext;
use crate::tool::{ToolError, ToolOutput, ToolRegistry, ToolType};
use crate::types::io::output::{FunctionToolCall, WebSearchCallStatus};
use crate::types::io::{InputItem, OutputItem, ResponsesInput};
use crate::utils::common::serialize_to_string;

const MAX_GATEWAY_TOOL_CALLS_PER_ROUND: usize = 8;
const MAX_GATEWAY_TOOL_CONCURRENCY: usize = 4;

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
    let calls = function_calls(output_items);
    !registry.client_owned(&calls).is_empty()
}

fn execution_error_output(call: &FunctionToolCall, message: &str) -> ExecutorResult<ToolOutput> {
    let output = serialize_to_string(&serde_json::json!({ "error": message })).map_err(ExecutorError::JsonError)?;
    Ok(ToolOutput {
        call_id: call.call_id.clone(),
        output,
    })
}

async fn execute_gateway_call(call: FunctionToolCall, registry: &ToolRegistry) -> ExecutorResult<GatewayCallResult> {
    let Some(dispatch) = registry.dispatch(&call).await else {
        return Err(ExecutorError::InvalidRequest(format!(
            "gateway tool '{}' was not dispatchable",
            call.name
        )));
    };
    let (output, status) = match dispatch.output {
        Ok(output) => (output, WebSearchCallStatus::Completed),
        Err(ToolError::Execution(message) | ToolError::Config(message)) => {
            (execution_error_output(&call, &message)?, WebSearchCallStatus::Failed)
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
    status: WebSearchCallStatus,
) -> Option<OutputItem> {
    match tool_type {
        ToolType::WebSearch => Some(crate::tool::web_search::output_item(call, output, status)),
        ToolType::Function
        | ToolType::CodexNamespace
        | ToolType::Mcp
        | ToolType::FileSearch
        | ToolType::CodeInterpreter => None,
    }
}

pub(super) async fn execute_output_calls(
    output_items: &[OutputItem],
    registry: &ToolRegistry,
) -> ExecutorResult<Vec<GatewayCallResult>> {
    let calls = function_calls(output_items);
    let gateway_calls = registry.gateway_owned(&calls);
    if gateway_calls.len() > MAX_GATEWAY_TOOL_CALLS_PER_ROUND {
        return Err(ExecutorError::InvalidRequest(format!(
            "gateway tool call limit exceeded: got {}, max {MAX_GATEWAY_TOOL_CALLS_PER_ROUND} per round",
            gateway_calls.len()
        )));
    }

    futures_stream::iter(
        gateway_calls
            .into_iter()
            .cloned()
            .map(|call| execute_gateway_call(call, registry)),
    )
    .buffered(MAX_GATEWAY_TOOL_CONCURRENCY)
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
                    ToolType::Function
                    | ToolType::CodexNamespace
                    | ToolType::Mcp
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
        let OutputItem::WebSearchCall(web_search_call) = output_item else {
            continue;
        };
        let item = output_item_value(output_item)?;
        let added_event = serde_json::json!({
                "type": "response.output_item.added",
                "output_index": plan.output_index,
                "item": item
        });
        emit_sse_json(sender, &added_event)?;
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
        let Some(OutputItem::WebSearchCall(web_search_call)) = &result.public_output else {
            continue;
        };
        let output_index = plans
            .iter()
            .find(|plan| plan.call_id == result.call.call_id)
            .map_or(0, |plan| plan.output_index);
        let output_item = OutputItem::WebSearchCall(web_search_call.clone());
        let item = output_item_value(&output_item)?;
        let completed_event = serde_json::json!({
                "type": "response.web_search_call.completed",
                "item_id": web_search_call.id,
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
