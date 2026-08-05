use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::StreamExt;
use serde_json::Value;

use crate::events::{EventFrame, EventPayload, SSEEventType, SSEItemType, WireEvent};
use crate::executor::accumulator::ResponseAccumulator;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::{GatewayStreamAccumulator, StreamEvent, emit_sse_frame};
use crate::executor::inference::{call_inference, fetch_response_json};
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::tool::ToolRegistry;
use crate::types::request_response::ResponsePayload;
use crate::utils::common::serialize_to_string;

struct StreamEmitContext<'a> {
    request: &'a RequestContext,
    registry: &'a ToolRegistry,
    sender: &'a tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    accumulator: &'a mut GatewayStreamAccumulator,
    output_offset: usize,
}

pub(super) struct StreamPayload {
    pub(super) payload: ResponsePayload,
    pub(super) deferred_events: Vec<EventFrame>,
}

pub(super) async fn fetch_blocking_payload(
    ctx: &RequestContext,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
) -> ExecutorResult<ResponsePayload> {
    let url = exec_ctx.responses_url();
    // Non-streaming request: stream=false -> full JSON body -> from_json.
    let upstream_request = ctx.enriched_request.to_upstream_request(false)?;
    let upstream_json = serialize_to_string(&upstream_request).map_err(ExecutorError::JsonError)?;

    let body = fetch_response_json(upstream_json, &url, &exec_ctx.client, auth).await?;

    let acc = ResponseAccumulator::from_json(&body, ctx.conversation_id.as_deref())?;
    let mut payload = acc.finalize(
        &ctx.enriched_request.model,
        ctx.original_request.previous_response_id.as_deref(),
        ctx.original_request.instructions.as_deref(),
    );
    ctx.inject_ids(&mut payload);

    Ok(payload)
}

pub(super) async fn fetch_stream_payload(
    ctx: &RequestContext,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
    registry: &ToolRegistry,
    mut stream: Option<(
        &mut GatewayStreamAccumulator,
        &tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    )>,
    output_offset: usize,
) -> ExecutorResult<StreamPayload> {
    let url = exec_ctx.responses_url();
    let upstream_request = ctx.enriched_request.to_upstream_request(true)?;
    let upstream_json = serialize_to_string(&upstream_request).map_err(ExecutorError::JsonError)?;
    let mut line_stream = Box::pin(call_inference(
        upstream_json,
        url,
        Arc::clone(&exec_ctx.client),
        auth.map(str::to_owned),
        exec_ctx.streaming_timeout,
    ));
    let mut acc = ResponseAccumulator::new(ctx.response_id.clone(), ctx.conversation_id.clone());
    let mut hidden_gateway_item_ids = HashSet::new();
    let mut pending_unnamed_function_events = HashMap::<String, Vec<EventFrame>>::new();
    let mut defer_from_output_index = None;
    let mut deferred_events = Vec::new();
    while let Some(line_result) = line_stream.next().await {
        let line = line_result?;
        if let Some(frame) = acc.process_sse_line(&line) {
            log_upstream_failure(&frame, &ctx.response_id);
            if let Some((accumulator, sender)) = stream.as_mut() {
                let mut emit_ctx = StreamEmitContext {
                    request: ctx,
                    registry,
                    sender,
                    accumulator,
                    output_offset,
                };
                emit_upstream_stream_event(
                    frame,
                    &mut emit_ctx,
                    &mut hidden_gateway_item_ids,
                    &mut pending_unnamed_function_events,
                    &mut defer_from_output_index,
                    &mut deferred_events,
                )?;
            }
        }
    }
    acc.finish_stream();
    let mut payload = acc.finalize(
        &ctx.enriched_request.model,
        ctx.original_request.previous_response_id.as_deref(),
        ctx.original_request.instructions.as_deref(),
    );
    ctx.inject_ids(&mut payload);
    Ok(StreamPayload {
        payload,
        deferred_events,
    })
}

fn log_upstream_failure(frame: &EventFrame, gateway_response_id: &str) {
    if frame.event_type != SSEEventType::ResponseFailed {
        return;
    }

    let response = frame.wire.rest.get("response").unwrap_or(&Value::Null);
    let error = &response["error"];
    let error_code = error.get("code").and_then(Value::as_str).unwrap_or_default();
    let error_message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or_default();
    let incomplete_reason = response["incomplete_details"]
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();

    tracing::warn!(
        response_id = %gateway_response_id,
        upstream_response_id = response["id"].as_str().unwrap_or_default(),
        error_code,
        error_message,
        incomplete_reason,
        "upstream response failed"
    );
}

