use super::{ToolEvent, ToolTranslator, TranslationContext, ensure_function_call_size, resolved_output_index};
use crate::events::{EventFrame, EventPayload, SSEEventType, SSEItemType};
use crate::executor::accumulator::AccumulatedFunctionCall;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::synthetic_event;
use crate::tool::{ToolType, tool_search};
use crate::types::event::ResponseStatus;
use crate::types::io::OutputItem;
use crate::utils::common::{serialize_to_string, serialize_to_value};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolSearchCallSource {
    Synthetic,
    Native,
}

#[derive(Debug)]
struct ToolSearchIdentity {
    source: ToolSearchCallSource,
    output_index: u32,
    call_id: String,
}

#[derive(Debug, Default)]
pub(super) struct ToolSearchTranslator {
    internal_item_id: Option<String>,
    output_index: u32,
}

impl ToolTranslator for ToolSearchTranslator {
    fn translate(
        &mut self,
        event: ToolEvent<'_>,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Vec<EventFrame>> {
        if matches!(event, ToolEvent::Delta(_)) {
            if let Some(call) = call {
                ensure_function_call_size(call.arguments())?;
            }
            return Ok(Vec::new());
        }
        let call = call.ok_or_else(|| ExecutorError::Tool(tool_search::invalid_upstream_search_call()))?;
        match event {
            ToolEvent::Added {
                name,
                output_index,
                original,
                ..
            } => {
                if let Some(original) = original.as_ref() {
                    validate_tool_search_added(original, name)?;
                }
                let public = tool_search::started_public_call(call.item)?;
                self.internal_item_id = Some(call.item.id.clone());
                self.output_index = output_index;
                Ok(vec![tool_search_frame(
                    SSEEventType::OutputItemAdded,
                    output_index,
                    &public,
                )?])
            }
            ToolEvent::ArgumentsDone(_) => {
                ensure_function_call_size(call.arguments())?;
                tool_search::validate_public_arguments(call.arguments())?;
                Ok(Vec::new())
            }
            ToolEvent::Done(original) => {
                ensure_function_call_size(call.arguments())?;
                // The assembled snapshot is finalized, but an aborted provider call may still be unfinished.
                let completed = matches!(&original.payload, EventPayload::OutputItemDone { item, .. }
                    if item.get("status").and_then(Value::as_str) == Some("completed"));
                if completed {
                    let public = tool_search::completed_public_call(call.item)?;
                    self.internal_item_id = None;
                    Ok(vec![tool_search_frame(
                        SSEEventType::OutputItemDone,
                        self.output_index,
                        &public,
                    )?])
                } else {
                    Ok(Vec::new())
                }
            }
            ToolEvent::Delta(_) => unreachable!("handled above"),
        }
    }

    fn unfinished_tool_search_item_id(&self) -> Option<&str> {
        self.internal_item_id.as_deref()
    }
}

#[derive(Debug, Default)]
pub(super) struct ToolSearchStreamState {
    active_native_tool_search: HashSet<u32>,
    tool_search_identity: Option<ToolSearchIdentity>,
}

impl ToolSearchStreamState {
    pub(super) fn start_synthetic(&mut self, output_index: u32, call_id: &str) -> ExecutorResult<()> {
        self.start_tool_search_call(ToolSearchCallSource::Synthetic, output_index, call_id)
    }

    pub(super) fn has_unfinished_native(&self) -> bool {
        !self.active_native_tool_search.is_empty()
    }

