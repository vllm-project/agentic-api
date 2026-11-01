//! Public SSE lifecycle for gateway-executed calls.
//!
//! Each scheduler slot keeps a typed [`GatewayEventPlan`]; these functions turn
//! those plans and the calls' public outputs into synthetic events. Indexes come
//! from the plans, and sequence numbers from the [`StreamRelay`] that presents them.

use std::{iter, slice};

use futures::StreamExt;
use tokio::sync::mpsc;

use super::{GatewayCallResult, GatewayEventPlan, GatewayPublicOutputSource, GatewayScheduler};
use crate::events::{EventFrame, EventPayload, SSEEventType};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::synthetic_event;
use crate::executor::relay::{Release, StreamRelay};
use crate::executor::request::RequestContext;
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::types::io::output::{McpCallStatus, WebSearchCallStatus};
use crate::types::io::{CodeInterpreterCallStatus, CodeInterpreterCallStreamEvent, OutputItem};
use crate::types::request_response::ResponsePayload;
use crate::types::tools::ToolSearchStatus;
use crate::utils::common::{deserialize_from_value, serialize_to_value};

fn output_item_value(item: &OutputItem) -> ExecutorResult<serde_json::Value> {
    serde_json::to_value(item).map_err(ExecutorError::JsonError)
}

fn code_interpreter_event_frame(event: &CodeInterpreterCallStreamEvent) -> ExecutorResult<EventFrame> {
    let event_type = match event {
        CodeInterpreterCallStreamEvent::InProgress { .. } => SSEEventType::CodeInterpreterCallInProgress,
        CodeInterpreterCallStreamEvent::CodeDelta { .. } => SSEEventType::CodeInterpreterCallCodeDelta,
        CodeInterpreterCallStreamEvent::CodeDone { .. } => SSEEventType::CodeInterpreterCallCodeDone,
        CodeInterpreterCallStreamEvent::Interpreting { .. } => SSEEventType::CodeInterpreterCallInterpreting,
        CodeInterpreterCallStreamEvent::Completed { .. } => SSEEventType::CodeInterpreterCallCompleted,
    };
    let value = serialize_to_value(event).map_err(ExecutorError::JsonError)?;
    let wire = deserialize_from_value(value).map_err(ExecutorError::JsonError)?;
    Ok(EventFrame {
        event_type,
        payload: EventPayload::None,
        wire,
    })
}

async fn emit_code_interpreter_event(
    event: &CodeInterpreterCallStreamEvent,
    relay: &mut StreamRelay,
) -> ExecutorResult<()> {
    let mut frame = code_interpreter_event_frame(event)?;
    relay.emit_local(&mut frame).await?;
    Ok(())
}

async fn emit_code_interpreter_start_events(
    call: &crate::types::io::CodeInterpreterCall,
    output_index: u32,
    relay: &mut StreamRelay,
) -> ExecutorResult<()> {
    let item_id = call.id.clone();
    emit_code_interpreter_event(
        &CodeInterpreterCallStreamEvent::InProgress {
            item_id: item_id.clone(),
            output_index,
            sequence_number: relay.upcoming_sequence_number(),
        },
        relay,
    )
    .await?;
    emit_code_interpreter_event(
        &CodeInterpreterCallStreamEvent::CodeDelta {
            item_id: item_id.clone(),
            output_index,
            sequence_number: relay.upcoming_sequence_number(),
            delta: call.code.clone(),
        },
        relay,
    )
    .await?;
    emit_code_interpreter_event(
        &CodeInterpreterCallStreamEvent::CodeDone {
            item_id: item_id.clone(),
            output_index,
            sequence_number: relay.upcoming_sequence_number(),
            code: call.code.clone(),
        },
        relay,
    )
    .await?;
    emit_code_interpreter_event(
        &CodeInterpreterCallStreamEvent::Interpreting {
            item_id,
            output_index,
            sequence_number: relay.upcoming_sequence_number(),
        },
        relay,
    )
    .await?;
    Ok(())
}

pub(in crate::executor) async fn emit_response_start_events(
    payload: &ResponsePayload,
    relay: &mut StreamRelay,
) -> ExecutorResult<()> {
    // A detached relay presents nothing, so skip building its events.
    if !relay.is_live() {
        return Ok(());
    }
    let mut response = payload.clone();
    "in_progress".clone_into(&mut response.status);
    response.output.clear();
    response.usage = None;
    let response = serialize_to_value(&response).map_err(ExecutorError::JsonError)?;
    for event_type in [SSEEventType::ResponseCreated, SSEEventType::ResponseInProgress] {
        let mut event = synthetic_event(event_type, [("response".to_owned(), response.clone())])?;
        relay.emit_local(&mut event).await?;
    }
    Ok(())
}

