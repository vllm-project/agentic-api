use agentic_core::events::{EventPayload, SSEEventType, normalize_sse_line};

/// Simulated streaming cassette matching the format of
/// `resp-single-gpt-4o-streaming.yaml` — single turn, text "GLOBE" split
/// across 3 deltas.
const SIMULATED_SSE: &[&str] = &[
    r#"data: {"type":"response.created","response":{"id":"resp_abc","status":"in_progress","usage":null},"sequence_number":0}"#,
    r#"data: {"type":"response.in_progress","response":{"id":"resp_abc","status":"in_progress","usage":null},"sequence_number":1}"#,
    r#"data: {"type":"response.output_item.added","item":{"id":"msg_1","type":"message","status":"in_progress","content":[]},"output_index":0,"sequence_number":2}"#,
    r#"data: {"type":"response.content_part.added","content_index":0,"item_id":"msg_1","output_index":0,"part":{"type":"output_text","text":""},"sequence_number":3}"#,
    r#"data: {"type":"response.output_text.delta","content_index":0,"delta":"G","item_id":"msg_1","output_index":0,"sequence_number":4}"#,
    r#"data: {"type":"response.output_text.delta","content_index":0,"delta":"LO","item_id":"msg_1","output_index":0,"sequence_number":5}"#,
    r#"data: {"type":"response.output_text.delta","content_index":0,"delta":"BE","item_id":"msg_1","output_index":0,"sequence_number":6}"#,
    r#"data: {"type":"response.output_text.done","content_index":0,"item_id":"msg_1","output_index":0,"text":"GLOBE","sequence_number":7}"#,
    r#"data: {"type":"response.content_part.done","content_index":0,"item_id":"msg_1","output_index":0,"part":{"type":"output_text","text":"GLOBE"},"sequence_number":8}"#,
    r#"data: {"type":"response.output_item.done","item":{"id":"msg_1","type":"message","status":"completed","content":[{"type":"output_text","text":"GLOBE"}],"role":"assistant"},"output_index":0,"sequence_number":9}"#,
    r#"data: {"type":"response.completed","response":{"id":"resp_abc","status":"completed","usage":{"input_tokens":14,"output_tokens":4,"total_tokens":18}},"sequence_number":10}"#,
];

#[test]
fn test_event_distribution() {
    let mut counts = std::collections::HashMap::new();
    for line in SIMULATED_SSE {
        if let Some(frame) = normalize_sse_line(line) {
            *counts.entry(frame.event_type).or_insert(0u32) += 1;
        }
    }

    assert_eq!(counts.get(&SSEEventType::ResponseCreated), Some(&1));
    assert_eq!(counts.get(&SSEEventType::ResponseInProgress), Some(&1));
    assert_eq!(counts.get(&SSEEventType::OutputItemAdded), Some(&1));
    assert_eq!(counts.get(&SSEEventType::OutputTextDelta), Some(&3));
    assert_eq!(counts.get(&SSEEventType::OutputTextDone), Some(&1));
    assert_eq!(counts.get(&SSEEventType::ContentPartAdded), Some(&1));
    assert_eq!(counts.get(&SSEEventType::ContentPartDone), Some(&1));
    assert_eq!(counts.get(&SSEEventType::OutputItemDone), Some(&1));
    assert_eq!(counts.get(&SSEEventType::ResponseCompleted), Some(&1));
}

#[test]
fn test_text_accumulation() {
    let mut text = String::new();
    for line in SIMULATED_SSE {
        if let Some(frame) = normalize_sse_line(line) {
            if let EventPayload::TextDelta { delta, .. } = &frame.payload {
                text.push_str(delta);
            }
        }
    }
    assert_eq!(text, "GLOBE");
}

#[test]
fn test_sequence_numbers_increasing() {
    let mut last_seq: Option<u64> = None;
    for line in SIMULATED_SSE {
        if let Some(frame) = normalize_sse_line(line) {
            if let Some(seq) = frame.sequence_number {
                if let Some(prev) = last_seq {
                    assert!(seq > prev, "sequence {seq} should be > {prev}");
                }
                last_seq = Some(seq);
            }
        }
    }
    assert!(last_seq.is_some());
}

