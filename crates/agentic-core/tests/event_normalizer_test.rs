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