fn emit_upstream_stream_event(
    frame: EventFrame,
    emit_ctx: &mut StreamEmitContext<'_>,
    hidden_gateway_item_ids: &mut HashSet<String>,
    pending_unnamed_function_events: &mut HashMap<String, Vec<EventFrame>>,
    defer_from_output_index: &mut Option<u64>,
    deferred_events: &mut Vec<EventFrame>,
) -> ExecutorResult<()> {
    defer_after_gateway_call(&frame, emit_ctx.registry, defer_from_output_index);
    if should_hide_upstream_event(
        frame.event_type,
        &frame.payload,
        emit_ctx.registry,
        hidden_gateway_item_ids,
    ) || is_terminal_response_event(frame.event_type)
    {
        drop_pending_function_events(&frame.payload, pending_unnamed_function_events);
        return Ok(());
    }
    let Some(frame) = defer_or_flush_function_event(
        frame,
        emit_ctx,
        hidden_gateway_item_ids,
        pending_unnamed_function_events,
        defer_from_output_index,
        deferred_events,
    )?
    else {
        return Ok(());
    };

    emit_or_defer_stream_frame(frame, emit_ctx, *defer_from_output_index, deferred_events)
}

pub(super) fn emit_deferred_stream_events(
    deferred_events: Vec<EventFrame>,
    request: &RequestContext,
    registry: &ToolRegistry,
    accumulator: &mut GatewayStreamAccumulator,
    sender: &tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    output_offset: usize,
) -> ExecutorResult<()> {
    let mut emit_ctx = StreamEmitContext {
        request,
        registry,
        sender,
        accumulator,
        output_offset,
    };
    for mut frame in deferred_events {
        emit_stream_frame(&mut frame, &mut emit_ctx)?;
    }
    Ok(())
}

fn defer_after_gateway_call(frame: &EventFrame, registry: &ToolRegistry, defer_from_output_index: &mut Option<u64>) {
    let EventPayload::OutputItemAdded {
        item_type: SSEItemType::FunctionCall,
        name: Some(name),
        ..
    } = &frame.payload
    else {
        return;
    };
    if registry.is_gateway_owned_name(name) {
        record_first_hidden_gateway_output_index(frame, defer_from_output_index);
    }
}

fn record_first_hidden_gateway_output_index(frame: &EventFrame, defer_from_output_index: &mut Option<u64>) {
    let Some(output_index) = frame.wire.output_index else {
        return;
    };
    if defer_from_output_index.is_none_or(|first_hidden_index| output_index < first_hidden_index) {
        *defer_from_output_index = Some(output_index);
    }
}

fn should_defer_stream_event(frame: &EventFrame, defer_from_output_index: Option<u64>) -> bool {
    defer_from_output_index.is_some_and(|first_hidden_index| {
        frame
            .wire
            .output_index
            .is_some_and(|output_index| output_index >= first_hidden_index)
    })
}

fn emit_stream_frame(frame: &mut EventFrame, emit_ctx: &mut StreamEmitContext<'_>) -> ExecutorResult<()> {
    apply_context_response_ids(&mut frame.wire, emit_ctx.request);
    emit_ctx.registry.restore_stream_event_wire(&mut frame.wire);
    if emit_ctx.accumulator.process_event(frame, emit_ctx.output_offset) {
        emit_sse_frame(emit_ctx.sender, frame)?;
    }
    Ok(())
}

fn emit_or_defer_stream_frame(
    mut frame: EventFrame,
    emit_ctx: &mut StreamEmitContext<'_>,
    defer_from_output_index: Option<u64>,
    deferred_events: &mut Vec<EventFrame>,
) -> ExecutorResult<()> {
    if should_defer_stream_event(&frame, defer_from_output_index) {
        deferred_events.push(frame);
        return Ok(());
    }
    emit_stream_frame(&mut frame, emit_ctx)
}

