//! Cassette-driven integration tests: feed real vLLM SSE recordings through
//! the full accumulator pipeline (normalize → `process_event` → finalize) and
//! verify the resulting output items match expected values.
//!
//! Tests cover both the legacy `events/` cassettes (flat SSE list) and the
//! newer `tool_calls/` cassettes from PR #60 (multi-turn `turns` format).

use serde::Deserialize;

use agentic_core::executor::accumulator::ResponseAccumulator;
use agentic_core::types::io::OutputItem;

const CASSETTE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/events");
const TOOL_CALLS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/tool_calls");
const REASONING_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/reasoning/responses");

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

// === Legacy cassette tests ===

/// Feeds a real vLLM `function_call` SSE recording through the accumulator and
/// verifies the output contains the correct `FunctionCall` item.
#[test]
fn test_accumulator_cassette_function_call_vllm_gemma4() {
    let cassette = load_cassette("function-call-vllm-gemma4.yaml");
    let expected_fc = cassette
        .expected_function_call
        .expect("cassette must have expected_function_call");

    let acc = ResponseAccumulator::from_sse_lines(cassette.sse, None);
    let payload = acc.finalize("google/gemma-4-26B-A4B-it", None, None);

    assert_eq!(payload.status, "completed");
    assert_eq!(payload.output.len(), 1, "expected exactly one output item");

    if let OutputItem::FunctionCall(fc) = &payload.output[0] {
        assert_eq!(fc.name, expected_fc.name);
        assert_eq!(fc.arguments, expected_fc.arguments);
        assert_eq!(fc.status, "completed");
        assert!(!fc.call_id.is_empty(), "call_id should be populated");
        assert!(!fc.id.is_empty(), "id should be populated");
    } else {
        panic!("expected OutputItem::FunctionCall, got {:?}", payload.output[0]);
    }

    assert!(payload.usage.is_some(), "usage should be present");
    let usage = payload.usage.unwrap();
    assert_eq!(usage.input_tokens, 66);
    assert_eq!(usage.output_tokens, 21);
    assert_eq!(usage.total_tokens, 87);
}

/// Feeds the text-only cassette through the accumulator and verifies no
/// `function_call` items leak in — regression guard for type-aware branching.
#[test]
fn test_accumulator_cassette_text_only_no_function_calls() {
    let cassette = load_cassette("text-only-vllm-gemma4.yaml");

    let acc = ResponseAccumulator::from_sse_lines(cassette.sse, None);
    let payload = acc.finalize("google/gemma-4-26B-A4B-it", None, None);

    assert_eq!(payload.status, "completed");
    for item in &payload.output {
        assert!(
            matches!(item, OutputItem::Message(_)),
            "text-only cassette should only produce Message items, got {item:?}"
        );
    }
}

// === PR #60 tool_calls cassette tests ===

/// `tool_choice=auto` streaming: model decides to call multiple tools (parallel tool use).
#[test]
fn test_tool_calls_cassette_auto_streaming() {
    let cassette = load_turn_cassette("tool-call-auto-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml");
    let turn = &cassette.turns[0];
    let data_lines = extract_data_lines(&turn.response.sse);

    let acc = ResponseAccumulator::from_sse_lines(data_lines, None);
    let payload = acc.finalize("Qwen/Qwen3-30B-A3B-FP8", None, None);

    assert_eq!(payload.status, "completed");

    let function_calls: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::FunctionCall(_)))
        .collect();

    assert!(
        !function_calls.is_empty(),
        "auto mode should produce at least one function call"
    );

    for item in &function_calls {
        if let OutputItem::FunctionCall(fc) = item {
            assert!(!fc.name.is_empty(), "function call name must not be empty");
            assert!(!fc.arguments.is_empty(), "function call arguments must not be empty");
            assert_eq!(fc.status, "completed");
            assert!(!fc.call_id.is_empty(), "call_id must be populated");
        }
    }

    assert!(payload.usage.is_some());
}

