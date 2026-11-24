use super::*;

#[test]
fn test_custom_tool_cassettes_accumulate_public_streaming_shape() {
    let cases = [
        (
            "gateway",
            "custom-tool-gateway-Qwen-Qwen3.5-35B-A3B-FP8-streaming.yaml",
            "Qwen/Qwen3.5-35B-A3B-FP8",
        ),
        (
            "OpenAI",
            "custom-tool-openai-reference-gpt-5.6-streaming.yaml",
            "gpt-5.6",
        ),
    ];

    for (provider, filename, model) in cases {
        let cassette = load_custom_tool_cassette(filename);
        assert_eq!(cassette.turns.len(), 2, "{provider} cassette should contain two turns");

        let output = process_codex_streaming_turn(&cassette, 0, model);
        assert!(
            !output.iter().any(|item| matches!(item, OutputItem::FunctionCall(_))),
            "{provider} must not expose the normalized function call"
        );
        let call = first_custom_tool_call(&output);
        assert!(call.id.starts_with("ctc_"), "{provider} custom item ID");
        assert!(!call.call_id.is_empty(), "{provider} custom call ID");
        assert_eq!(call.name, "agentic_raw_echo", "{provider} custom tool name");
        assert_eq!(call.input.trim(), "CUSTOM_CASSETTE_OK", "{provider} custom input");
        assert_eq!(call.status, Some(MessageStatus::Completed), "{provider} custom status");

        let continuation = process_codex_streaming_turn(&cassette, 1, model);
        assert!(continuation.iter().any(|item| {
            matches!(
                item,
                OutputItem::Message(message)
                    if message.content.iter().any(|content| content.text().contains("CUSTOM_CASSETTE_OUTPUT_OK"))
            )
        }));
    }
}

#[test]
fn test_custom_tool_cassettes_accumulate_public_nonstreaming_shape() {
    let cases = [
        (
            "gateway",
            "custom-tool-gateway-Qwen-Qwen3.5-35B-A3B-FP8-nonstreaming.yaml",
            "Qwen/Qwen3.5-35B-A3B-FP8",
        ),
        (
            "OpenAI",
            "custom-tool-openai-reference-gpt-5.6-nonstreaming.yaml",
            "gpt-5.6",
        ),
    ];

    for (provider, filename, model) in cases {
        let cassette = load_custom_tool_cassette(filename);
        assert_eq!(cassette.turns.len(), 2, "{provider} cassette should contain two turns");

        let output = process_nonstreaming_turn(&cassette, 0, model);
        assert!(
            !output.iter().any(|item| matches!(item, OutputItem::FunctionCall(_))),
            "{provider} must not expose the normalized function call"
        );
        let call = first_custom_tool_call(&output);
        assert!(call.id.starts_with("ctc_"), "{provider} custom item ID");
        assert!(!call.call_id.is_empty(), "{provider} custom call ID");
        assert_eq!(call.name, "agentic_raw_echo", "{provider} custom tool name");
        assert_eq!(call.input.trim(), "CUSTOM_CASSETTE_OK", "{provider} custom input");
        assert_eq!(call.status, Some(MessageStatus::Completed), "{provider} custom status");

        let continuation = process_nonstreaming_turn(&cassette, 1, model);
        assert!(continuation.iter().any(|item| {
            matches!(
                item,
                OutputItem::Message(message)
                    if message.content.iter().any(|content| content.text().contains("CUSTOM_CASSETTE_OUTPUT_OK"))
            )
        }));
    }
}

// ═══════════════════════════════════════════════════════════════════
// Stateful 3-turn: get_job_status → get_error_logs → search_runbook
// Non-streaming, store=true, previous_response_id chain
// ═══════════════════════════════════════════════════════════════════