async fn emit_gateway_added_event(
    output_index: u32,
    output_item: &OutputItem,
    relay: &mut StreamRelay,
) -> ExecutorResult<()> {
    let item = match output_item {
        OutputItem::CodeInterpreterCall(call) => {
            let mut added = call.clone();
            added.code.clear();
            output_item_value(&OutputItem::CodeInterpreterCall(added))?
        }
        _ => output_item_value(output_item)?,
    };
    let mut added_event = synthetic_event(
        SSEEventType::OutputItemAdded,
        [
            ("output_index".to_owned(), serde_json::json!(output_index)),
            ("item".to_owned(), item),
        ],
    )?;
    relay.emit_local(&mut added_event).await?;
    Ok(())
}

pub(in crate::executor) async fn emit_gateway_start_events<'a>(
    plans: impl IntoIterator<Item = &'a GatewayEventPlan>,
    relay: &mut StreamRelay,
) -> ExecutorResult<()> {
    if !relay.is_live() {
        return Ok(());
    }
    for plan in plans {
        let Some(output_item) = &plan.started_output else {
            continue;
        };
        emit_gateway_added_event(plan.output_index, output_item, relay).await?;
        match output_item {
            OutputItem::WebSearchCall(web_search_call) => {
                let mut in_progress_event = synthetic_event(
                    SSEEventType::WebSearchCallInProgress,
                    [
                        ("item_id".to_owned(), serde_json::json!(web_search_call.id)),
                        ("output_index".to_owned(), serde_json::json!(plan.output_index)),
                    ],
                )?;
                relay.emit_local(&mut in_progress_event).await?;
                let mut searching_event = synthetic_event(
                    SSEEventType::WebSearchCallSearching,
                    [
                        ("item_id".to_owned(), serde_json::json!(web_search_call.id)),
                        ("output_index".to_owned(), serde_json::json!(plan.output_index)),
                    ],
                )?;
                relay.emit_local(&mut searching_event).await?;
            }
            OutputItem::McpCall(mcp_call) => {
                let mut in_progress_event = synthetic_event(
                    SSEEventType::McpCallInProgress,
                    [
                        ("item_id".to_owned(), serde_json::json!(mcp_call.id)),
                        ("output_index".to_owned(), serde_json::json!(plan.output_index)),
                    ],
                )?;
                relay.emit_local(&mut in_progress_event).await?;
                let arguments = plan.arguments.as_deref().unwrap_or_default();
                let mut arguments_delta_event = synthetic_event(
                    SSEEventType::McpCallArgumentsDelta,
                    [
                        ("delta".to_owned(), serde_json::json!(arguments)),
                        ("item_id".to_owned(), serde_json::json!(mcp_call.id)),
                        ("output_index".to_owned(), serde_json::json!(plan.output_index)),
                    ],
                )?;
                relay.emit_local(&mut arguments_delta_event).await?;
                let mut arguments_done_event = synthetic_event(
                    SSEEventType::McpCallArgumentsDone,
                    [
                        ("arguments".to_owned(), serde_json::json!(arguments)),
                        ("item_id".to_owned(), serde_json::json!(mcp_call.id)),
                        ("output_index".to_owned(), serde_json::json!(plan.output_index)),
                    ],
                )?;
                relay.emit_local(&mut arguments_done_event).await?;
            }
            OutputItem::McpListTools(list_tools) => {
                let mut in_progress_event = synthetic_event(
                    SSEEventType::McpListToolsInProgress,
                    [
                        ("item_id".to_owned(), serde_json::json!(list_tools.id)),
                        ("output_index".to_owned(), serde_json::json!(plan.output_index)),
                    ],
                )?;
                relay.emit_local(&mut in_progress_event).await?;
            }
            OutputItem::CodeInterpreterCall(code_interpreter_call) => {
                emit_code_interpreter_start_events(code_interpreter_call, plan.output_index, relay).await?;
            }
            OutputItem::Message(_)
            | OutputItem::FunctionCall(_)
            | OutputItem::ToolSearchCall(_)
            | OutputItem::ToolSearchOutput(_)
            | OutputItem::CustomToolCall(_)
            | OutputItem::ShellCall(_)
            | OutputItem::Reasoning(_)
            | OutputItem::Compaction(_)
            | OutputItem::MultiAgentCall(_)
            | OutputItem::MultiAgentCallOutput(_)
            | OutputItem::AgentMessage(_)
            | OutputItem::Unknown => {}
        }
    }
    Ok(())
}

