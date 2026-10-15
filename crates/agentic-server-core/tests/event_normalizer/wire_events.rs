use agentic_core::events::{EventPayload, SSEEventType, normalize_sse_line};

#[test]
fn negative_sequence_numbers_preserve_event_payloads() {
    for sequence in [-1, -2, i64::MIN] {
        let line = format!(
            r#"data: {{"type":"response.output_text.delta","delta":"hello","item_id":"msg_1","output_index":0,"content_index":0,"sequence_number":{sequence}}}"#
        );
        let frame = normalize_sse_line(&line).expect("negative sequence must not discard the event");
        assert_eq!(frame.sequence_number(), None);
        assert!(matches!(frame.payload, EventPayload::TextDelta { ref delta, .. } if delta == "hello"));
        assert_eq!(frame.wire.output_index, Some(0));
    }
}

#[test]
fn sequence_number_deserialization_preserves_unsigned_range_and_rejects_nonintegers() {
    for sequence in [0, 1, u64::MAX] {
        let line = format!(r#"data: {{"type":"response.in_progress","sequence_number":{sequence}}}"#);
        assert_eq!(normalize_sse_line(&line).unwrap().sequence_number(), Some(sequence));
    }
    for sequence in [r#""-1""#, "-1.5", "true", "{}", "[]"] {
        let line = format!(r#"data: {{"type":"response.in_progress","sequence_number":{sequence}}}"#);
        assert!(normalize_sse_line(&line).is_none(), "invalid sequence: {sequence}");
    }
    for fields in ["", r#", "sequence_number": null"#] {
        let line = format!(r#"data: {{"type":"response.in_progress"{fields}}}"#);
        assert_eq!(normalize_sse_line(&line).unwrap().sequence_number(), None);
    }
}

#[test]
fn test_text_delta() {
    let line = r#"data: {"type":"response.output_text.delta","delta":"hello","item_id":"msg_1","output_index":0,"content_index":0,"sequence_number":4}"#;
    let frame = normalize_sse_line(line).unwrap();
    assert_eq!(frame.event_type, SSEEventType::OutputTextDelta);
    assert_eq!(frame.sequence_number(), Some(4));
    if let EventPayload::TextDelta {
        delta,
        item_id,
        output_index,
        content_index,
    } = &frame.payload
    {
        assert_eq!(delta, "hello");
        assert_eq!(item_id, "msg_1");
        assert_eq!(*output_index, Some(0));
        assert_eq!(*content_index, 0);
    } else {
        panic!("expected TextDelta payload");
    }
}

#[test]
fn test_output_item_done() {
    let line = r#"data: {"type":"response.output_item.done","item":{"id":"msg_1","item_id":"msg_fallback","type":"message","status":"completed","content":[{"type":"output_text","text":"hi"}]},"output_index":0,"sequence_number":9}"#;
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
        assert_eq!(item["id"], "msg_1");
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
    if let EventPayload::Response { id, status, usage, .. } = &frame.payload {
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
fn test_typeless_json_event_is_preserved() {
    let frame = normalize_sse_line(r#"data: {"foo":1}"#).unwrap();

    assert_eq!(frame.event_type, SSEEventType::Other);
    assert_eq!(serde_json::to_value(frame.wire).unwrap(), serde_json::json!({"foo": 1}));
}

#[test]
fn test_wire_event_preserves_unknown_fields() {
    let line = r#"data: {"type":"response.output_text.delta","sequence_number":4,"output_index":2,"item_id":"msg_1","content_index":0,"delta":"hello","provider_extra":{"nested":true},"future_array":[1,2]}"#;
    let frame = normalize_sse_line(line).unwrap();
    let wire = serde_json::to_value(&frame.wire).unwrap();

    assert_eq!(wire["type"], "response.output_text.delta");
    assert_eq!(wire["sequence_number"], 4);
    assert_eq!(wire["output_index"], 2);
    assert_eq!(wire["provider_extra"]["nested"], true);
    assert_eq!(wire["future_array"], serde_json::json!([1, 2]));
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
    assert_eq!(frame.sequence_number(), Some(0));
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
        assert_eq!(*output_index, Some(0));
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
    let line =
        r#"data: {"type":"response.output_text.delta","delta":"x","item_id":"m","output_index":0,"content_index":0}"#;
    let frame = normalize_sse_line(line).unwrap();
    assert_eq!(frame.sequence_number(), None);
}

#[test]
fn test_response_failed() {
    let line = r#"data: {"type":"response.failed","response":{"id":"resp_err","status":"failed","usage":null},"sequence_number":2}"#;
    let frame = normalize_sse_line(line).unwrap();
    assert_eq!(frame.event_type, SSEEventType::ResponseFailed);
    if let EventPayload::Response { id, status, .. } = &frame.payload {
        assert_eq!(id, "resp_err");
        assert_eq!(status, "failed");
    } else {
        panic!("expected Response payload");
    }
}

#[test]
fn test_response_incomplete() {
    let line = r#"data: {"type":"response.incomplete","response":{"id":"resp_inc","status":"incomplete","usage":{"input_tokens":100,"output_tokens":4096,"total_tokens":4196}},"sequence_number":99}"#;
    let frame = normalize_sse_line(line).unwrap();
    assert_eq!(frame.event_type, SSEEventType::ResponseIncomplete);
    if let EventPayload::Response { status, usage, .. } = &frame.payload {
        assert_eq!(status, "incomplete");
        assert!(usage.is_some());
    } else {
        panic!("expected Response payload");
    }
}

#[test]
fn test_empty_delta() {
    let line = r#"data: {"type":"response.output_text.delta","delta":"","item_id":"msg_1","output_index":0,"content_index":0,"sequence_number":4}"#;
    let frame = normalize_sse_line(line).unwrap();
    assert_eq!(frame.event_type, SSEEventType::OutputTextDelta);
    if let EventPayload::TextDelta { delta, .. } = &frame.payload {
        assert_eq!(delta, "");
    } else {
        panic!("expected TextDelta payload");
    }
}

#[test]
fn test_unicode_in_delta() {
    let line = r#"data: {"type":"response.output_text.delta","delta":"こんにちは 🌍","item_id":"msg_1","output_index":0,"content_index":0,"sequence_number":4}"#;
    let frame = normalize_sse_line(line).unwrap();
    if let EventPayload::TextDelta { delta, .. } = &frame.payload {
        assert_eq!(delta, "こんにちは 🌍");
    } else {
        panic!("expected TextDelta payload");
    }
}

#[test]
fn test_file_search_classification() {
    let line =
        r#"data: {"type":"response.file_search_call.searching","item_id":"fs_1","output_index":0,"sequence_number":3}"#;
    let frame = normalize_sse_line(line).unwrap();
    assert_eq!(frame.event_type, SSEEventType::FileSearchCallSearching);
    assert!(matches!(frame.payload, EventPayload::Raw(_)));
}

#[test]
fn test_web_search_classification() {
    for (line, expected) in [
        (
            r#"data: {"type":"response.web_search_call.in_progress","item_id":"ws_1","output_index":0,"sequence_number":4}"#,
            SSEEventType::WebSearchCallInProgress,
        ),
        (
            r#"data: {"type":"response.web_search_call.searching","item_id":"ws_1","output_index":0,"sequence_number":5}"#,
            SSEEventType::WebSearchCallSearching,
        ),
        (
            r#"data: {"type":"response.web_search_call.completed","item_id":"ws_1","output_index":0,"sequence_number":6}"#,
            SSEEventType::WebSearchCallCompleted,
        ),
    ] {
        let frame = normalize_sse_line(line).unwrap();
        assert_eq!(frame.event_type, expected);
        assert!(matches!(frame.payload, EventPayload::Raw(_)));
    }
}
