//! Cassette-driven integration tests: feed real vLLM SSE recordings through
//! the full accumulator pipeline (normalize → `process_event` → finalize) and
//! verify the resulting output items match expected values.
//!
//! Tests cover both the legacy `events/` cassettes (flat SSE list) and the
//! newer `tool_calls/` cassettes from PR #60 (multi-turn `turns` format).

use serde::Deserialize;

use agentic_core::executor::accumulator::ResponseAccumulator;
use agentic_core::types::event::MessageStatus;
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
        assert_eq!(fc.status, MessageStatus::Completed);
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
            assert_eq!(fc.status, MessageStatus::Completed);
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
            assert_eq!(fc.status, MessageStatus::Completed);
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
            assert_eq!(fc.status, MessageStatus::Completed);
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

// === Stateful multi-turn cassette tests (previous_response_id chaining) ===
//
// These cassettes are recorded against gpt-oss-20b with `store=true` and
// `previous_response_id` chaining (via record_cassette.py --mode responses).
// They exercise realistic multi-turn conversations where the server maintains
// conversation state — the key pattern our accumulator must handle for PR #67.
//
// Scenario: SRE debugging a failed ETL pipeline job-382.
// Tools: get_job_status, get_error_logs, search_runbook, run_analysis,
// restart_job, web_search.

const MULTI_TURN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/tool_calls/multi_turn");

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
    let acc = ResponseAccumulator::from_sse_lines(data_lines, None);
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

/// Extracts the `arguments` JSON string from the first function call in output items.
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

// ═══════════════════════════════════════════════════════════════════
// Stateful 3-turn: get_job_status → get_error_logs → search_runbook
// Non-streaming, store=true, previous_response_id chain
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_stateful_responses_3turn_tool_calls() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "responses_tool_calls_3turn.yaml");

    let t1 = process_nonstreaming_turn(&cassette, 0, "openai/gpt-oss-20b");
    let t1_names = get_function_call_names(&t1);
    assert_eq!(count_function_calls(&t1), 1);
    assert_eq!(t1_names, vec!["get_job_status"]);
    assert!(has_reasoning(&t1));

    let t2 = process_nonstreaming_turn(&cassette, 1, "openai/gpt-oss-20b");
    let t2_names = get_function_call_names(&t2);
    assert_eq!(count_function_calls(&t2), 1);
    assert_eq!(t2_names, vec!["get_error_logs"]);

    let t3 = process_nonstreaming_turn(&cassette, 2, "openai/gpt-oss-20b");
    let t3_names = get_function_call_names(&t3);
    assert_eq!(count_function_calls(&t3), 1);
    assert_eq!(t3_names, vec!["search_runbook"]);
}

/// Context retention proof: turn 2 prompt says "that job" (no explicit job ID),
/// but the model resolves it to "job-382" because `previous_response_id` gives
/// it access to turn 1's conversation state.
#[test]
fn test_stateful_responses_3turn_context_retention() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "responses_tool_calls_3turn.yaml");

    // Turn 2 prompt says "that job" — model must resolve from turn 1 context
    let t2 = process_nonstreaming_turn(&cassette, 1, "openai/gpt-oss-20b");
    let t2_args = get_first_fc_arguments(&t2);
    assert!(
        t2_args.contains("job-382"),
        "turn 2 must resolve 'that job' to 'job-382' via retained context, got: {t2_args}"
    );

    // Turn 3 prompt says "those errors" — model must recall turn 2's investigation
    let t3 = process_nonstreaming_turn(&cassette, 2, "openai/gpt-oss-20b");
    let t3_args = get_first_fc_arguments(&t3);
    assert!(
        t3_args.contains("job-382") || t3_args.contains("error") || t3_args.contains("ETL"),
        "turn 3 must reference context from earlier turns, got: {t3_args}"
    );
}

