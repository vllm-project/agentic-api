//! Public SSE lifecycle for gateway-executed calls.
//!
//! Each scheduler slot keeps a typed [`GatewayEventPlan`]; these functions turn
//! those plans and the calls' public outputs into synthetic events. Indexes come
//! from the plans, and sequence numbers from [`GatewayStreamAccumulator`].

use super::{GatewayCallResult, GatewayEventPlan, GatewayPublicOutputSource, GatewayScheduler};
use crate::events::{EventFrame, EventPayload, SSEEventType};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::{GatewayStreamAccumulator, StreamEvent, synthetic_event};
use crate::executor::pipeline::{emit_deferred_stream_events, emit_gateway_event};
use crate::executor::request::RequestContext;
use crate::types::io::output::{McpCallStatus, WebSearchCallStatus};
use crate::types::io::{CodeInterpreterCallStatus, CodeInterpreterCallStreamEvent, OutputItem};
use crate::types::request_response::ResponsePayload;
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
    stream_accumulator: &mut GatewayStreamAccumulator,
    stream_sender: &tokio::sync::mpsc::Sender<StreamEvent>,
) -> ExecutorResult<()> {
    let mut frame = code_interpreter_event_frame(event)?;
    emit_gateway_event(&mut frame, stream_accumulator, stream_sender).await
}

async fn emit_code_interpreter_start_events(
    call: &crate::types::io::CodeInterpreterCall,
    output_index: u32,
    stream_accumulator: &mut GatewayStreamAccumulator,
    stream_sender: &tokio::sync::mpsc::Sender<StreamEvent>,
) -> ExecutorResult<()> {
    let item_id = call.id.clone();
    emit_code_interpreter_event(
        &CodeInterpreterCallStreamEvent::InProgress {
            item_id: item_id.clone(),
            output_index,
            sequence_number: stream_accumulator.upcoming_sequence_number(),
        },
        stream_accumulator,
        stream_sender,
    )
    .await?;
    emit_code_interpreter_event(
        &CodeInterpreterCallStreamEvent::CodeDelta {
            item_id: item_id.clone(),
            output_index,
            sequence_number: stream_accumulator.upcoming_sequence_number(),
            delta: call.code.clone(),
        },
        stream_accumulator,
        stream_sender,
    )
    .await?;
    emit_code_interpreter_event(
        &CodeInterpreterCallStreamEvent::CodeDone {
            item_id: item_id.clone(),
            output_index,
            sequence_number: stream_accumulator.upcoming_sequence_number(),
            code: call.code.clone(),
        },
        stream_accumulator,
        stream_sender,
    )
    .await?;
    emit_code_interpreter_event(
        &CodeInterpreterCallStreamEvent::Interpreting {
            item_id,
            output_index,
            sequence_number: stream_accumulator.upcoming_sequence_number(),
        },
        stream_accumulator,
        stream_sender,
    )
    .await?;
    Ok(())
}

pub(in crate::executor) async fn emit_response_start_events(
    payload: &ResponsePayload,
    stream_accumulator: &mut GatewayStreamAccumulator,
    stream_sender: &tokio::sync::mpsc::Sender<StreamEvent>,
) -> ExecutorResult<()> {
    let mut response = payload.clone();
    "in_progress".clone_into(&mut response.status);
    response.output.clear();
    response.usage = None;
    let response = serialize_to_value(&response).map_err(ExecutorError::JsonError)?;
    for event_type in [SSEEventType::ResponseCreated, SSEEventType::ResponseInProgress] {
        let mut event = synthetic_event(event_type, [("response".to_owned(), response.clone())])?;
        emit_gateway_event(&mut event, stream_accumulator, stream_sender).await?;
    }
    Ok(())
}

async fn emit_gateway_added_event(
    output_index: u32,
    output_item: &OutputItem,
    stream_accumulator: &mut GatewayStreamAccumulator,
    stream_sender: &tokio::sync::mpsc::Sender<StreamEvent>,
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
    emit_gateway_event(&mut added_event, stream_accumulator, stream_sender).await?;
    Ok(())
}

