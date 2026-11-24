//! Cassette-driven integration tests: feed real vLLM SSE recordings through
//! the full accumulator pipeline (normalize → `process_event` → finalize) and
//! verify the resulting output items match expected values.
//!
//! Tests cover both the legacy `events/` cassettes (flat SSE list) and the
//! newer `tool_calls/` cassettes from PR #60 (multi-turn `turns` format).

use serde::Deserialize;

use agentic_core::executor::accumulator::ResponseAccumulator;
use agentic_core::types::event::MessageStatus;
use agentic_core::types::io::{CustomToolCall, FunctionToolCall, OutputItem, WebSearchCall};

const CASSETTE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/events");
const TOOL_CALLS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/tool_calls");
const REASONING_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/reasoning/responses");
const CODEX_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/codex");
const WEB_SEARCH_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/web_search");
const CUSTOM_TOOL_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/custom_tool");
const WEB_SEARCH_GATEWAY_MODEL: &str = "Qwen/Qwen3.5-35B-A3B-FP8";
const WEB_SEARCH_GATEWAY_MODEL_SLUG: &str = "Qwen-Qwen3.5-35B-A3B-FP8";
const WEB_SEARCH_OPENAI_MODEL: &str = "gpt-5.6";
const WEB_SEARCH_OPENAI_MODEL_SLUG: &str = "gpt-5.6";

fn from_sse_lines(lines: impl IntoIterator<Item = String>, conversation_id: Option<&str>) -> ResponseAccumulator {
    ResponseAccumulator::from_sse_lines(lines, conversation_id).expect("valid cassette SSE stream")
}

// --- Legacy event cassette format ---

#[derive(Deserialize)]
struct EventCassette {
    sse: Vec<String>,
    expected_function_call: Option<ExpectedFunctionCall>,
    #[allow(dead_code)]
    expected_text: Option<String>,
}

#[derive(Deserialize)]
struct ExpectedFunctionCall {
    name: String,
    arguments: String,
}