#[test]
fn test_stateful_responses_3turn_null_status_deserialization() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "responses_tool_calls_3turn.yaml");
    for i in 0..3 {
        let output = process_nonstreaming_turn(&cassette, i, "openai/gpt-oss-20b");
        for item in &output {
            if let OutputItem::FunctionCall(fc) = item {
                assert_eq!(
                    fc.status,
                    MessageStatus::Completed,
                    "turn {} function_call status must default to Completed (gpt-oss emits null)",
                    i + 1
                );
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Stateful 5-turn: full investigation pipeline
// get_job_status → get_error_logs → search_runbook → run_analysis → restart_job
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_stateful_responses_5turn_tool_sequence() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "responses_tool_calls_5turn.yaml");

    let expected_tools = [
        "get_job_status",
        "get_error_logs",
        "search_runbook",
        "run_analysis",
        "restart_job",
    ];
    for (i, expected) in expected_tools.iter().enumerate() {
        let output = process_nonstreaming_turn(&cassette, i, "openai/gpt-oss-20b");
        let names = get_function_call_names(&output);
        assert_eq!(names.len(), 1, "turn {} should call exactly 1 tool", i + 1);
        assert_eq!(&names[0], expected, "turn {} should call {expected}", i + 1);
        assert!(has_reasoning(&output), "turn {} should have reasoning", i + 1);
    }
}

/// Context retention proof for 5-turn: turn 5 says "restart it" without naming
/// job-382, but the model resolves correctly because all prior context is retained.
#[test]
fn test_stateful_responses_5turn_context_retention() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "responses_tool_calls_5turn.yaml");

    // Turn 2: "that failed job" → must resolve to job-382
    let t2 = process_nonstreaming_turn(&cassette, 1, "openai/gpt-oss-20b");
    let t2_args = get_first_fc_arguments(&t2);
    assert!(
        t2_args.contains("job-382"),
        "turn 2 'that failed job' must resolve to job-382, got: {t2_args}"
    );

    // Turn 5: "restart it" → must resolve to job-382 with correct params
    let t5 = process_nonstreaming_turn(&cassette, 4, "openai/gpt-oss-20b");
    let t5_args = get_first_fc_arguments(&t5);
    assert!(
        t5_args.contains("job-382"),
        "turn 5 'restart it' must resolve to job-382, got: {t5_args}"
    );
    assert!(
        t5_args.contains("64"),
        "turn 5 must include memory_override_gb=64, got: {t5_args}"
    );
}

#[test]
fn test_stateful_responses_5turn_function_call_fields() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "responses_tool_calls_5turn.yaml");
    for i in 0..5 {
        let output = process_nonstreaming_turn(&cassette, i, "openai/gpt-oss-20b");
        for item in &output {
            if let OutputItem::FunctionCall(fc) = item {
                assert!(!fc.id.is_empty(), "turn {} fc.id must not be empty", i + 1);
                assert!(!fc.call_id.is_empty(), "turn {} fc.call_id must not be empty", i + 1);
                assert!(!fc.name.is_empty(), "turn {} fc.name must not be empty", i + 1);
                assert!(
                    !fc.arguments.is_empty(),
                    "turn {} fc.arguments must not be empty",
                    i + 1
                );
                assert_eq!(fc.status, MessageStatus::Completed);
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Stateful 3-turn streaming: SSE events with previous_response_id
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_stateful_responses_streaming_3turn() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "responses_tool_calls_3turn_streaming.yaml");
    assert_eq!(cassette.turns.len(), 3);

    for i in 0..3 {
        let output = process_streaming_turn(&cassette, i, "openai/gpt-oss-20b");
        assert!(
            count_function_calls(&output) >= 1,
            "streaming turn {} must produce at least one function_call",
            i + 1
        );
        for item in &output {
            if let OutputItem::FunctionCall(fc) = item {
                assert!(!fc.call_id.is_empty(), "streaming fc must have call_id");
                assert!(!fc.name.is_empty(), "streaming fc must have name");
                assert!(!fc.arguments.is_empty(), "streaming fc must have arguments");
                assert_eq!(fc.status, MessageStatus::Completed);
            }
        }
    }
}

/// Context retention in streaming mode: turn 2 says "that job" and the model
/// resolves it to "job-382" even in streaming (SSE) delivery.
#[test]
fn test_stateful_responses_streaming_context_retention() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "responses_tool_calls_3turn_streaming.yaml");

    // Turn 2: "that job" → must resolve to job-382 in streaming mode
    let t2 = process_streaming_turn(&cassette, 1, "openai/gpt-oss-20b");
    let t2_args = get_first_fc_arguments(&t2);
    assert!(
        t2_args.contains("job-382"),
        "streaming turn 2 must resolve 'that job' to job-382, got: {t2_args}"
    );
}

