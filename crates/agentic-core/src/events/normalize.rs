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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_delta() {
        let line = r#"data: {"type":"response.output_text.delta","delta":"hello","item_id":"msg_1","output_index":0,"content_index":0,"sequence_number":4}"#;
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.event_type, SSEEventType::OutputTextDelta);
        assert_eq!(frame.sequence_number, Some(4));
        if let EventPayload::TextDelta {
            delta,
            item_id,
            output_index,
            content_index,
        } = &frame.payload
        {
            assert_eq!(delta, "hello");
            assert_eq!(item_id, "msg_1");
            assert_eq!(*output_index, 0);
            assert_eq!(*content_index, 0);
        } else {
            panic!("expected TextDelta payload");
        }
    }

    #[test]
    fn test_function_call_args_delta() {
        let line = r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"city\":","call_id":"call_abc","item_id":"fc_1","output_index":0,"sequence_number":7}"#;
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.event_type, SSEEventType::FunctionCallArgumentsDelta);
        assert_eq!(frame.sequence_number, Some(7));
        if let EventPayload::FunctionCallArgsDelta {
            delta,
            call_id,
            item_id,
            output_index,
        } = &frame.payload
        {
            assert_eq!(delta, r#"{"city":"#);
            assert_eq!(call_id.as_deref(), Some("call_abc"));
            assert_eq!(item_id, "fc_1");
            assert_eq!(*output_index, 0);
        } else {
            panic!("expected FunctionCallArgsDelta payload");
        }
    }

    #[test]
    fn test_function_call_args_done() {
        let line = r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"city\":\"SF\"}","call_id":"call_abc","item_id":"fc_1","name":"get_weather","output_index":0,"sequence_number":8}"#;
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.event_type, SSEEventType::FunctionCallArgumentsDone);
        if let EventPayload::FunctionCallArgsDone {
            arguments,
            call_id,
            name,
            ..
        } = &frame.payload
        {
            assert_eq!(arguments, r#"{"city":"SF"}"#);
            assert_eq!(call_id.as_deref(), Some("call_abc"));
            assert_eq!(name, "get_weather");
        } else {
            panic!("expected FunctionCallArgsDone payload");
        }
    }

    #[test]
    fn test_output_item_done() {
        let line = r#"data: {"type":"response.output_item.done","item":{"id":"msg_1","type":"message","status":"completed","content":[{"type":"output_text","text":"hi"}]},"output_index":0,"sequence_number":9}"#;
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.event_type, SSEEventType::OutputItemDone);
        if let EventPayload::OutputItemDone {
            item_id,
            item_type,
            item,
            ..
        } = &frame.payload
        {
            assert_eq!(item_id, "msg_1");
            assert_eq!(item_type, "message");
            assert_eq!(item["content"][0]["text"].as_str(), Some("hi"));
        } else {
            panic!("expected OutputItemDone payload");
        }
    }

    #[test]
    fn test_vllm_response_done_maps_to_completed() {
        let line = r#"data: {"type":"response.done","response":{"id":"resp_1","status":"completed","usage":{"total_tokens":10}},"sequence_number":9}"#;
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.event_type, SSEEventType::ResponseCompleted);
        if let EventPayload::Response { id, status, usage } = &frame.payload {
            assert_eq!(id, "resp_1");
            assert_eq!(status, "completed");
            assert!(usage.is_some());
        } else {
            panic!("expected Response payload");
        }
    }

    #[test]
    fn test_openai_response_completed() {
        let line = r#"data: {"type":"response.completed","response":{"id":"resp_2","status":"completed","usage":null},"sequence_number":10}"#;
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.event_type, SSEEventType::ResponseCompleted);
        if let EventPayload::Response { id, usage, .. } = &frame.payload {
            assert_eq!(id, "resp_2");
            assert!(usage.is_none());
        } else {
            panic!("expected Response payload");
        }
    }

    #[test]
    fn test_done_marker_returns_none() {
        assert!(normalize_sse_line("data: [DONE]").is_none());
    }

    #[test]
    fn test_non_data_lines_return_none() {
        assert!(normalize_sse_line("event: response.created").is_none());
        assert!(normalize_sse_line("").is_none());
        assert!(normalize_sse_line(": comment").is_none());
        assert!(normalize_sse_line("id: 123").is_none());
    }

    #[test]
    fn test_unknown_event_type() {
        let line = r#"data: {"type":"response.unknown_future_event","foo":"bar"}"#;
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.event_type, SSEEventType::Other);
        assert!(matches!(frame.payload, EventPayload::Raw(_)));
    }

    #[test]
    fn test_malformed_json_returns_none() {
        assert!(normalize_sse_line("data: {not valid json}").is_none());
        assert!(normalize_sse_line("data: ").is_none());
    }

    #[test]
    fn test_response_created() {
        let line = r#"data: {"type":"response.created","response":{"id":"resp_abc","status":"in_progress","usage":null},"sequence_number":0}"#;
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.event_type, SSEEventType::ResponseCreated);
        assert_eq!(frame.sequence_number, Some(0));
        if let EventPayload::Response { id, status, .. } = &frame.payload {
            assert_eq!(id, "resp_abc");
            assert_eq!(status, "in_progress");
        } else {
            panic!("expected Response payload");
        }
    }

    #[test]
    fn test_output_item_added_message() {
        let line = r#"data: {"type":"response.output_item.added","item":{"id":"msg_1","type":"message","status":"in_progress","content":[]},"output_index":0,"sequence_number":2}"#;
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.event_type, SSEEventType::OutputItemAdded);
        if let EventPayload::OutputItemAdded {
            item_id,
            item_type,
            output_index,
            ..
        } = &frame.payload
        {
            assert_eq!(item_id, "msg_1");
            assert_eq!(item_type, "message");
            assert_eq!(*output_index, 0);
        } else {
            panic!("expected OutputItemAdded payload");
        }
    }

    #[test]
    fn test_output_item_added_function_call() {
        let line = r#"data: {"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","status":"in_progress","name":"get_weather","call_id":"call_1","arguments":""},"output_index":1,"sequence_number":5}"#;
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.event_type, SSEEventType::OutputItemAdded);
        if let EventPayload::OutputItemAdded {
            item_id,
            item_type,
            output_index,
            name,
            call_id,
        } = &frame.payload
        {
            assert_eq!(item_id, "fc_1");
            assert_eq!(item_type, "function_call");
            assert_eq!(*output_index, 1);
            assert_eq!(name.as_deref(), Some("get_weather"));
            assert_eq!(call_id.as_deref(), Some("call_1"));
        } else {
            panic!("expected OutputItemAdded payload");
        }
    }

    #[test]
    fn test_content_part_added_is_raw() {
        let line = r#"data: {"type":"response.content_part.added","content_index":0,"item_id":"msg_1","output_index":0,"part":{"type":"output_text","text":""},"sequence_number":3}"#;
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.event_type, SSEEventType::ContentPartAdded);
        assert!(matches!(frame.payload, EventPayload::Raw(_)));
    }

    #[test]
    fn test_no_sequence_number() {
        let line = r#"data: {"type":"response.output_text.delta","delta":"x","item_id":"m","output_index":0,"content_index":0}"#;
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.sequence_number, None);
    }
}
