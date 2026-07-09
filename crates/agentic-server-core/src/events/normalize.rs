use serde_json::Value;

use super::types::{EventFrame, EventPayload, SSEEventType, SSEItemType};
use crate::utils::common::{deserialize_from_str_opt, deserialize_from_value_opt};

/// Normalize a raw SSE data line into a typed [`EventFrame`].
///
/// Expects input in the form `data: {...}` (the `data: ` prefix is required).
/// Returns `None` for non-data lines, empty lines, and the `data: [DONE]`
/// sentinel.
#[must_use]
pub fn normalize_sse_line(line: &str) -> Option<EventFrame> {
    let data_str = line.strip_prefix("data: ")?;
    if data_str == "[DONE]" {
        return None;
    }

    let json: Value = deserialize_from_str_opt(data_str)?;

    let event_type = json
        .get("type")
        .and_then(Value::as_str)
        .map_or(SSEEventType::Other, classify_event_type);

    let sequence_number = json.get("sequence_number").and_then(Value::as_u64);

    let payload = extract_payload(event_type, &json);

    Some(EventFrame {
        event_type,
        payload,
        sequence_number,
    })
}

/// Map a wire-format event type string to our enum.
fn classify_event_type(type_str: &str) -> SSEEventType {
    match type_str {
        "response.created" => SSEEventType::ResponseCreated,
        "response.in_progress" => SSEEventType::ResponseInProgress,
        "response.completed" | "response.done" => SSEEventType::ResponseCompleted,
        "response.failed" => SSEEventType::ResponseFailed,
        "response.incomplete" => SSEEventType::ResponseIncomplete,
        "response.output_item.added" => SSEEventType::OutputItemAdded,
        "response.output_item.done" => SSEEventType::OutputItemDone,
        "response.output_text.delta" => SSEEventType::OutputTextDelta,
        "response.output_text.done" => SSEEventType::OutputTextDone,
        "response.content_part.added" => SSEEventType::ContentPartAdded,
        "response.content_part.done" => SSEEventType::ContentPartDone,
        "response.function_call_arguments.delta" => SSEEventType::FunctionCallArgumentsDelta,
        "response.function_call_arguments.done" => SSEEventType::FunctionCallArgumentsDone,
        "response.reasoning_text.delta" => SSEEventType::ReasoningTextDelta,
        "response.reasoning_text.done" => SSEEventType::ReasoningTextDone,
        "response.reasoning_part.added" => SSEEventType::ReasoningPartAdded,
        "response.reasoning_part.done" => SSEEventType::ReasoningPartDone,
        "response.reasoning_summary_text.delta" => SSEEventType::ReasoningSummaryTextDelta,
        "response.reasoning_summary_text.done" => SSEEventType::ReasoningSummaryTextDone,
        "response.file_search_call.searching" => SSEEventType::FileSearchCallSearching,
        "response.file_search_call.completed" => SSEEventType::FileSearchCallCompleted,
        "response.web_search_call.in_progress" => SSEEventType::WebSearchCallInProgress,
        "response.web_search_call.searching" => SSEEventType::WebSearchCallSearching,
        "response.web_search_call.completed" => SSEEventType::WebSearchCallCompleted,
        _ => SSEEventType::Other,
    }
}

