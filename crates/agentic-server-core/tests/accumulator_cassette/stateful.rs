use super::*;

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
            let acc = from_sse_lines(data_lines, None);
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
