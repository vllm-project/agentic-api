use agentic_core::events::{EventPayload, SSEEventType, normalize_sse_line};
use serde::Deserialize;

const CASSETTE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/events");

#[derive(Deserialize)]
struct EventCassette {
    sse: Vec<String>,
    expected_text: Option<String>,
    expected_function_call: Option<ExpectedFunctionCall>,
}

#[derive(Deserialize)]
struct ExpectedFunctionCall {
    name: String,
    arguments: String,
}

fn load_event_cassette(filename: &str) -> EventCassette {
    let path = format!("{CASSETTE_DIR}/{filename}");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_yml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

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

// --- Cassette-driven tests ---

#[test]
fn test_cassette_text_only_vllm() {
    let cassette = load_event_cassette("text-only-vllm-gemma4.yaml");
    let mut text = String::new();
    let mut parsed_count = 0;

    for line in &cassette.sse {
        if let Some(frame) = normalize_sse_line(line) {
            parsed_count += 1;
            if let EventPayload::TextDelta { delta, .. } = &frame.payload {
                text.push_str(delta);
            }
        }
    }

    assert_eq!(text, cassette.expected_text.unwrap());
    assert_eq!(parsed_count, cassette.sse.len(), "all lines should parse");
}

#[test]
fn test_cassette_function_call_vllm() {
    let cassette = load_event_cassette("function-call-vllm-gemma4.yaml");
    let expected = cassette.expected_function_call.unwrap();

    let mut args = String::new();
    let mut final_name = String::new();
    let mut parsed_count = 0;

    for line in &cassette.sse {
        if let Some(frame) = normalize_sse_line(line) {
            parsed_count += 1;
            match &frame.payload {
                EventPayload::FunctionCallArgsDelta { delta, .. } => args.push_str(delta),
                EventPayload::FunctionCallArgsDone { name, .. } => final_name = name.clone(),
                _ => {}
            }
        }
    }

    assert_eq!(args, expected.arguments);
    assert_eq!(final_name, expected.name);
    assert_eq!(parsed_count, cassette.sse.len(), "all lines should parse");
}

/// Parallel function calls — two tools called in the same response (different `output_index`).
#[test]
fn test_parallel_function_calls() {
    let lines = &[
        r#"data: {"type":"response.created","response":{"id":"resp_par","status":"in_progress","usage":null},"sequence_number":0}"#,
        r#"data: {"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","name":"get_weather","call_id":"call_1","arguments":"","status":"in_progress"},"output_index":0,"sequence_number":1}"#,
        r#"data: {"type":"response.output_item.added","item":{"id":"fc_2","type":"function_call","name":"get_time","call_id":"call_2","arguments":"","status":"in_progress"},"output_index":1,"sequence_number":2}"#,
        r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"city\":\"SF\"}","item_id":"fc_1","output_index":0,"sequence_number":3}"#,
        r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"tz\":\"PST\"}","item_id":"fc_2","output_index":1,"sequence_number":4}"#,
        r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"city\":\"SF\"}","item_id":"fc_1","name":"get_weather","output_index":0,"sequence_number":5}"#,
        r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"tz\":\"PST\"}","item_id":"fc_2","name":"get_time","output_index":1,"sequence_number":6}"#,
        r#"data: {"type":"response.output_item.done","item":{"id":"fc_1","type":"function_call","name":"get_weather","call_id":"call_1","arguments":"{\"city\":\"SF\"}","status":"completed"},"output_index":0,"sequence_number":7}"#,
        r#"data: {"type":"response.output_item.done","item":{"id":"fc_2","type":"function_call","name":"get_time","call_id":"call_2","arguments":"{\"tz\":\"PST\"}","status":"completed"},"output_index":1,"sequence_number":8}"#,
        r#"data: {"type":"response.completed","response":{"id":"resp_par","status":"completed","usage":{"input_tokens":30,"output_tokens":15,"total_tokens":45}},"sequence_number":9}"#,
    ];

    let mut calls: std::collections::HashMap<String, (String, String)> = std::collections::HashMap::new();

    for line in lines {
        let frame = normalize_sse_line(line).unwrap();
        if let EventPayload::FunctionCallArgsDone {
            item_id,
            name,
            arguments,
            ..
        } = &frame.payload
        {
            calls.insert(item_id.clone(), (name.clone(), arguments.clone()));
        }
    }

    assert_eq!(calls.len(), 2);
    assert_eq!(calls["fc_1"], ("get_weather".into(), r#"{"city":"SF"}"#.into()));
    assert_eq!(calls["fc_2"], ("get_time".into(), r#"{"tz":"PST"}"#.into()));
}

/// Mixed response: text message (`output_index`=0) + function call (`output_index`=1).
#[test]
fn test_mixed_text_and_function_call() {
    let lines = &[
        r#"data: {"type":"response.created","response":{"id":"resp_mix","status":"in_progress","usage":null},"sequence_number":0}"#,
        r#"data: {"type":"response.output_item.added","item":{"id":"msg_1","type":"message","status":"in_progress","content":[]},"output_index":0,"sequence_number":1}"#,
        r#"data: {"type":"response.output_text.delta","delta":"Let me check ","item_id":"msg_1","output_index":0,"content_index":0,"sequence_number":2}"#,
        r#"data: {"type":"response.output_text.delta","delta":"the weather.","item_id":"msg_1","output_index":0,"content_index":0,"sequence_number":3}"#,
        r#"data: {"type":"response.output_text.done","text":"Let me check the weather.","item_id":"msg_1","output_index":0,"sequence_number":4}"#,
        r#"data: {"type":"response.output_item.done","item":{"id":"msg_1","type":"message","status":"completed","content":[{"type":"output_text","text":"Let me check the weather."}]},"output_index":0,"sequence_number":5}"#,
        r#"data: {"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","name":"get_weather","call_id":"call_x","arguments":"","status":"in_progress"},"output_index":1,"sequence_number":6}"#,
        r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"city\":\"NYC\"}","item_id":"fc_1","output_index":1,"sequence_number":7}"#,
        r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"city\":\"NYC\"}","item_id":"fc_1","name":"get_weather","output_index":1,"sequence_number":8}"#,
        r#"data: {"type":"response.output_item.done","item":{"id":"fc_1","type":"function_call","name":"get_weather","call_id":"call_x","arguments":"{\"city\":\"NYC\"}","status":"completed"},"output_index":1,"sequence_number":9}"#,
        r#"data: {"type":"response.completed","response":{"id":"resp_mix","status":"completed","usage":{"input_tokens":25,"output_tokens":20,"total_tokens":45}},"sequence_number":10}"#,
    ];

    let mut text = String::new();
    let mut fn_name = String::new();
    let mut fn_args = String::new();

    for line in lines {
        let frame = normalize_sse_line(line).unwrap();
        match &frame.payload {
            EventPayload::TextDelta { delta, .. } => text.push_str(delta),
            EventPayload::FunctionCallArgsDone { name, arguments, .. } => {
                fn_name = name.clone();
                fn_args = arguments.clone();
            }
            _ => {}
        }
    }

    assert_eq!(text, "Let me check the weather.");
    assert_eq!(fn_name, "get_weather");
    assert_eq!(fn_args, r#"{"city":"NYC"}"#);
}

/// Verify `call_id` is recoverable from `OutputItemAdded` for vLLM streams.
#[test]
fn test_call_id_from_output_item_added() {
    let line = r#"data: {"type":"response.output_item.added","item":{"arguments":"","call_id":"call_abc123","name":"search","type":"function_call","id":"fc_99","status":"in_progress"},"output_index":0,"sequence_number":2}"#;
    let frame = normalize_sse_line(line).unwrap();
    if let EventPayload::OutputItemAdded {
        call_id, name, item_id, ..
    } = &frame.payload
    {
        assert_eq!(call_id.as_deref(), Some("call_abc123"));
        assert_eq!(name.as_deref(), Some("search"));
        assert_eq!(item_id, "fc_99");
    } else {
        panic!("expected OutputItemAdded");
    }
}