/// Simulate a function-call streaming session.
#[test]
fn test_function_call_flow() {
    let lines = &[
        r#"data: {"type":"response.created","response":{"id":"resp_fc","status":"in_progress","usage":null},"sequence_number":0}"#,
        r#"data: {"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","status":"in_progress","name":"get_weather","call_id":"call_1","arguments":""},"output_index":0,"sequence_number":1}"#,
        r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"ci","call_id":"call_1","item_id":"fc_1","output_index":0,"sequence_number":2}"#,
        r#"data: {"type":"response.function_call_arguments.delta","delta":"ty\":\"SF\"}","call_id":"call_1","item_id":"fc_1","output_index":0,"sequence_number":3}"#,
        r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"city\":\"SF\"}","call_id":"call_1","item_id":"fc_1","name":"get_weather","output_index":0,"sequence_number":4}"#,
        r#"data: {"type":"response.output_item.done","item":{"id":"fc_1","type":"function_call","status":"completed","name":"get_weather","call_id":"call_1","arguments":"{\"city\":\"SF\"}"},"output_index":0,"sequence_number":5}"#,
        r#"data: {"type":"response.completed","response":{"id":"resp_fc","status":"completed","usage":{"input_tokens":20,"output_tokens":8,"total_tokens":28}},"sequence_number":6}"#,
    ];

    let mut args_accumulated = String::new();
    let mut final_args = String::new();
    let mut final_name = String::new();

    for line in lines {
        let frame = normalize_sse_line(line).unwrap();
        match &frame.payload {
            EventPayload::FunctionCallArgsDelta { delta, .. } => {
                args_accumulated.push_str(delta);
            }
            EventPayload::FunctionCallArgsDone { arguments, name, .. } => {
                final_args = arguments.clone();
                final_name = name.clone();
            }
            _ => {}
        }
    }

    assert_eq!(args_accumulated, r#"{"city":"SF"}"#);
    assert_eq!(final_args, r#"{"city":"SF"}"#);
    assert_eq!(final_name, "get_weather");
}

/// Real vLLM output captured from `google/gemma-4-26B-A4B-it` on 2026-06-09.
/// Key differences from `OpenAI`: no `call_id` in delta events, different id format.
#[test]
fn test_real_vllm_function_call_stream() {
    let lines = &[
        r#"data: {"response":{"id":"resp_938d583bbec02940","created_at":1781048957,"status":"in_progress","output":[],"model":"google/gemma-4-26B-A4B-it","object":"response"},"sequence_number":0,"type":"response.created"}"#,
        r#"data: {"response":{"id":"resp_938d583bbec02940","status":"in_progress"},"sequence_number":1,"type":"response.in_progress"}"#,
        r#"data: {"item":{"arguments":"","call_id":"call_92fd766dcc21a19c","name":"get_weather","type":"function_call","id":"8c5375b5b08d666c","status":"in_progress"},"output_index":0,"sequence_number":2,"type":"response.output_item.added"}"#,
        r#"data: {"delta":"{\"","item_id":"8c5375b5b08d666c","output_index":0,"sequence_number":3,"type":"response.function_call_arguments.delta"}"#,
        r#"data: {"delta":"city","item_id":"8c5375b5b08d666c","output_index":0,"sequence_number":4,"type":"response.function_call_arguments.delta"}"#,
        r#"data: {"delta":"\":","item_id":"8c5375b5b08d666c","output_index":0,"sequence_number":5,"type":"response.function_call_arguments.delta"}"#,
        r#"data: {"delta":" \"","item_id":"8c5375b5b08d666c","output_index":0,"sequence_number":6,"type":"response.function_call_arguments.delta"}"#,
        r#"data: {"delta":"San","item_id":"8c5375b5b08d666c","output_index":0,"sequence_number":7,"type":"response.function_call_arguments.delta"}"#,
        r#"data: {"delta":" Francisco","item_id":"8c5375b5b08d666c","output_index":0,"sequence_number":8,"type":"response.function_call_arguments.delta"}"#,
        r#"data: {"delta":"\"","item_id":"8c5375b5b08d666c","output_index":0,"sequence_number":9,"type":"response.function_call_arguments.delta"}"#,
        r#"data: {"delta":"}","item_id":"8c5375b5b08d666c","output_index":0,"sequence_number":10,"type":"response.function_call_arguments.delta"}"#,
        r#"data: {"arguments":"{\"city\": \"San Francisco\"}","item_id":"8c5375b5b08d666c","name":"get_weather","output_index":0,"sequence_number":11,"type":"response.function_call_arguments.done"}"#,
        r#"data: {"item":{"arguments":"{\"city\": \"San Francisco\"}","call_id":"call_92fd766dcc21a19c","name":"get_weather","type":"function_call","id":"8c5375b5b08d666c","status":"completed"},"output_index":0,"sequence_number":12,"type":"response.output_item.done"}"#,
        r#"data: {"response":{"id":"resp_938d583bbec02940","status":"completed","usage":{"input_tokens":66,"output_tokens":21,"total_tokens":87}},"sequence_number":13,"type":"response.completed"}"#,
    ];

    let mut args = String::new();
    let mut final_name = String::new();
    let mut event_types = Vec::new();

    for line in lines {
        let frame = normalize_sse_line(line).expect("all lines should parse");
        event_types.push(frame.event_type);
        match &frame.payload {
            EventPayload::FunctionCallArgsDelta { delta, .. } => args.push_str(delta),
            EventPayload::FunctionCallArgsDone { name, .. } => final_name = name.clone(),
            _ => {}
        }
    }

    assert_eq!(args, r#"{"city": "San Francisco"}"#);
    assert_eq!(final_name, "get_weather");

    assert_eq!(event_types[0], SSEEventType::ResponseCreated);
    assert_eq!(event_types[1], SSEEventType::ResponseInProgress);
    assert_eq!(event_types[2], SSEEventType::OutputItemAdded);
    assert_eq!(event_types[3], SSEEventType::FunctionCallArgumentsDelta);
    assert_eq!(event_types[11], SSEEventType::FunctionCallArgumentsDone);
    assert_eq!(event_types[12], SSEEventType::OutputItemDone);
    assert_eq!(event_types[13], SSEEventType::ResponseCompleted);

    // Verify the output_item.done carries the full function_call item
    let done_frame = normalize_sse_line(lines[12]).unwrap();
    if let EventPayload::OutputItemDone { item_type, item, .. } = &done_frame.payload {
        assert_eq!(item_type, "function_call");
        assert_eq!(item["name"].as_str(), Some("get_weather"));
        assert_eq!(item["call_id"].as_str(), Some("call_92fd766dcc21a19c"));
    } else {
        panic!("expected OutputItemDone");
    }
}