// ═══════════════════════════════════════════════════════════════════
// Branching: turn 3 diverges from turn 1 (not turn 2)
// Tests previous_response_id pointing back to an earlier response
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_stateful_responses_branch_divergence() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "responses_tool_calls_branch.yaml");
    assert_eq!(cassette.turns.len(), 3);

    // Turn 1: no prev_id
    let body1 = cassette.turns[0].request.as_mapping().unwrap();
    let req1 = body1
        .get(serde_yml::Value::String("body".into()))
        .and_then(serde_yml::Value::as_mapping)
        .unwrap();
    let prev1 = req1.get(serde_yml::Value::String("previous_response_id".into()));
    assert!(prev1.is_none() || prev1.unwrap().is_null());

    // Turn 2: prev_id = turn 1's response id
    let body2 = cassette.turns[1].request.as_mapping().unwrap();
    let req2 = body2
        .get(serde_yml::Value::String("body".into()))
        .and_then(serde_yml::Value::as_mapping)
        .unwrap();
    let prev2 = req2
        .get(serde_yml::Value::String("previous_response_id".into()))
        .and_then(serde_yml::Value::as_str)
        .expect("turn 2 must have prev_id");

    // Turn 3: prev_id = turn 1's response id (branches back, NOT from turn 2)
    let body3 = cassette.turns[2].request.as_mapping().unwrap();
    let req3 = body3
        .get(serde_yml::Value::String("body".into()))
        .and_then(serde_yml::Value::as_mapping)
        .unwrap();
    let prev3 = req3
        .get(serde_yml::Value::String("previous_response_id".into()))
        .and_then(serde_yml::Value::as_str)
        .expect("turn 3 must have prev_id");

    // Turn 2 and Turn 3 both point to the same response (turn 1)
    assert_eq!(
        prev2, prev3,
        "branch: turn 3 should reference same prev_id as turn 2 (turn 1's response)"
    );
}

#[test]
fn test_stateful_responses_branch_all_turns_parse() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "responses_tool_calls_branch.yaml");
    for i in 0..3 {
        let output = process_nonstreaming_turn(&cassette, i, "openai/gpt-oss-20b");
        assert!(
            count_function_calls(&output) >= 1,
            "branch turn {} must produce a function_call",
            i + 1
        );
        assert!(has_reasoning(&output), "branch turn {} should have reasoning", i + 1);
    }
}

// ═══════════════════════════════════════════════════════════════════
// Cross-cassette: all stateful cassettes parse without error
// ═══════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════
// Tool-output-only turn: model responds autonomously with text
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_stateful_responses_tool_output_only_produces_text() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "responses_tool_calls_tool_output_only.yaml");
    assert_eq!(cassette.turns.len(), 3);

    // Turn 2 has tool output only (no user message) → model should produce text
    let t2 = process_nonstreaming_turn(&cassette, 1, "openai/gpt-oss-20b");
    let has_text = t2.iter().any(|item| matches!(item, OutputItem::Message(_)));
    assert!(has_text, "tool-output-only turn should produce a text response");
}

// ═══════════════════════════════════════════════════════════════════
// Parallel tool calls (OpenAI only — gpt-4o reliably produces these)
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_openai_parallel_tool_calls() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "openai_responses_tool_calls_parallel.yaml");
    assert_eq!(cassette.turns.len(), 3);

    // Turn 1 should have 2 parallel function calls
    let t1 = process_nonstreaming_turn(&cassette, 0, "gpt-4o");
    let t1_names = get_function_call_names(&t1);
    assert!(
        t1_names.len() >= 2,
        "parallel cassette turn 1 must have 2+ function calls, got: {t1_names:?}"
    );
    assert!(t1_names.contains(&"get_job_status".to_string()));
    assert!(t1_names.contains(&"web_search".to_string()));
}

/// Verifies that the request input for turn 2 contains multiple `function_call_output`
/// items (one per parallel call from turn 1).
#[test]
fn test_openai_parallel_tool_outputs_in_request() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "openai_responses_tool_calls_parallel.yaml");

    let body2 = cassette.turns[1].request.as_mapping().unwrap();
    let req2 = body2
        .get(serde_yml::Value::String("body".into()))
        .and_then(serde_yml::Value::as_mapping)
        .unwrap();
    let input2 = req2
        .get(serde_yml::Value::String("input".into()))
        .expect("turn 2 must have input");
    let input_seq = input2.as_sequence().expect("turn 2 input must be a list");

    let tool_outputs: Vec<_> = input_seq
        .iter()
        .filter(|item| {
            item.as_mapping()
                .and_then(|m| m.get(serde_yml::Value::String("type".into())))
                .and_then(serde_yml::Value::as_str)
                == Some("function_call_output")
        })
        .collect();

    assert!(
        tool_outputs.len() >= 2,
        "turn 2 input must contain 2+ function_call_output items for parallel calls, got {}",
        tool_outputs.len()
    );
}

// ═══════════════════════════════════════════════════════════════════
// OpenAI cassettes: verify they parse identically to vLLM
// (status is "completed" string, not null)
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_openai_3turn_parses_and_retains_context() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "openai_responses_tool_calls_3turn.yaml");
    assert_eq!(cassette.turns.len(), 3);

    let t1 = process_nonstreaming_turn(&cassette, 0, "gpt-4o");
    assert_eq!(get_function_call_names(&t1), vec!["get_job_status"]);

    // Context retention: turn 2 says "that job"
    let t2 = process_nonstreaming_turn(&cassette, 1, "gpt-4o");
    let t2_args = get_first_fc_arguments(&t2);
    assert!(
        t2_args.contains("job-382"),
        "OpenAI turn 2 must resolve 'that job' to job-382, got: {t2_args}"
    );
}