fn load_cassette(filename: &str) -> EventCassette {
    let path = format!("{CASSETTE_DIR}/{filename}");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_yml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

// --- New multi-turn cassette format (PR #60) ---

#[derive(Deserialize)]
struct TurnCassette {
    turns: Vec<Turn>,
}

#[derive(Deserialize)]
struct Turn {
    #[allow(dead_code)]
    filename: String,
    #[allow(dead_code)]
    request: serde_yml::Value,
    response: TurnResponse,
}

#[derive(Deserialize)]
struct TurnResponse {
    #[allow(dead_code)]
    headers: serde_yml::Value,
    #[allow(dead_code)]
    status_code: Option<u16>,
    #[serde(default)]
    sse: Vec<String>,
    body: Option<serde_json::Value>,
}

fn load_turn_cassette_from(dir: &str, filename: &str) -> TurnCassette {
    let path = format!("{dir}/{filename}");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_yml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn load_turn_cassette(filename: &str) -> TurnCassette {
    load_turn_cassette_from(TOOL_CALLS_DIR, filename)
}

fn load_reasoning_cassette(filename: &str) -> TurnCassette {
    load_turn_cassette_from(REASONING_DIR, filename)
}

fn load_codex_cassette(filename: &str) -> TurnCassette {
    load_turn_cassette_from(CODEX_DIR, filename)
}

fn load_web_search_cassette(filename: &str) -> TurnCassette {
    load_turn_cassette_from(WEB_SEARCH_DIR, filename)
}

fn load_custom_tool_cassette(filename: &str) -> TurnCassette {
    load_turn_cassette_from(CUSTOM_TOOL_DIR, filename)
}

fn load_web_search_cassette_pair(streaming: bool) -> (TurnCassette, TurnCassette) {
    let mode = if streaming { "streaming" } else { "nonstreaming" };
    let openai = load_web_search_cassette(&format!(
        "web-search-openai-reference-{WEB_SEARCH_OPENAI_MODEL_SLUG}-{mode}.yaml"
    ));
    let gateway = load_web_search_cassette(&format!(
        "web-search-gateway-{WEB_SEARCH_GATEWAY_MODEL_SLUG}-{mode}.yaml"
    ));
    (openai, gateway)
}

/// Extracts `data: ...` lines from raw SSE entries (which may include
/// `event:` lines and blank separators).
fn extract_data_lines(sse_entries: &[String]) -> Vec<String> {
    sse_entries
        .iter()
        .flat_map(|entry| entry.lines())
        .filter(|line| line.starts_with("data: "))
        .map(ToString::to_string)
        .collect()
}

fn response_body_from_data_line(line: &str) -> Option<serde_json::Value> {
    let data = line.strip_prefix("data: ")?;
    if data == "[DONE]" {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    if let Some(response) = value.get("response").and_then(serde_json::Value::as_object) {
        return Some(serde_json::Value::Object(response.clone()));
    }
    if value.get("object").and_then(serde_json::Value::as_str) == Some("response") {
        return Some(value);
    }
    None
}

fn process_completed_response_object_from_sse(
    cassette: &TurnCassette,
    turn_idx: usize,
    model: &str,
) -> Vec<OutputItem> {
    let data_lines = extract_data_lines(&cassette.turns[turn_idx].response.sse);
    let response = data_lines
        .iter()
        .filter_map(|line| response_body_from_data_line(line))
        .find(|value| value.get("status").and_then(serde_json::Value::as_str) == Some("completed"))
        .unwrap_or_else(|| panic!("turn {} must contain a completed response object", turn_idx + 1));
    let body = serde_json::to_string(&response).unwrap();
    let acc = ResponseAccumulator::from_json(&body, None).unwrap();
    let payload = acc.finalize(model, None, None);
    assert_eq!(payload.status, "completed");
    payload.output
}

fn process_codex_streaming_turn(cassette: &TurnCassette, turn_idx: usize, model: &str) -> Vec<OutputItem> {
    let data_lines = extract_data_lines(&cassette.turns[turn_idx].response.sse);
    assert!(
        !data_lines.is_empty(),
        "Codex cassette turn {} must have SSE data lines",
        turn_idx + 1
    );
    let acc = from_sse_lines(data_lines, None);
    let payload = acc.finalize(model, None, None);
    assert_eq!(payload.status, "completed");
    payload.output
}

fn first_function_call(output: &[OutputItem]) -> &FunctionToolCall {
    output
        .iter()
        .find_map(|item| {
            if let OutputItem::FunctionCall(call) = item {
                Some(call)
            } else {
                None
            }
        })
        .expect("output must contain a function call")
}

fn first_custom_tool_call(output: &[OutputItem]) -> &CustomToolCall {
    output
        .iter()
        .find_map(|item| {
            if let OutputItem::CustomToolCall(call) = item {
                Some(call)
            } else {
                None
            }
        })
        .expect("output must contain a custom tool call")
}

fn turn_request_body(turn: &Turn) -> serde_json::Value {
    let body = turn.request.get("body").expect("turn request must have body");
    serde_json::to_value(body).expect("request body must convert to JSON")
}

// --- Helpers ---

fn process_nonstreaming_turn(cassette: &TurnCassette, turn_idx: usize, model: &str) -> Vec<OutputItem> {
    let body = cassette.turns[turn_idx]
        .response
        .body
        .as_ref()
        .unwrap_or_else(|| panic!("turn {} must have response body", turn_idx + 1));
    let body_str = serde_json::to_string(body).unwrap();
    let acc = ResponseAccumulator::from_json(&body_str, None).unwrap();
    let payload = acc.finalize(model, None, None);
    assert_eq!(payload.status, "completed");
    payload.output
}

fn process_streaming_turn(cassette: &TurnCassette, turn_idx: usize, model: &str) -> Vec<OutputItem> {
    let data_lines = extract_data_lines(&cassette.turns[turn_idx].response.sse);
    assert!(
        !data_lines.is_empty(),
        "streaming turn {} must have SSE data lines",
        turn_idx + 1
    );
    let acc = from_sse_lines(data_lines, None);
    let payload = acc.finalize(model, None, None);
    assert_eq!(payload.status, "completed");
    payload.output
}

fn count_function_calls(output: &[OutputItem]) -> usize {
    output
        .iter()
        .filter(|item| matches!(item, OutputItem::FunctionCall(_)))
        .count()
}

fn get_function_call_names(output: &[OutputItem]) -> Vec<String> {
    output
        .iter()
        .filter_map(|item| {
            if let OutputItem::FunctionCall(fc) = item {
                Some(fc.name.clone())
            } else {
                None
            }
        })
        .collect()
}

fn has_reasoning(output: &[OutputItem]) -> bool {
    output.iter().any(|item| matches!(item, OutputItem::Reasoning(_)))
}

fn get_first_fc_arguments(output: &[OutputItem]) -> String {
    output
        .iter()
        .find_map(|item| {
            if let OutputItem::FunctionCall(fc) = item {
                Some(fc.arguments.clone())
            } else {
                None
            }
        })
        .expect("output must contain at least one function call")
}

#[path = "accumulator_cassette/legacy.rs"]
mod legacy;

#[path = "accumulator_cassette/tool_calls.rs"]
mod tool_calls;

#[path = "accumulator_cassette/reasoning.rs"]
mod reasoning;

#[path = "accumulator_cassette/codex.rs"]
mod codex;

#[path = "accumulator_cassette/web_search.rs"]
mod web_search;

#[path = "accumulator_cassette/custom_tools.rs"]
mod custom_tools;

#[path = "accumulator_cassette/stateful.rs"]
mod stateful;