fn defer_or_flush_function_event(
    frame: EventFrame,
    emit_ctx: &mut StreamEmitContext<'_>,
    hidden_gateway_item_ids: &mut HashSet<String>,
    pending_unnamed_function_events: &mut HashMap<String, Vec<EventFrame>>,
    defer_from_output_index: &mut Option<u64>,
    deferred_events: &mut Vec<EventFrame>,
) -> ExecutorResult<Option<EventFrame>> {
    match &frame.payload {
        EventPayload::OutputItemAdded {
            item_id,
            item_type,
            name: None,
            ..
        } if *item_type == SSEItemType::FunctionCall => {
            let item_id = item_id.clone();
            pending_unnamed_function_events.entry(item_id).or_default().push(frame);
            Ok(None)
        }
        EventPayload::FunctionCallArgsDelta { item_id, .. }
            if pending_unnamed_function_events.contains_key(item_id) =>
        {
            let item_id = item_id.clone();
            pending_unnamed_function_events.entry(item_id).or_default().push(frame);
            Ok(None)
        }
        EventPayload::FunctionCallArgsDone { item_id, name, .. } => {
            if emit_ctx.registry.is_gateway_owned_name(name) {
                hidden_gateway_item_ids.insert(item_id.clone());
                record_first_hidden_gateway_output_index(&frame, defer_from_output_index);
                pending_unnamed_function_events.remove(item_id);
                return Ok(None);
            }
            flush_pending_function_events(
                item_id,
                emit_ctx,
                pending_unnamed_function_events,
                *defer_from_output_index,
                deferred_events,
            )?;
            Ok(Some(frame))
        }
        EventPayload::OutputItemDone {
            item_id,
            item_type,
            item,
            ..
        } if *item_type == SSEItemType::FunctionCall => {
            if item
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| emit_ctx.registry.is_gateway_owned_name(name))
            {
                hidden_gateway_item_ids.insert(item_id.clone());
                record_first_hidden_gateway_output_index(&frame, defer_from_output_index);
                pending_unnamed_function_events.remove(item_id);
                return Ok(None);
            }
            flush_pending_function_events(
                item_id,
                emit_ctx,
                pending_unnamed_function_events,
                *defer_from_output_index,
                deferred_events,
            )?;
            Ok(Some(frame))
        }
        _ => Ok(Some(frame)),
    }
}

fn flush_pending_function_events(
    item_id: &str,
    emit_ctx: &mut StreamEmitContext<'_>,
    pending_unnamed_function_events: &mut HashMap<String, Vec<EventFrame>>,
    defer_from_output_index: Option<u64>,
    deferred_events: &mut Vec<EventFrame>,
) -> ExecutorResult<()> {
    let Some(frames) = pending_unnamed_function_events.remove(item_id) else {
        return Ok(());
    };
    for frame in frames {
        emit_or_defer_stream_frame(frame, emit_ctx, defer_from_output_index, deferred_events)?;
    }
    Ok(())
}

fn drop_pending_function_events(
    payload: &EventPayload,
    pending_unnamed_function_events: &mut HashMap<String, Vec<EventFrame>>,
) {
    match payload {
        EventPayload::OutputItemDone { item_id, .. }
        | EventPayload::FunctionCallArgsDelta { item_id, .. }
        | EventPayload::FunctionCallArgsDone { item_id, .. } => {
            pending_unnamed_function_events.remove(item_id);
        }
        EventPayload::OutputItemAdded { .. }
        | EventPayload::TextDelta { .. }
        | EventPayload::TextDone { .. }
        | EventPayload::CustomToolCallInputDelta { .. }
        | EventPayload::CustomToolCallInputDone { .. }
        | EventPayload::ReasoningDelta { .. }
        | EventPayload::ReasoningDone { .. }
        | EventPayload::Response { .. }
        | EventPayload::Raw(_)
        | EventPayload::None => {}
    }
}

fn should_hide_upstream_event(
    event_type: SSEEventType,
    payload: &EventPayload,
    registry: &ToolRegistry,
    hidden_gateway_item_ids: &mut HashSet<String>,
) -> bool {
    match (event_type, payload) {
        (
            SSEEventType::OutputItemAdded,
            EventPayload::OutputItemAdded {
                item_id,
                item_type,
                name: Some(name),
                ..
            },
        ) if *item_type == SSEItemType::FunctionCall && registry.is_gateway_owned_name(name) => {
            hidden_gateway_item_ids.insert(item_id.clone());
            true
        }
        (SSEEventType::OutputItemDone, EventPayload::OutputItemDone { item_id, item_type, .. })
            if *item_type == SSEItemType::FunctionCall && hidden_gateway_item_ids.contains(item_id) =>
        {
            true
        }
        (
            SSEEventType::FunctionCallArgumentsDelta | SSEEventType::FunctionCallArgumentsDone,
            EventPayload::FunctionCallArgsDelta { item_id, .. } | EventPayload::FunctionCallArgsDone { item_id, .. },
        ) => hidden_gateway_item_ids.contains(item_id),
        _ => false,
    }
}

fn is_terminal_response_event(event_type: SSEEventType) -> bool {
    matches!(
        event_type,
        SSEEventType::ResponseCompleted | SSEEventType::ResponseFailed | SSEEventType::ResponseIncomplete
    )
}

fn apply_context_response_ids(wire: &mut WireEvent, ctx: &RequestContext) {
    let Some(response) = wire.rest.get_mut("response").and_then(Value::as_object_mut) else {
        return;
    };
    response.insert("id".to_owned(), Value::String(ctx.response_id.clone()));
    if let Some(previous_response_id) = &ctx.original_request.previous_response_id {
        response.insert(
            "previous_response_id".to_owned(),
            Value::String(previous_response_id.clone()),
        );
    }
    if let Some(conversation_id) = &ctx.conversation_id {
        response.insert("conversation".to_owned(), Value::String(conversation_id.clone()));
    }
}
