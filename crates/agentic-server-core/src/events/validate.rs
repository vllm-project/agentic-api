use serde::Deserialize;
use serde_json::{Map, Value};
use thiserror::Error;

use super::{EventFrame, SSEEventType};
use crate::types::io::OutputItem;

#[derive(Debug, Error)]
#[error("{0}")]
pub(crate) struct EventError(String);

/// Validates the stateless wire-format requirements of one normalized frame.
pub(crate) fn validate_frame(frame: &EventFrame) -> Result<(), EventError> {
    let event_name = frame
        .wire
        .event_type
        .as_deref()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid("streaming event has no valid 'type'"))?;

    match frame.event_type {
        SSEEventType::ResponseCreated
        | SSEEventType::ResponseInProgress
        | SSEEventType::ResponseCompleted
        | SSEEventType::ResponseFailed
        | SSEEventType::ResponseIncomplete => validate_response_event(frame, event_name),
        SSEEventType::OutputItemAdded => validate_output_item(frame, event_name, false),
        SSEEventType::OutputItemDone => validate_output_item(frame, event_name, true),
        SSEEventType::Other => Ok(()),
        event_type => {
            required_output_index(frame, event_name)?;
            required_str(&frame.wire.rest, "item_id", event_name)?;
            validate_event_fields(&frame.wire.rest, event_type, event_name)
        }
    }
}

pub(crate) fn expected_item_type(event_type: SSEEventType) -> &'static str {
    match event_type {
        SSEEventType::OutputTextDelta
        | SSEEventType::OutputTextDone
        | SSEEventType::ContentPartAdded
        | SSEEventType::ContentPartDone => "message",
        SSEEventType::FunctionCallArgumentsDelta | SSEEventType::FunctionCallArgumentsDone => "function_call",
        SSEEventType::CustomToolCallInputDelta | SSEEventType::CustomToolCallInputDone => "custom_tool_call",
        SSEEventType::ReasoningTextDelta
        | SSEEventType::ReasoningTextDone
        | SSEEventType::ReasoningPartAdded
        | SSEEventType::ReasoningPartDone
        | SSEEventType::ReasoningSummaryTextDelta
        | SSEEventType::ReasoningSummaryTextDone => "reasoning",
        SSEEventType::FileSearchCallSearching | SSEEventType::FileSearchCallCompleted => "file_search_call",
        SSEEventType::WebSearchCallInProgress
        | SSEEventType::WebSearchCallSearching
        | SSEEventType::WebSearchCallCompleted => "web_search_call",
        SSEEventType::McpCallInProgress
        | SSEEventType::McpCallArgumentsDelta
        | SSEEventType::McpCallArgumentsDone
        | SSEEventType::McpCallCompleted
        | SSEEventType::McpCallFailed => "mcp_call",
        SSEEventType::McpListToolsInProgress
        | SSEEventType::McpListToolsCompleted
        | SSEEventType::McpListToolsFailed => "mcp_list_tools",
        SSEEventType::ResponseCreated
        | SSEEventType::ResponseInProgress
        | SSEEventType::ResponseCompleted
        | SSEEventType::ResponseFailed
        | SSEEventType::ResponseIncomplete
        | SSEEventType::OutputItemAdded
        | SSEEventType::OutputItemDone
        | SSEEventType::Other => unreachable!("only item events are classified"),
    }
}

pub(crate) fn output_item_identity<'a>(
    item: &'a Map<String, Value>,
    owner: &str,
) -> Result<(&'a str, &'a str), EventError> {
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .or_else(|| item.get("item_id").and_then(Value::as_str).filter(|id| !id.is_empty()))
        .ok_or_else(|| missing_field(owner, "id"))?;
    let item_type = required_str(item, "type", owner)?;
    ensure_supported_output_item_type(item_type)?;
    Ok((item_id, item_type))
}

pub(crate) fn ensure_supported_output_item_type(item_type: &str) -> Result<(), EventError> {
    if matches!(
        item_type,
        "message"
            | "function_call"
            | "custom_tool_call"
            | "web_search_call"
            | "mcp_call"
            | "mcp_list_tools"
            | "reasoning"
            | "compaction"
    ) {
        return Ok(());
    }
    Err(invalid(format!(
        "upstream output item type '{item_type}' is unsupported"
    )))
}

fn validate_response_event(frame: &EventFrame, event_name: &str) -> Result<(), EventError> {
    let response = required_object(&frame.wire.rest, "response", event_name)?;
    required_str(response, "id", "upstream response")?;
    let status = required_str(response, "status", "upstream response")?;
    let expected_status = match frame.event_type {
        SSEEventType::ResponseCreated | SSEEventType::ResponseInProgress => "in_progress",
        SSEEventType::ResponseCompleted => "completed",
        SSEEventType::ResponseFailed => "failed",
        SSEEventType::ResponseIncomplete => "incomplete",
        _ => return Ok(()),
    };
    if status == expected_status {
        return Ok(());
    }
    Err(invalid(format!(
        "upstream stream event '{event_name}' has status '{status}', expected '{expected_status}'"
    )))
}

