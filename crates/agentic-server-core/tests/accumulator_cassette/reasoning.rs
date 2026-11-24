use super::*;

// === Reasoning cassette tests (regression guard for reasoning + function_call coexistence) ===

/// Reasoning streaming (Qwen3): accumulator produces `Reasoning` + `Message` items.
#[test]
fn test_reasoning_cassette_qwen3_streaming() {
    let cassette = load_reasoning_cassette("reasoning-single-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml");
    let turn = &cassette.turns[0];
    let data_lines = extract_data_lines(&turn.response.sse);

    let acc = from_sse_lines(data_lines, None);
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

    let acc = from_sse_lines(data_lines, None);
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