/// Extract a typed payload from the JSON body based on the classified event type.
fn extract_payload(event_type: SSEEventType, json: &Value) -> EventPayload {
    match event_type {
        SSEEventType::ResponseCreated
        | SSEEventType::ResponseInProgress
        | SSEEventType::ResponseCompleted
        | SSEEventType::ResponseFailed
        | SSEEventType::ResponseIncomplete => extract_response_payload(json),

        SSEEventType::OutputItemAdded => extract_output_item_added(json),
        SSEEventType::OutputItemDone => extract_output_item_done(json),

        SSEEventType::OutputTextDelta => extract_text_delta(json),
        SSEEventType::OutputTextDone => extract_text_done(json),

        SSEEventType::FunctionCallArgumentsDelta => extract_fn_call_args_delta(json),
        SSEEventType::FunctionCallArgumentsDone => extract_fn_call_args_done(json),

        SSEEventType::ReasoningTextDelta | SSEEventType::ReasoningSummaryTextDelta => extract_reasoning_delta(json),
        SSEEventType::ReasoningTextDone | SSEEventType::ReasoningSummaryTextDone => extract_reasoning_done(json),

        SSEEventType::ContentPartAdded
        | SSEEventType::ContentPartDone
        | SSEEventType::ReasoningPartAdded
        | SSEEventType::ReasoningPartDone
        | SSEEventType::FileSearchCallSearching
        | SSEEventType::FileSearchCallCompleted
        | SSEEventType::WebSearchCallInProgress
        | SSEEventType::WebSearchCallSearching
        | SSEEventType::WebSearchCallCompleted
        | SSEEventType::Other => EventPayload::Raw(json.clone()),
    }
}

fn json_str(json: &Value, key: &str) -> String {
    json[key].as_str().unwrap_or_default().to_string()
}

fn json_str_opt(json: &Value, key: &str) -> Option<String> {
    json[key].as_str().map(ToString::to_string)
}

fn json_u32(json: &Value, key: &str) -> u32 {
    u32::try_from(json[key].as_u64().unwrap_or(0)).unwrap_or(u32::MAX)
}

fn extract_response_payload(json: &Value) -> EventPayload {
    let response = &json["response"];
    EventPayload::Response {
        id: json_str(response, "id"),
        status: json_str(response, "status"),
        usage: response
            .get("usage")
            .filter(|v| !v.is_null())
            .and_then(|v| deserialize_from_value_opt(v.clone())),
    }
}

fn extract_output_item_added(json: &Value) -> EventPayload {
    let item = &json["item"];
    EventPayload::OutputItemAdded {
        item_id: json_str(item, "id"),
        item_type: SSEItemType::from(json_str(item, "type")),
        output_index: json_u32(json, "output_index"),
        name: json_str_opt(item, "name"),
        namespace: json_str_opt(item, "namespace"),
        call_id: json_str_opt(item, "call_id"),
    }
}

fn extract_output_item_done(json: &Value) -> EventPayload {
    let item = &json["item"];
    EventPayload::OutputItemDone {
        item_id: json_str(item, "id"),
        item_type: SSEItemType::from(json_str(item, "type")),
        output_index: json_u32(json, "output_index"),
        item: item.clone(),
    }
}

fn extract_text_delta(json: &Value) -> EventPayload {
    EventPayload::TextDelta {
        delta: json_str(json, "delta"),
        item_id: json_str(json, "item_id"),
        output_index: json_u32(json, "output_index"),
        content_index: json_u32(json, "content_index"),
    }
}

fn extract_text_done(json: &Value) -> EventPayload {
    EventPayload::TextDone {
        text: json_str(json, "text"),
        item_id: json_str(json, "item_id"),
        output_index: json_u32(json, "output_index"),
    }
}

fn extract_fn_call_args_delta(json: &Value) -> EventPayload {
    EventPayload::FunctionCallArgsDelta {
        delta: json_str(json, "delta"),
        call_id: json_str_opt(json, "call_id"),
        item_id: json_str(json, "item_id"),
        output_index: json_u32(json, "output_index"),
    }
}

fn extract_fn_call_args_done(json: &Value) -> EventPayload {
    EventPayload::FunctionCallArgsDone {
        arguments: json_str(json, "arguments"),
        call_id: json_str_opt(json, "call_id"),
        item_id: json_str(json, "item_id"),
        name: json_str(json, "name"),
        output_index: json_u32(json, "output_index"),
    }
}

fn extract_reasoning_delta(json: &Value) -> EventPayload {
    EventPayload::ReasoningDelta {
        delta: json_str(json, "delta"),
        item_id: json_str(json, "item_id"),
    }
}

fn extract_reasoning_done(json: &Value) -> EventPayload {
    EventPayload::ReasoningDone {
        text: json_str(json, "text"),
        item_id: json_str(json, "item_id"),
    }
}
