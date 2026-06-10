use serde_json::Value;

use super::types::{EventFrame, EventPayload, SSEEventType};

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

    let json: Value = serde_json::from_str(data_str).ok()?;

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
        "response.reasoning_summary_text.delta" => SSEEventType::ReasoningSummaryTextDelta,
        "response.reasoning_summary_text.done" => SSEEventType::ReasoningSummaryTextDone,
        "response.file_search_call.searching" => SSEEventType::FileSearchCallSearching,
        "response.file_search_call.completed" => SSEEventType::FileSearchCallCompleted,
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

        SSEEventType::ReasoningSummaryTextDelta => extract_reasoning_delta(json),
        SSEEventType::ReasoningSummaryTextDone => extract_reasoning_done(json),

        SSEEventType::ContentPartAdded
        | SSEEventType::ContentPartDone
        | SSEEventType::FileSearchCallSearching
        | SSEEventType::FileSearchCallCompleted
        | SSEEventType::WebSearchCallSearching
        | SSEEventType::WebSearchCallCompleted
        | SSEEventType::Other => EventPayload::Raw(json.clone()),
    }
}

fn index_u32(json: &Value, key: &str) -> u32 {
    u32::try_from(json[key].as_u64().unwrap_or(0)).unwrap_or(u32::MAX)
}

fn extract_response_payload(json: &Value) -> EventPayload {
    let response = &json["response"];
    EventPayload::Response {
        id: response["id"].as_str().unwrap_or_default().to_string(),
        status: response["status"].as_str().unwrap_or_default().to_string(),
        usage: response.get("usage").filter(|v| !v.is_null()).cloned(),
    }
}

fn extract_output_item_added(json: &Value) -> EventPayload {
    let item = &json["item"];
    EventPayload::OutputItemAdded {
        item_id: item["id"].as_str().unwrap_or_default().to_string(),
        item_type: item["type"].as_str().unwrap_or_default().to_string(),
        output_index: index_u32(json, "output_index"),
        name: item["name"].as_str().map(ToString::to_string),
        call_id: item["call_id"].as_str().map(ToString::to_string),
    }
}

fn extract_output_item_done(json: &Value) -> EventPayload {
    let item = &json["item"];
    EventPayload::OutputItemDone {
        item_id: item["id"].as_str().unwrap_or_default().to_string(),
        item_type: item["type"].as_str().unwrap_or_default().to_string(),
        output_index: index_u32(json, "output_index"),
        item: item.clone(),
    }
}

fn extract_text_delta(json: &Value) -> EventPayload {
    EventPayload::TextDelta {
        delta: json["delta"].as_str().unwrap_or_default().to_string(),
        item_id: json["item_id"].as_str().unwrap_or_default().to_string(),
        output_index: index_u32(json, "output_index"),
        content_index: index_u32(json, "content_index"),
    }
}

fn extract_text_done(json: &Value) -> EventPayload {
    EventPayload::TextDone {
        text: json["text"].as_str().unwrap_or_default().to_string(),
        item_id: json["item_id"].as_str().unwrap_or_default().to_string(),
        output_index: index_u32(json, "output_index"),
    }
}

fn extract_fn_call_args_delta(json: &Value) -> EventPayload {
    EventPayload::FunctionCallArgsDelta {
        delta: json["delta"].as_str().unwrap_or_default().to_string(),
        call_id: json["call_id"].as_str().map(ToString::to_string),
        item_id: json["item_id"].as_str().unwrap_or_default().to_string(),
        output_index: index_u32(json, "output_index"),
    }
}

fn extract_fn_call_args_done(json: &Value) -> EventPayload {
    EventPayload::FunctionCallArgsDone {
        arguments: json["arguments"].as_str().unwrap_or_default().to_string(),
        call_id: json["call_id"].as_str().map(ToString::to_string),
        item_id: json["item_id"].as_str().unwrap_or_default().to_string(),
        name: json["name"].as_str().unwrap_or_default().to_string(),
        output_index: index_u32(json, "output_index"),
    }
}

fn extract_reasoning_delta(json: &Value) -> EventPayload {
    EventPayload::ReasoningDelta {
        delta: json["delta"].as_str().unwrap_or_default().to_string(),
        item_id: json["item_id"].as_str().unwrap_or_default().to_string(),
    }
}

fn extract_reasoning_done(json: &Value) -> EventPayload {
    EventPayload::ReasoningDone {
        text: json["text"].as_str().unwrap_or_default().to_string(),
        item_id: json["item_id"].as_str().unwrap_or_default().to_string(),
    }
}