pub(in crate::executor) async fn emit_gateway_start_events<'a>(
    plans: impl IntoIterator<Item = &'a GatewayEventPlan>,
    stream_accumulator: &mut GatewayStreamAccumulator,
    stream_sender: &tokio::sync::mpsc::Sender<StreamEvent>,
) -> ExecutorResult<()> {
    for plan in plans {
        let Some(output_item) = &plan.started_output else {
            continue;
        };
        emit_gateway_added_event(plan.output_index, output_item, stream_accumulator, stream_sender).await?;
        match output_item {
            OutputItem::WebSearchCall(web_search_call) => {
                let mut in_progress_event = synthetic_event(
                    SSEEventType::WebSearchCallInProgress,
                    [
                        ("item_id".to_owned(), serde_json::json!(web_search_call.id)),
                        ("output_index".to_owned(), serde_json::json!(plan.output_index)),
                    ],
                )?;
                emit_gateway_event(&mut in_progress_event, stream_accumulator, stream_sender).await?;
                let mut searching_event = synthetic_event(
                    SSEEventType::WebSearchCallSearching,
                    [
                        ("item_id".to_owned(), serde_json::json!(web_search_call.id)),
                        ("output_index".to_owned(), serde_json::json!(plan.output_index)),
                    ],
                )?;
                emit_gateway_event(&mut searching_event, stream_accumulator, stream_sender).await?;
            }
            OutputItem::McpCall(mcp_call) => {
                let mut in_progress_event = synthetic_event(
                    SSEEventType::McpCallInProgress,
                    [
                        ("item_id".to_owned(), serde_json::json!(mcp_call.id)),
                        ("output_index".to_owned(), serde_json::json!(plan.output_index)),
                    ],
                )?;
                emit_gateway_event(&mut in_progress_event, stream_accumulator, stream_sender).await?;
                let arguments = plan.arguments.as_deref().unwrap_or_default();
                let mut arguments_delta_event = synthetic_event(
                    SSEEventType::McpCallArgumentsDelta,
                    [
                        ("delta".to_owned(), serde_json::json!(arguments)),
                        ("item_id".to_owned(), serde_json::json!(mcp_call.id)),
                        ("output_index".to_owned(), serde_json::json!(plan.output_index)),
                    ],
                )?;
                emit_gateway_event(&mut arguments_delta_event, stream_accumulator, stream_sender).await?;
                let mut arguments_done_event = synthetic_event(
                    SSEEventType::McpCallArgumentsDone,
                    [
                        ("arguments".to_owned(), serde_json::json!(arguments)),
                        ("item_id".to_owned(), serde_json::json!(mcp_call.id)),
                        ("output_index".to_owned(), serde_json::json!(plan.output_index)),
                    ],
                )?;
                emit_gateway_event(&mut arguments_done_event, stream_accumulator, stream_sender).await?;
            }
            OutputItem::McpListTools(list_tools) => {
                let mut in_progress_event = synthetic_event(
                    SSEEventType::McpListToolsInProgress,
                    [
                        ("item_id".to_owned(), serde_json::json!(list_tools.id)),
                        ("output_index".to_owned(), serde_json::json!(plan.output_index)),
                    ],
                )?;
                emit_gateway_event(&mut in_progress_event, stream_accumulator, stream_sender).await?;
            }
            OutputItem::CodeInterpreterCall(code_interpreter_call) => {
                emit_code_interpreter_start_events(
                    code_interpreter_call,
                    plan.output_index,
                    stream_accumulator,
                    stream_sender,
                )
                .await?;
            }
            OutputItem::Message(_)
            | OutputItem::FunctionCall(_)
            | OutputItem::ToolSearchCall(_)
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
    stream_accumulator: &mut GatewayStreamAccumulator,
    stream_sender: &tokio::sync::mpsc::Sender<StreamEvent>,
) -> ExecutorResult<()> {
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
            OutputItem::CodeInterpreterCall(_) | OutputItem::Compaction(_) | OutputItem::ShellCall(_) => None,
            OutputItem::Message(_)
            | OutputItem::FunctionCall(_)
            | OutputItem::ToolSearchCall(_)
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
                    sequence_number: stream_accumulator.upcoming_sequence_number(),
                },
                stream_accumulator,
                stream_sender,
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
            emit_gateway_event(&mut completed_event, stream_accumulator, stream_sender).await?;
        }
        let mut done_event = synthetic_event(
            SSEEventType::OutputItemDone,
            [
                ("output_index".to_owned(), serde_json::json!(output_index)),
                ("item".to_owned(), item),
            ],
        )?;
        emit_gateway_event(&mut done_event, stream_accumulator, stream_sender).await?;
    }
    Ok(())
}

/// One round's deferred upstream frames, bucketed by the model's output index.
pub(in crate::executor) struct DeferredRoundEvents {
    by_output: Vec<Vec<EventFrame>>,
    /// Frames without a usable output index are relayed after every item.
    remaining: Vec<EventFrame>,
}

impl DeferredRoundEvents {
    pub(in crate::executor) fn bucket(deferred: Vec<EventFrame>, item_count: usize) -> Self {
        let mut by_output = Vec::with_capacity(item_count);
        by_output.resize_with(item_count, Vec::new);
        let mut remaining = Vec::new();
        for frame in deferred {
            let Some(output_index) = frame
                .wire
                .output_index
                .and_then(|index| usize::try_from(index).ok())
                .filter(|index| *index < item_count)
            else {
                remaining.push(frame);
                continue;
            };
            by_output[output_index].push(frame);
        }
        Self { by_output, remaining }
    }
}

/// Relays one round's deferred frames in output order, emitting each gateway
/// call's lifecycle at its public index. Omitted refused calls have no public
/// item (ingestion suppresses gateway call frames) and shift later items so
/// public indexes stay contiguous. The first `initial_event_run_len` calls had
/// their start events emitted before execution.
pub(in crate::executor) async fn relay_round_events(
    scheduler: &GatewayScheduler,
    results: &[GatewayCallResult],
    deferred: DeferredRoundEvents,
    request: &RequestContext,
    stream: (&mut GatewayStreamAccumulator, &tokio::sync::mpsc::Sender<StreamEvent>),
    output_offset: usize,
    initial_event_run_len: usize,
) -> ExecutorResult<()> {
    let (stream_accumulator, stream_sender) = stream;
    let DeferredRoundEvents { by_output, remaining } = deferred;
    for (index, mut output_events) in by_output.into_iter().enumerate() {
        let Some(public_index) = scheduler.public_item_index(index) else {
            debug_assert!(output_events.is_empty(), "omitted call has frames");
            continue;
        };
        for frame in &mut output_events {
            frame.wire.output_index = u64::try_from(public_index).ok();
        }
        if let Some(call_index) = scheduler.call_index_for_item(index) {
            let plan = scheduler
                .event_plan(call_index)
                .expect("scheduled call index always has an event plan");
            let result = std::slice::from_ref(&results[call_index]);
            if call_index >= initial_event_run_len {
                emit_gateway_start_events(std::iter::once(plan), stream_accumulator, stream_sender).await?;
            }
            emit_gateway_completed_events(result, std::iter::once(plan), stream_accumulator, stream_sender).await?;
        }
        emit_deferred_stream_events(output_events, request, stream_accumulator, stream_sender, output_offset).await?;
    }
    emit_deferred_stream_events(remaining, request, stream_accumulator, stream_sender, output_offset).await
}