/// `tool_choice=required` streaming: model is forced to call a tool.
#[test]
fn test_tool_calls_cassette_required_streaming() {
    let cassette = load_turn_cassette("tool-call-required-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml");
    let turn = &cassette.turns[0];
    let data_lines = extract_data_lines(&turn.response.sse);

    let acc = ResponseAccumulator::from_sse_lines(data_lines, None);
    let payload = acc.finalize("Qwen/Qwen3-30B-A3B-FP8", None, None);

    assert_eq!(payload.status, "completed");

    let function_calls: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::FunctionCall(_)))
        .collect();

    assert!(
        !function_calls.is_empty(),
        "required mode must produce at least one function call"
    );

    for item in &function_calls {
        if let OutputItem::FunctionCall(fc) = item {
            assert_eq!(fc.status, "completed");
        }
    }
}

/// `tool_choice=named` streaming: model calls a specific named tool.
#[test]
fn test_tool_calls_cassette_named_streaming() {
    let cassette = load_turn_cassette("tool-call-named-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml");
    let turn = &cassette.turns[0];
    let data_lines = extract_data_lines(&turn.response.sse);

    let acc = ResponseAccumulator::from_sse_lines(data_lines, None);
    let payload = acc.finalize("Qwen/Qwen3-30B-A3B-FP8", None, None);

    assert_eq!(payload.status, "completed");

    let function_calls: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::FunctionCall(_)))
        .collect();

    assert!(
        !function_calls.is_empty(),
        "named mode must produce at least one function call"
    );
}

/// `tool_choice=none` streaming: model should NOT call any tools.
#[test]
fn test_tool_calls_cassette_none_streaming() {
    let cassette = load_turn_cassette("tool-call-none-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml");
    let turn = &cassette.turns[0];
    let data_lines = extract_data_lines(&turn.response.sse);

    let acc = ResponseAccumulator::from_sse_lines(data_lines, None);
    let payload = acc.finalize("Qwen/Qwen3-30B-A3B-FP8", None, None);

    assert_eq!(payload.status, "completed");

    let function_calls: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::FunctionCall(_)))
        .collect();

    assert!(
        function_calls.is_empty(),
        "none mode should produce zero function calls, got {}",
        function_calls.len()
    );

    assert!(
        !payload.output.is_empty(),
        "none mode should still produce message output"
    );
}

// === Non-streaming tool_calls cassette tests (exercises `from_json` path) ===

/// `tool_choice=auto` non-streaming: JSON response with parallel function calls.
#[test]
fn test_tool_calls_cassette_auto_nonstreaming() {
    let cassette = load_turn_cassette("tool-call-auto-Qwen-Qwen3-30B-A3B-FP8-nonstreaming.yaml");
    let body = cassette.turns[0]
        .response
        .body
        .as_ref()
        .expect("non-streaming cassette must have body");
    let body_str = serde_json::to_string(body).unwrap();

    let acc = ResponseAccumulator::from_json(&body_str, None).unwrap();
    let payload = acc.finalize("Qwen/Qwen3-30B-A3B-FP8", None, None);

    assert_eq!(payload.status, "completed");

    let function_calls: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::FunctionCall(_)))
        .collect();

    assert!(
        !function_calls.is_empty(),
        "auto mode should produce at least one function call"
    );

    for item in &function_calls {
        if let OutputItem::FunctionCall(fc) = item {
            assert!(!fc.name.is_empty());
            assert!(!fc.arguments.is_empty());
            assert_eq!(fc.status, "completed");
            assert!(!fc.call_id.is_empty());
        }
    }
}

/// `tool_choice=required` non-streaming: forced tool call in JSON response.
#[test]
fn test_tool_calls_cassette_required_nonstreaming() {
    let cassette = load_turn_cassette("tool-call-required-Qwen-Qwen3-30B-A3B-FP8-nonstreaming.yaml");
    let body = cassette.turns[0]
        .response
        .body
        .as_ref()
        .expect("non-streaming cassette must have body");
    let body_str = serde_json::to_string(body).unwrap();

    let acc = ResponseAccumulator::from_json(&body_str, None).unwrap();
    let payload = acc.finalize("Qwen/Qwen3-30B-A3B-FP8", None, None);

    assert_eq!(payload.status, "completed");

    let function_calls: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::FunctionCall(_)))
        .collect();

    assert!(
        !function_calls.is_empty(),
        "required mode must produce at least one function call"
    );
}