fn validate_output_item(frame: &EventFrame, event_name: &str, complete: bool) -> Result<(), EventError> {
    required_output_index(frame, event_name)?;
    let item = required_object(&frame.wire.rest, "item", event_name)?;
    let (_, item_type) = output_item_identity(item, "output item")?;
    if !complete {
        return Ok(());
    }

    let mut canonical = Value::Object(item.clone());
    if canonical.get("id").and_then(Value::as_str).is_none_or(str::is_empty) {
        canonical["id"] = Value::String(required_str(item, "item_id", "output item")?.to_owned());
    }
    let output = OutputItem::deserialize(canonical)
        .map_err(|error| invalid(format!("upstream stream output item is invalid: {error}")))?;
    if matches!(output, OutputItem::Unknown) {
        return Err(invalid(format!(
            "upstream output item type '{item_type}' is unsupported"
        )));
    }
    Ok(())
}

fn validate_event_fields(
    event: &Map<String, Value>,
    event_type: SSEEventType,
    event_name: &str,
) -> Result<(), EventError> {
    match event_type {
        SSEEventType::OutputTextDelta
        | SSEEventType::OutputTextDone
        | SSEEventType::ContentPartAdded
        | SSEEventType::ContentPartDone
        | SSEEventType::ReasoningTextDelta
        | SSEEventType::ReasoningTextDone
        | SSEEventType::ReasoningPartAdded
        | SSEEventType::ReasoningPartDone => {
            required_u32(event, "content_index", event_name)?;
        }
        SSEEventType::ReasoningSummaryTextDelta | SSEEventType::ReasoningSummaryTextDone => {
            required_u32(event, "summary_index", event_name)?;
        }
        _ => {}
    }

    let required = match event_type {
        SSEEventType::OutputTextDelta
        | SSEEventType::FunctionCallArgumentsDelta
        | SSEEventType::CustomToolCallInputDelta
        | SSEEventType::ReasoningTextDelta
        | SSEEventType::ReasoningSummaryTextDelta
        | SSEEventType::McpCallArgumentsDelta => Some("delta"),
        SSEEventType::OutputTextDone | SSEEventType::ReasoningTextDone | SSEEventType::ReasoningSummaryTextDone => {
            Some("text")
        }
        SSEEventType::FunctionCallArgumentsDone | SSEEventType::McpCallArgumentsDone => Some("arguments"),
        SSEEventType::CustomToolCallInputDone => Some("input"),
        SSEEventType::ContentPartAdded
        | SSEEventType::ContentPartDone
        | SSEEventType::ReasoningPartAdded
        | SSEEventType::ReasoningPartDone => {
            required_object(event, "part", event_name)?;
            None
        }
        SSEEventType::ResponseCreated
        | SSEEventType::ResponseInProgress
        | SSEEventType::ResponseCompleted
        | SSEEventType::ResponseFailed
        | SSEEventType::ResponseIncomplete
        | SSEEventType::OutputItemAdded
        | SSEEventType::OutputItemDone
        | SSEEventType::FileSearchCallSearching
        | SSEEventType::FileSearchCallCompleted
        | SSEEventType::WebSearchCallInProgress
        | SSEEventType::WebSearchCallSearching
        | SSEEventType::WebSearchCallCompleted
        | SSEEventType::McpCallInProgress
        | SSEEventType::McpCallCompleted
        | SSEEventType::McpCallFailed
        | SSEEventType::McpListToolsInProgress
        | SSEEventType::McpListToolsCompleted
        | SSEEventType::McpListToolsFailed
        | SSEEventType::Other => None,
    };
    if let Some(field) = required {
        required_string(event, field, event_name)?;
    }
    Ok(())
}

fn required_output_index(frame: &EventFrame, owner: &str) -> Result<u32, EventError> {
    frame
        .wire
        .output_index
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| missing_field(owner, "output_index"))
}

fn required_str<'a>(value: &'a Map<String, Value>, field: &str, owner: &str) -> Result<&'a str, EventError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| missing_field(owner, field))
}

fn required_string<'a>(value: &'a Map<String, Value>, field: &str, owner: &str) -> Result<&'a str, EventError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| missing_field(owner, field))
}

fn required_object<'a>(
    value: &'a Map<String, Value>,
    field: &str,
    owner: &str,
) -> Result<&'a Map<String, Value>, EventError> {
    value
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| missing_field(owner, field))
}

fn required_u32(value: &Map<String, Value>, field: &str, owner: &str) -> Result<u32, EventError> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| missing_field(owner, field))
}

fn missing_field(owner: &str, field: &str) -> EventError {
    invalid(format!("{owner} has no valid '{field}'"))
}

fn invalid(message: impl Into<String>) -> EventError {
    EventError(message.into())
}
