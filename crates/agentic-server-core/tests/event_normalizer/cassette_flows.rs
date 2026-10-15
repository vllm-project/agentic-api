use agentic_core::events::{EventPayload, SSEEventType, normalize_sse_line};
use serde::Deserialize;

// --- Helpers and constants for integration tests ---

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
        if let Some(frame) = normalize_sse_line(line)
            && let EventPayload::TextDelta { delta, .. } = &frame.payload
        {
            text.push_str(delta);
        }
    }
    assert_eq!(text, "GLOBE");
}

#[test]
fn sentinel_sequence_stream_preserves_terminal_usage_and_text() {
    use agentic_core::executor::accumulator::ResponseAccumulator;
    use agentic_core::types::io::OutputItem;

    // Synthetic protocol regression data, not a captured provider recording.
    let lines = SIMULATED_SSE.iter().map(|line| {
        let mut event: serde_json::Value = serde_json::from_str(line.strip_prefix("data: ").unwrap()).unwrap();
        event["sequence_number"] = (-1).into();
        format!("data: {event}")
    });
    let payload = ResponseAccumulator::from_sse_lines(lines, None)
        .expect("sentinel frames remain valid")
        .finalize("test-model", None, None);
    assert_eq!(payload.status, "completed");
    assert_eq!(payload.output.len(), 1);
    assert!(matches!(&payload.output[0], OutputItem::Message(message) if message.id == "msg_1"));
    let serialized = serde_json::to_value(&payload).unwrap();
    assert_eq!(serialized["output"][0]["content"][0]["text"], "GLOBE");
    assert_eq!(serialized["usage"]["total_tokens"], 18);
}

#[test]
fn test_sequence_numbers_increasing() {
    let mut last_seq: Option<u64> = None;
    for line in SIMULATED_SSE {
        if let Some(frame) = normalize_sse_line(line)
            && let Some(seq) = frame.sequence_number()
        {
            if let Some(prev) = last_seq {
                assert!(seq > prev, "sequence {seq} should be > {prev}");
            }
            last_seq = Some(seq);
        }
    }
    assert!(last_seq.is_some());
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
