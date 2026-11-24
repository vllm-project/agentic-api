use super::*;

// === Legacy cassette tests ===

/// Feeds a real vLLM `function_call` SSE recording through the accumulator and
/// verifies the output contains the correct `FunctionCall` item.
#[test]
fn test_accumulator_cassette_function_call_vllm_gemma4() {
    let cassette = load_cassette("function-call-vllm-gemma4.yaml");
    let expected_fc = cassette
        .expected_function_call
        .expect("cassette must have expected_function_call");

    let acc = from_sse_lines(cassette.sse, None);
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

    let acc = from_sse_lines(cassette.sse, None);
    let payload = acc.finalize("google/gemma-4-26B-A4B-it", None, None);

    assert_eq!(payload.status, "completed");
    for item in &payload.output {
        assert!(
            matches!(item, OutputItem::Message(_)),
            "text-only cassette should only produce Message items, got {item:?}"
        );
    }
}