/// `tool_choice=named` non-streaming: specific named tool in JSON response.
#[test]
fn test_tool_calls_cassette_named_nonstreaming() {
    let cassette = load_turn_cassette("tool-call-named-Qwen-Qwen3-30B-A3B-FP8-nonstreaming.yaml");
    let body = cassette.turns[0]
        .response
        .body
        .as_ref()
        .expect("non-streaming cassette must have body");
    let body_str = serde_json::to_string(body).unwrap();

    let acc = ResponseAccumulator::from_json(&body_str, None).unwrap();
    let payload = acc.finalize("Qwen/Qwen3-30B-A3B-FP8", None, None);

    assert_eq!(payload.status, "completed");

    let function_calls: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::FunctionCall(_)))
        .collect();

    assert!(
        !function_calls.is_empty(),
        "named mode must produce at least one function call"
    );
}

/// `tool_choice=none` non-streaming: no function calls in JSON response.
#[test]
fn test_tool_calls_cassette_none_nonstreaming() {
    let cassette = load_turn_cassette("tool-call-none-Qwen-Qwen3-30B-A3B-FP8-nonstreaming.yaml");
    let body = cassette.turns[0]
        .response
        .body
        .as_ref()
        .expect("non-streaming cassette must have body");
    let body_str = serde_json::to_string(body).unwrap();

    let acc = ResponseAccumulator::from_json(&body_str, None).unwrap();
    let payload = acc.finalize("Qwen/Qwen3-30B-A3B-FP8", None, None);

    assert_eq!(payload.status, "completed");

    let function_calls: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::FunctionCall(_)))
        .collect();

    assert!(
        function_calls.is_empty(),
        "none mode should produce zero function calls, got {}",
        function_calls.len()
    );
}

// === Reasoning cassette tests (regression guard for reasoning + function_call coexistence) ===

/// Reasoning streaming (Qwen3): accumulator produces `Reasoning` + `Message` items.
#[test]
fn test_reasoning_cassette_qwen3_streaming() {
    let cassette = load_reasoning_cassette("reasoning-single-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml");
    let turn = &cassette.turns[0];
    let data_lines = extract_data_lines(&turn.response.sse);

    let acc = ResponseAccumulator::from_sse_lines(data_lines, None);
    let payload = acc.finalize("Qwen/Qwen3-30B-A3B-FP8", None, None);

    assert_eq!(payload.status, "completed");

    let reasoning_items: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::Reasoning(_)))
        .collect();

    let message_items: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::Message(_)))
        .collect();

    assert!(
        !reasoning_items.is_empty(),
        "reasoning cassette must produce at least one Reasoning item"
    );
    assert!(
        !message_items.is_empty(),
        "reasoning cassette should also produce a Message item"
    );

    // No function calls should leak in
    let function_calls: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::FunctionCall(_)))
        .collect();
    assert!(
        function_calls.is_empty(),
        "reasoning-only cassette should not produce function calls"
    );
}

/// Reasoning streaming (GPT-oss): validates accumulator handles different model's reasoning format.
/// Note: GPT-oss emits `output_text.done` without a preceding `output_item.added` for the
/// message, so the accumulator only captures the reasoning item from the streaming path.
/// The message content is available in the `response.completed` payload's output array.
#[test]
fn test_reasoning_cassette_gpt_oss_streaming() {
    let cassette = load_reasoning_cassette("reasoning-single-openai-gpt-oss-20b-streaming.yaml");
    let turn = &cassette.turns[0];
    let data_lines = extract_data_lines(&turn.response.sse);

    let acc = ResponseAccumulator::from_sse_lines(data_lines, None);
    let payload = acc.finalize("openai/gpt-oss-20b", None, None);

    assert_eq!(payload.status, "completed");

    let reasoning_items: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::Reasoning(_)))
        .collect();

    assert!(
        !reasoning_items.is_empty(),
        "GPT-oss reasoning cassette must produce at least one Reasoning item"
    );

    // No function calls should leak in
    let function_calls: Vec<_> = payload
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::FunctionCall(_)))
        .collect();
    assert!(
        function_calls.is_empty(),
        "reasoning-only cassette should not produce function calls"
    );
}