    pub(super) fn validate_frame(context: &TranslationContext, frame: &EventFrame) -> ExecutorResult<()> {
        let lifecycle_name = match &frame.payload {
            EventPayload::OutputItemAdded {
                item_type: SSEItemType::FunctionCall,
                name: Some(name),
                ..
            }
            | EventPayload::FunctionCallArgsDone { name, .. } => Some(name.as_str()),
            EventPayload::OutputItemDone {
                item_type: SSEItemType::FunctionCall,
                item,
                ..
            } => item.get("name").and_then(Value::as_str),
            _ => None,
        };
        if let Some(name) = lifecycle_name {
            tool_search::ensure_function_is_available(context.is_withheld_function(name))?;
        }

        match &frame.payload {
            EventPayload::OutputItemDone {
                item_type: SSEItemType::FunctionCall,
                item,
                ..
            } if item
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| context.tool_type(name) == ToolType::ToolSearch) =>
            {
                tool_search::strict_function_call(item)?;
            }
            EventPayload::Response { .. }
                if matches!(
                    frame.event_type,
                    SSEEventType::ResponseCompleted | SSEEventType::ResponseFailed | SSEEventType::ResponseIncomplete
                ) =>
            {
                Self::validate_terminal_output(context, frame)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn validate_terminal_output(context: &TranslationContext, frame: &EventFrame) -> ExecutorResult<()> {
        let Some(output) = frame
            .wire
            .rest
            .get("response")
            .and_then(|response| response.get("output"))
            .and_then(Value::as_array)
        else {
            return Ok(());
        };
        let completed = frame.event_type == SSEEventType::ResponseCompleted;
        let mut saw_tool_search_call = false;
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("function_call") => {
                    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                    tool_search::ensure_function_is_available(context.is_withheld_function(name))?;
                    if context.tool_type(name) == ToolType::ToolSearch {
                        if saw_tool_search_call {
                            return Err(tool_search::invalid_upstream_search_call().into());
                        }
                        saw_tool_search_call = true;
                        let call = tool_search::strict_function_call(item)?;
                        ensure_function_call_size(&call.arguments)?;
                        if completed && call.status != crate::types::event::MessageStatus::Completed {
                            return Err(tool_search::invalid_upstream_search_call().into());
                        }
                    }
                }
                Some("tool_search_call") => {
                    if saw_tool_search_call {
                        return Err(tool_search::invalid_upstream_search_call().into());
                    }
                    saw_tool_search_call = true;
                    let call = tool_search::strict_native_call(item.clone())?;
                    let arguments = serialize_to_string(&call.arguments).map_err(ExecutorError::JsonError)?;
                    ensure_function_call_size(&arguments)?;
                    if completed && call.status != crate::types::tools::ToolSearchStatus::Completed {
                        return Err(tool_search::invalid_upstream_search_call().into());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) fn track_native_tool_search(&mut self, frame: &EventFrame) -> ExecutorResult<()> {
        match &frame.payload {
            EventPayload::OutputItemAdded {
                item_type: SSEItemType::ToolSearchCall,
                output_index,
                ..
            } => {
                let call = validate_native_tool_search_frame(frame)?;
                self.start_tool_search_call(
                    ToolSearchCallSource::Native,
                    resolved_output_index(*output_index)?,
                    &call.call_id,
                )?;
                self.active_native_tool_search
                    .insert(resolved_output_index(*output_index)?);
            }
            EventPayload::OutputItemDone {
                item_type: SSEItemType::ToolSearchCall,
                output_index,
                ..
            } => {
                let call = validate_native_tool_search_frame(frame)?;
                self.observe_tool_search_call(
                    ToolSearchCallSource::Native,
                    resolved_output_index(*output_index)?,
                    &call.call_id,
                )?;
                if call.status == crate::types::tools::ToolSearchStatus::Completed {
                    self.active_native_tool_search
                        .remove(&resolved_output_index(*output_index)?);
                } else {
                    self.active_native_tool_search
                        .insert(resolved_output_index(*output_index)?);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn start_tool_search_call(
        &mut self,
        source: ToolSearchCallSource,
        output_index: u32,
        call_id: &str,
    ) -> ExecutorResult<()> {
        if self.tool_search_identity.is_some() {
            return Err(tool_search::invalid_upstream_search_call().into());
        }
        self.tool_search_identity = Some(ToolSearchIdentity {
            source,
            output_index,
            call_id: call_id.to_owned(),
        });
        Ok(())
    }

    fn observe_tool_search_call(
        &mut self,
        source: ToolSearchCallSource,
        output_index: u32,
        call_id: &str,
    ) -> ExecutorResult<()> {
        match self.tool_search_identity.as_ref() {
            Some(identity)
                if identity.source != source
                    || identity.output_index != output_index
                    || identity.call_id != call_id =>
            {
                Err(tool_search::invalid_upstream_search_call().into())
            }
            Some(_) => Ok(()),
            None => {
                self.tool_search_identity = Some(ToolSearchIdentity {
                    source,
                    output_index,
                    call_id: call_id.to_owned(),
                });
                Ok(())
            }
        }
    }
}

fn validate_tool_search_added(frame: &EventFrame, name: &str) -> ExecutorResult<()> {
    let Some(item) = frame.wire.rest.get("item") else {
        return Err(tool_search::invalid_upstream_search_call().into());
    };
    let mut item = item.clone();
    match item.get("name") {
        None | Some(Value::Null) => {
            item.as_object_mut()
                .ok_or_else(tool_search::invalid_upstream_search_call)?
                .insert("name".to_owned(), Value::String(name.to_owned()));
        }
        Some(Value::String(_)) => {}
        Some(_) => return Err(tool_search::invalid_upstream_search_call().into()),
    }
    tool_search::strict_started_function(&item)?;
    Ok(())
}

fn validate_native_tool_search_frame(frame: &EventFrame) -> ExecutorResult<crate::types::io::ToolSearchCall> {
    let item = frame
        .wire
        .rest
        .get("item")
        .cloned()
        .ok_or_else(tool_search::invalid_upstream_search_call)?;
    let call = tool_search::strict_native_call(item)?;
    let arguments = serialize_to_string(&call.arguments).map_err(ExecutorError::JsonError)?;
    ensure_function_call_size(&arguments)?;
    if frame.event_type == SSEEventType::OutputItemAdded
        && (call.status != crate::types::tools::ToolSearchStatus::InProgress
            || !matches!(&call.arguments, Value::Object(arguments) if arguments.is_empty()))
    {
        return Err(tool_search::invalid_upstream_search_call().into());
    }
    Ok(call)
}

fn tool_search_frame(
    event_type: SSEEventType,
    output_index: u32,
    call: &crate::types::io::ToolSearchCall,
) -> ExecutorResult<EventFrame> {
    let item = serialize_to_value(&OutputItem::ToolSearchCall(call.clone())).map_err(ExecutorError::JsonError)?;
    let mut frame = synthetic_event(event_type, [("item".to_owned(), item)])?;
    frame.wire.output_index = Some(u64::from(output_index));
    Ok(frame)
}

/// Normalize native and synthetic upstream tool-search calls into the
/// canonical public output item.
pub(super) fn normalize_response_output(
    context: &TranslationContext,
    output: &mut Vec<OutputItem>,
    status: ResponseStatus,
    unfinished_stream_item_ids: &HashSet<String>,
) -> ExecutorResult<()> {
    let discard_unidentified_unfinished = matches!(status, ResponseStatus::Error | ResponseStatus::Incomplete);
    let mut normalized = Vec::with_capacity(output.len());
    let mut saw_tool_search_call = false;
    for item in std::mem::take(output) {
        match item {
            OutputItem::FunctionCall(call) => {
                tool_search::ensure_function_is_available(context.is_withheld_function(&call.name))?;
                if context.tool_type(&call.name) == ToolType::ToolSearch {
                    if saw_tool_search_call {
                        return Err(tool_search::invalid_upstream_search_call().into());
                    }
                    saw_tool_search_call = true;
                    if let Some(public) = tool_search::project_synthetic_call(
                        &call,
                        status,
                        unfinished_stream_item_ids.contains(&call.id),
                    )? {
                        normalized.push(OutputItem::ToolSearchCall(public));
                    }
                } else if !(discard_unidentified_unfinished && unfinished_stream_item_ids.contains(&call.id)) {
                    normalized.push(OutputItem::FunctionCall(call));
                }
            }
            OutputItem::ToolSearchCall(call) => {
                if saw_tool_search_call {
                    return Err(tool_search::invalid_upstream_search_call().into());
                }
                saw_tool_search_call = true;
                if let Some(public) = tool_search::project_native_call(&call, status)? {
                    normalized.push(OutputItem::ToolSearchCall(public));
                }
            }
            item => normalized.push(item),
        }
    }
    *output = normalized;
    Ok(())
}

/// Public tool-search catalog, stripped of request-only credentials and discovery details.
pub(super) fn public_response_tools(
    tools: Option<Vec<crate::types::tools::ResponsesTool>>,
) -> Option<Vec<crate::types::tools::ResponsesTool>> {
    tools.map(|mut tools| {
        for tool in &mut tools {
            tool.sanitize_for_persistence();
        }
        tools
    })
}

pub(super) fn restore_response_tools(
    wire: &mut crate::events::WireEvent,
    tools: Option<&[crate::types::tools::ResponsesTool]>,
    tool_choice: Option<&crate::types::io::ToolChoice>,
) -> ExecutorResult<()> {
    let Some(response) = wire.rest.get_mut("response").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let Some(tools) = tools else {
        return Ok(());
    };
    if response.contains_key("tools") {
        response.insert(
            "tools".to_owned(),
            crate::utils::common::serialize_to_value(&tools)
                .map_err(|_| tool_search::invalid_upstream_search_call())?,
        );
    }
    if response.contains_key("tool_choice") {
        response.insert(
            "tool_choice".to_owned(),
            crate::utils::common::serialize_to_value(tool_choice.unwrap_or(&crate::types::io::ToolChoice::Auto))
                .map_err(crate::executor::error::ExecutorError::JsonError)?,
        );
    }
    Ok(())
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use crate::tool::ToolSearchHandler;
    use crate::types::request_response::RequestPayload;
    use serde_json::json;
    #[test]
    fn prepared_response_tools_remove_request_scoped_mcp_secrets_and_discovery() {
        let mut request: RequestPayload = serde_json::from_value(json!({
            "model": "test",
            "input": "find weather",
            "parallel_tool_calls": false,
            "tools": [
                {"type": "tool_search", "execution": "client"},
                {
                    "type": "mcp",
                    "server_label": "weather",
                    "server_url": "https://mcp.example.test/mcp",
                    "headers": {"Authorization": "Bearer header-secret"},
                    "authorization": "field-secret",
                    "_agentic_discovered_tools": [{
                        "server_label": "weather",
                        "tool_name": "forecast",
                        "internal_name": "mcp__weather__forecast",
                        "tool": {"name": "forecast", "inputSchema": {"type": "object"}}
                    }]
                }
            ]
        }))
        .expect("request shape");

        let state = ToolSearchHandler::prepare_request(&mut request, &[], false)
            .expect("tool-search preparation")
            .expect("active tool-search state");
        let tools = public_response_tools(Some(state.public_response_tools())).expect("active tools");
        let serialized = serde_json::to_value(tools).expect("public tools serialize");
        let serialized = serialized.to_string();

        for secret in [
            "header-secret",
            "field-secret",
            "mcp__weather__forecast",
            "_agentic_discovered_tools",
        ] {
            assert!(!serialized.contains(secret));
        }
    }
}