pub(in crate::executor) async fn emit_gateway_completed_events<'a, T: GatewayPublicOutputSource>(
    results: &[T],
    plans: impl IntoIterator<Item = &'a GatewayEventPlan>,
    relay: &mut StreamRelay,
) -> ExecutorResult<()> {
    if !relay.is_live() {
        return Ok(());
    }
    for (index, plan) in plans.into_iter().enumerate() {
        let Some(public_output) = plan
            .completed_output
            .as_ref()
            .or_else(|| results.get(index).and_then(GatewayPublicOutputSource::public_output))
        else {
            continue;
        };
        let output_index = plan.output_index;
        let completed_event = match public_output {
            OutputItem::WebSearchCall(web_search_call) => (web_search_call.status != WebSearchCallStatus::Searching)
                .then_some((SSEEventType::WebSearchCallCompleted, web_search_call.id.as_str())),
            OutputItem::McpCall(mcp_call) => Some((
                if mcp_call.status == Some(McpCallStatus::Failed) {
                    SSEEventType::McpCallFailed
                } else {
                    SSEEventType::McpCallCompleted
                },
                mcp_call.id.as_str(),
            )),
            OutputItem::McpListTools(list_tools) => Some((
                if list_tools.error.is_some() {
                    SSEEventType::McpListToolsFailed
                } else {
                    SSEEventType::McpListToolsCompleted
                },
                list_tools.id.as_str(),
            )),
            // A hosted search has only the generic added/done events.
            OutputItem::CodeInterpreterCall(_)
            | OutputItem::Compaction(_)
            | OutputItem::ShellCall(_)
            | OutputItem::ToolSearchCall(_)
            | OutputItem::ToolSearchOutput(_) => None,
            OutputItem::Message(_)
            | OutputItem::FunctionCall(_)
            | OutputItem::CustomToolCall(_)
            | OutputItem::Reasoning(_)
            | OutputItem::MultiAgentCall(_)
            | OutputItem::MultiAgentCallOutput(_)
            | OutputItem::AgentMessage(_)
            | OutputItem::Unknown => continue,
        };
        let item = output_item_value(public_output)?;
        if let OutputItem::CodeInterpreterCall(code_interpreter_call) = public_output
            && code_interpreter_call.status != CodeInterpreterCallStatus::Interpreting
        {
            emit_code_interpreter_event(
                &CodeInterpreterCallStreamEvent::Completed {
                    item_id: code_interpreter_call.id.clone(),
                    output_index,
                    sequence_number: relay.upcoming_sequence_number(),
                },
                relay,
            )
            .await?;
        }
        if let Some((event_type, item_id)) = completed_event {
            let mut completed_fields = serde_json::Map::from_iter([
                ("item_id".to_owned(), serde_json::json!(item_id)),
                ("output_index".to_owned(), serde_json::json!(output_index)),
            ]);
            if matches!(public_output, OutputItem::WebSearchCall(_)) {
                completed_fields.insert("item".to_owned(), item.clone());
            }
            let mut completed_event = synthetic_event(event_type, completed_fields)?;
            relay.emit_local(&mut completed_event).await?;
        }
        let mut done_event = synthetic_event(
            SSEEventType::OutputItemDone,
            [
                ("output_index".to_owned(), serde_json::json!(output_index)),
                ("item".to_owned(), item),
            ],
        )?;
        relay.emit_local(&mut done_event).await?;
        if let Some(follow_up) = &plan.follow_up_output {
            emit_follow_up_events(output_index.saturating_add(1), follow_up, relay).await?;
        }
    }
    Ok(())
}

/// A follow-up item is complete when planned; it still opens `in_progress`
/// before completing, as the Responses API streams a `tool_search_output`.
async fn emit_follow_up_events(output_index: u32, item: &OutputItem, relay: &mut StreamRelay) -> ExecutorResult<()> {
    let mut added = item.clone();
    if let OutputItem::ToolSearchOutput(output) = &mut added {
        output.status = ToolSearchStatus::InProgress;
    }
    emit_gateway_added_event(output_index, &added, relay).await?;
    let mut done_event = synthetic_event(
        SSEEventType::OutputItemDone,
        [
            ("output_index".to_owned(), serde_json::json!(output_index)),
            ("item".to_owned(), output_item_value(item)?),
        ],
    )?;
    relay.emit_local(&mut done_event).await?;
    Ok(())
}

/// Results in call order, as [`drive_in_order`] hands them to its consumer.
pub(super) type OrderedResults = mpsc::Receiver<ExecutorResult<GatewayCallResult>>;

