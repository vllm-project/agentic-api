use super::*;

// === Codex integration cassettes ===

#[test]
fn test_codex_gateway_http_cassettes_parse_completed_response_objects() {
    let cases = [
        (
            "codex-gateway-http-function-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            None,
            "agentic_plain_echo",
        ),
        (
            "codex-gateway-http-namespace-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            Some("mcp__agentic_fixture"),
            "add_numbers",
        ),
    ];

    for (filename, expected_namespace, expected_name) in cases {
        let cassette = load_codex_cassette(filename);
        assert_eq!(cassette.turns.len(), 2, "{filename} should have two turns");

        let output = process_completed_response_object_from_sse(&cassette, 0, "Qwen/Qwen3.6-35B-A3B");
        let call = first_function_call(&output);
        assert_eq!(call.namespace.as_deref(), expected_namespace, "{filename} namespace");
        assert_eq!(call.name, expected_name, "{filename} function name");
        assert_eq!(call.status, MessageStatus::Completed);
        assert!(!call.call_id.is_empty(), "{filename} call_id must be populated");
        assert!(!call.arguments.is_empty(), "{filename} arguments must be populated");
    }
}

#[test]
fn test_codex_gateway_websocket_cassettes_preserve_function_and_namespace_calls() {
    let cases = [
        (
            "codex-gateway-websocket-function-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            None,
            "agentic_plain_echo",
        ),
        (
            "codex-gateway-websocket-namespace-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            Some("mcp__agentic_fixture"),
            "add_numbers",
        ),
    ];

    for (filename, expected_namespace, expected_name) in cases {
        let cassette = load_codex_cassette(filename);
        assert_eq!(cassette.turns.len(), 2, "{filename} should have two turns");

        let output = process_codex_streaming_turn(&cassette, 0, "Qwen/Qwen3.6-35B-A3B");
        let call = first_function_call(&output);
        assert_eq!(call.namespace.as_deref(), expected_namespace, "{filename} namespace");
        assert_eq!(call.name, expected_name, "{filename} function name");
        assert_eq!(call.status, MessageStatus::Completed);
    }
}

#[test]
fn test_codex_custom_tool_cassettes_preserve_raw_input() {
    let gateway_http = load_codex_cassette("codex-gateway-http-custom-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml");
    let gateway_http_output = process_completed_response_object_from_sse(&gateway_http, 0, "Qwen/Qwen3.6-35B-A3B");

    let gateway_websocket =
        load_codex_cassette("codex-gateway-websocket-custom-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml");
    let gateway_websocket_output = process_codex_streaming_turn(&gateway_websocket, 0, "Qwen/Qwen3.6-35B-A3B");

    let direct_vllm = load_codex_cassette("codex-direct-vllm-http-custom-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml");
    let direct_vllm_output = process_codex_streaming_turn(&direct_vllm, 0, "Qwen/Qwen3.6-35B-A3B");

    let openai_https = load_codex_cassette("codex-openai-https-custom-tool-gpt-5.6-streaming.yaml");
    let openai_https_output = process_codex_streaming_turn(&openai_https, 0, "gpt-5.6");

    let openai_websocket = load_codex_cassette("codex-openai-websocket-custom-tool-gpt-5.6-streaming.yaml");
    let openai_websocket_output = process_codex_streaming_turn(&openai_websocket, 0, "gpt-5.6");

    for (label, output) in [
        ("gateway HTTP", gateway_http_output),
        ("gateway WebSocket", gateway_websocket_output),
        ("direct vLLM HTTP", direct_vllm_output),
        ("OpenAI HTTPS", openai_https_output),
        ("OpenAI WebSocket", openai_websocket_output),
    ] {
        let call = first_custom_tool_call(&output);
        assert_eq!(call.name, "agentic_raw_echo", "{label} custom tool name");
        assert_eq!(call.input, "CUSTOM_CASSETTE_OK", "{label} raw custom input");
        assert_eq!(call.status, Some(MessageStatus::Completed), "{label} custom status");
        assert!(!call.call_id.is_empty(), "{label} call_id must be populated");
    }
}

