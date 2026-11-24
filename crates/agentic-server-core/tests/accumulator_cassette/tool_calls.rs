use super::*;

// === PR #60 tool_calls cassette tests ===

/// `tool_choice=auto` streaming: model decides to call multiple tools (parallel tool use).
#[test]
fn test_tool_calls_cassette_auto_streaming() {
    let cassette = load_turn_cassette("tool-call-auto-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml");
    let turn = &cassette.turns[0];
    let data_lines = extract_data_lines(&turn.response.sse);

    let acc = from_sse_lines(data_lines, None);
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

    let acc = from_sse_lines(data_lines, None);
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

    let acc = from_sse_lines(data_lines, None);
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

    let acc = from_sse_lines(data_lines, None);
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