/// Runs a round's calls to completion while `consume` emits their results.
///
/// The calls keep running however slowly the client reads: each result goes
/// into a channel with one slot per call (each output is already bounded by
/// `MAX_GATEWAY_TOOL_OUTPUT_BYTES`), so producing never waits on emission.
/// Every call is drained before this returns, even after `consume` stops early
/// on a call error or a relay error; that early error is the one returned.
pub(super) async fn drive_in_order<T>(
    scheduler: &GatewayScheduler,
    response_budget: &ExecutorResponseBudget,
    consume: impl AsyncFnOnce(&mut OrderedResults) -> ExecutorResult<T>,
) -> ExecutorResult<T> {
    let (sender, mut receiver) = mpsc::channel(scheduler.call_count().max(1));
    let execution_slots = scheduler.policy.new_round_execution_slots();
    let produce = async move {
        let mut ordered = scheduler.results_in_order(&execution_slots, response_budget);
        while let Some(result) = ordered.next().await {
            // Never waits: the channel has a slot per call. After the consumer stops early,
            // the remaining results still fill their slots and are dropped with the channel.
            let _ = sender.send(result).await;
        }
    };
    let ((), outcome) = tokio::join!(produce, consume(&mut receiver));
    outcome
}

async fn next_result(results: &mut OrderedResults) -> ExecutorResult<GatewayCallResult> {
    results
        .recv()
        .await
        .ok_or_else(|| ExecutorError::StreamError("a planned gateway call produced no result".to_owned()))?
}

/// Emits one round's gateway lifecycles when no upstream frames are deferred.
/// Calls up to the first follow-up item are announced before they run. Each
/// completion follows in call order as soon as that call and every earlier one
/// have finished; after a follow-up item, the calls up to the next follow-up
/// are announced together, so their starts precede their completions.
pub(in crate::executor) async fn emit_calls_in_order(
    scheduler: &GatewayScheduler,
    response_budget: &ExecutorResponseBudget,
    relay: &mut StreamRelay,
) -> ExecutorResult<Vec<GatewayCallResult>> {
    let mut announced = scheduler.start_run_end(0);
    emit_gateway_start_events(scheduler.event_plans().take(announced), relay).await?;
    drive_in_order(scheduler, response_budget, async |ordered| {
        let mut results = Vec::with_capacity(scheduler.call_count());
        while results.len() < scheduler.call_count() {
            let result = next_result(ordered).await?;
            let plan = planned_event(scheduler, results.len())?;
            emit_gateway_completed_events(slice::from_ref(&result), iter::once(plan), relay).await?;
            let next_run_end = scheduler.start_run_end(results.len() + 1);
            emit_gateway_start_events(scheduler.event_plans().take(next_run_end).skip(announced), relay).await?;
            announced = announced.max(next_run_end);
            results.push(result);
        }
        Ok(results)
    })
    .await
}

/// Releases one round's deferred upstream frames in output order, emitting each
/// gateway call's lifecycle at its public index first. Calls run concurrently
/// (see [`drive_in_order`]); each completes on the wire as soon as it and every
/// earlier call finished, and a call past the first `initial_event_run_len` is
/// announced just before that. Omitted refused calls have no public item
/// (ingestion suppresses gateway call frames) and shift later items so public
/// indexes stay contiguous.
pub(in crate::executor) async fn relay_round_events(
    scheduler: &GatewayScheduler,
    response_budget: &ExecutorResponseBudget,
    item_count: usize,
    relay: &mut StreamRelay,
    request: &RequestContext,
    initial_event_run_len: usize,
) -> ExecutorResult<Vec<GatewayCallResult>> {
    let results = drive_in_order(scheduler, response_budget, async |ordered| {
        let mut results = Vec::with_capacity(scheduler.call_count());
        for index in 0..item_count {
            let public_index = scheduler.public_item_index(index);
            if let Some(call_index) = scheduler.call_index_for_item(index) {
                let plan = planned_event(scheduler, call_index)?;
                if public_index.is_some() && call_index >= initial_event_run_len {
                    emit_gateway_start_events(iter::once(plan), relay).await?;
                }
                let result = next_result(ordered).await?;
                if public_index.is_some() {
                    emit_gateway_completed_events(slice::from_ref(&result), iter::once(plan), relay).await?;
                }
                results.push(result);
            }
            let Some(public_index) = public_index else {
                continue;
            };
            let release = Release::Item {
                index: index as u64,
                public_index: public_index as u64,
            };
            relay.release_deferred(release, request).await?;
        }
        Ok(results)
    })
    .await?;
    // Frames without an index, or past this round's items, follow every item.
    relay.release_deferred(Release::All, request).await?;
    Ok(results)
}

fn planned_event(scheduler: &GatewayScheduler, call_index: usize) -> ExecutorResult<&GatewayEventPlan> {
    scheduler
        .event_plan(call_index)
        .ok_or_else(|| ExecutorError::StreamError("a gateway call has no event plan".to_owned()))
}