#[test]
fn test_codex_direct_vllm_http_cassettes_capture_upstream_tool_shapes() {
    let cases = [
        (
            "codex-direct-vllm-http-function-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            None,
            "agentic_plain_echo",
        ),
        (
            "codex-direct-vllm-http-flat-namespace-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            None,
            "agentic_ns__mcp__agentic_fixture__add_numbers",
        ),
    ];

    for (filename, expected_namespace, expected_name) in cases {
        let cassette = load_codex_cassette(filename);
        assert_eq!(cassette.turns.len(), 2, "{filename} should have two turns");

        let output = process_codex_streaming_turn(&cassette, 0, "Qwen/Qwen3.6-35B-A3B");
        let call = first_function_call(&output);
        assert_eq!(call.namespace.as_deref(), expected_namespace, "{filename} namespace");
        assert_eq!(call.name, expected_name, "{filename} function name");
        assert_eq!(call.status, MessageStatus::Completed);
    }
}

#[test]
fn test_codex_openai_baseline_cassettes_accept_namespace_on_http_and_websocket() {
    let cases = [
        (
            "codex-openai-https-function-tool-gpt-4o-streaming.yaml",
            None,
            "agentic_plain_echo",
        ),
        (
            "codex-openai-https-namespace-tool-gpt-4o-streaming.yaml",
            Some("mcp__agentic_fixture"),
            "add_numbers",
        ),
        (
            "codex-openai-websocket-function-tool-gpt-4o-streaming.yaml",
            None,
            "agentic_plain_echo",
        ),
        (
            "codex-openai-websocket-namespace-tool-gpt-4o-streaming.yaml",
            Some("mcp__agentic_fixture"),
            "add_numbers",
        ),
    ];

    for (filename, expected_namespace, expected_name) in cases {
        let cassette = load_codex_cassette(filename);
        assert_eq!(cassette.turns.len(), 2, "{filename} should have two turns");

        let output = process_codex_streaming_turn(&cassette, 0, "gpt-4o");
        let call = first_function_call(&output);
        assert_eq!(call.namespace.as_deref(), expected_namespace, "{filename} namespace");
        assert_eq!(call.name, expected_name, "{filename} function name");
        assert_eq!(call.status, MessageStatus::Completed);
    }
}

#[test]
fn test_codex_cassette_second_turns_are_tool_output_continuations() {
    let cassettes = [
        (
            "codex-direct-vllm-http-custom-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            "custom_tool_call_output",
        ),
        (
            "codex-direct-vllm-http-flat-namespace-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            "function_call_output",
        ),
        (
            "codex-direct-vllm-http-function-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            "function_call_output",
        ),
        (
            "codex-gateway-http-custom-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            "custom_tool_call_output",
        ),
        (
            "codex-gateway-http-function-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            "function_call_output",
        ),
        (
            "codex-gateway-http-namespace-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            "function_call_output",
        ),
        (
            "codex-gateway-websocket-custom-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            "custom_tool_call_output",
        ),
        (
            "codex-gateway-websocket-function-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            "function_call_output",
        ),
        (
            "codex-gateway-websocket-namespace-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            "function_call_output",
        ),
        (
            "codex-openai-https-function-tool-gpt-4o-streaming.yaml",
            "function_call_output",
        ),
        (
            "codex-openai-https-custom-tool-gpt-5.6-streaming.yaml",
            "custom_tool_call_output",
        ),
        (
            "codex-openai-https-namespace-tool-gpt-4o-streaming.yaml",
            "function_call_output",
        ),
        (
            "codex-openai-websocket-function-tool-gpt-4o-streaming.yaml",
            "function_call_output",
        ),
        (
            "codex-openai-websocket-custom-tool-gpt-5.6-streaming.yaml",
            "custom_tool_call_output",
        ),
        (
            "codex-openai-websocket-namespace-tool-gpt-4o-streaming.yaml",
            "function_call_output",
        ),
    ];

    for (filename, expected_output_type) in cassettes {
        let cassette = load_codex_cassette(filename);
        assert_eq!(cassette.turns.len(), 2, "{filename} should have two turns");
        let turn2_body = turn_request_body(&cassette.turns[1]);
        assert!(
            turn2_body
                .get("previous_response_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| !id.is_empty()),
            "{filename} turn 2 must include previous_response_id"
        );

        let input = turn2_body
            .get("input")
            .and_then(serde_json::Value::as_array)
            .unwrap_or_else(|| panic!("{filename} turn 2 input must be an array"));
        assert!(
            input
                .iter()
                .any(|item| item.get("type").and_then(serde_json::Value::as_str) == Some(expected_output_type)),
            "{filename} turn 2 must include a {expected_output_type}"
        );
    }
}