#[test]
fn test_openai_5turn_full_sequence() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "openai_responses_tool_calls_5turn.yaml");
    assert_eq!(cassette.turns.len(), 5);

    let expected_tools = [
        "get_job_status",
        "get_error_logs",
        "search_runbook",
        "run_analysis",
        "restart_job",
    ];
    for (i, expected) in expected_tools.iter().enumerate() {
        let output = process_nonstreaming_turn(&cassette, i, "gpt-4o");
        let names = get_function_call_names(&output);
        assert_eq!(names.len(), 1, "OpenAI turn {} should call 1 tool", i + 1);
        assert_eq!(&names[0], expected, "OpenAI turn {} should call {expected}", i + 1);
    }
}

#[test]
fn test_openai_streaming_3turn() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "openai_responses_tool_calls_3turn_streaming.yaml");
    assert_eq!(cassette.turns.len(), 3);

    for i in 0..3 {
        let output = process_streaming_turn(&cassette, i, "gpt-4o");
        assert!(
            count_function_calls(&output) >= 1,
            "OpenAI streaming turn {} must produce a function_call",
            i + 1
        );
    }
}

#[test]
fn test_openai_branch_divergence() {
    let cassette = load_turn_cassette_from(MULTI_TURN_DIR, "openai_responses_tool_calls_branch.yaml");
    assert_eq!(cassette.turns.len(), 3);

    let body2 = cassette.turns[1].request.as_mapping().unwrap();
    let req2 = body2
        .get(serde_yml::Value::String("body".into()))
        .and_then(serde_yml::Value::as_mapping)
        .unwrap();
    let prev2 = req2
        .get(serde_yml::Value::String("previous_response_id".into()))
        .and_then(serde_yml::Value::as_str)
        .expect("turn 2 must have prev_id");

    let body3 = cassette.turns[2].request.as_mapping().unwrap();
    let req3 = body3
        .get(serde_yml::Value::String("body".into()))
        .and_then(serde_yml::Value::as_mapping)
        .unwrap();
    let prev3 = req3
        .get(serde_yml::Value::String("previous_response_id".into()))
        .and_then(serde_yml::Value::as_str)
        .expect("turn 3 must have prev_id");

    assert_eq!(
        prev2, prev3,
        "OpenAI branch: turn 3 must branch from turn 1 (same prev_id as turn 2)"
    );
}

// ═══════════════════════════════════════════════════════════════════
// Cross-cassette: ALL stateful cassettes parse without error
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_all_stateful_cassettes_parse_without_error() {
    let nonstreaming = [
        "responses_tool_calls_3turn.yaml",
        "responses_tool_calls_5turn.yaml",
        "responses_tool_calls_branch.yaml",
        "responses_tool_calls_parallel.yaml",
        "responses_tool_calls_tool_output_only.yaml",
        "openai_responses_tool_calls_3turn.yaml",
        "openai_responses_tool_calls_5turn.yaml",
        "openai_responses_tool_calls_branch.yaml",
        "openai_responses_tool_calls_parallel.yaml",
        "openai_responses_tool_calls_tool_output_only.yaml",
    ];

    for filename in &nonstreaming {
        let cassette = load_turn_cassette_from(MULTI_TURN_DIR, filename);
        for i in 0..cassette.turns.len() {
            let body = cassette.turns[i]
                .response
                .body
                .as_ref()
                .unwrap_or_else(|| panic!("{filename} turn {i} must have body"));
            let body_str = serde_json::to_string(body).unwrap();
            let result = ResponseAccumulator::from_json(&body_str, None);
            assert!(
                result.is_ok(),
                "{filename} turn {} failed to parse: {:?}",
                i + 1,
                result.err()
            );
            let payload = result.unwrap().finalize("gpt-4o", None, None);
            assert_eq!(
                payload.status,
                "completed",
                "{filename} turn {} status != completed",
                i + 1
            );
        }
    }

    let streaming = [
        "responses_tool_calls_3turn_streaming.yaml",
        "openai_responses_tool_calls_3turn_streaming.yaml",
    ];
    for filename in &streaming {
        let cassette = load_turn_cassette_from(MULTI_TURN_DIR, filename);
        for i in 0..cassette.turns.len() {
            let data_lines = extract_data_lines(&cassette.turns[i].response.sse);
            assert!(
                !data_lines.is_empty(),
                "{filename} turn {} has no SSE data lines",
                i + 1
            );
            let acc = ResponseAccumulator::from_sse_lines(data_lines, None);
            let payload = acc.finalize("gpt-4o", None, None);
            assert_eq!(
                payload.status,
                "completed",
                "{filename} turn {} status != completed",
                i + 1
            );
        }
    }
}
