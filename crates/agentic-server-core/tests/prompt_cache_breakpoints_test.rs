//! Pin content-boundary preservation across canonical and model-facing representations.
use agentic_core::types::RequestPayload;
use agentic_core::types::io::ResponsesInput;
use serde_json::{Value, json};

#[test]
fn breakpoints_survive_image_defaults_and_custom_tool_normalization() {
    let parts = json!([
        {"type":"input_text","text":"prefix","prompt_cache_breakpoint":{"mode":"explicit"}},
        {"type":"input_image","image_url":"data:image/png;base64,AQ==",
            "prompt_cache_breakpoint":{"mode":"explicit"}},
        {"type":"input_text","text":"unmarked suffix"}
    ]);
    for output_kind in ["function_call_output", "custom_tool_call_output"] {
        let output = json!([
            {"type":"input_text","text":"tool prefix","prompt_cache_breakpoint":{"mode":"explicit"}},
            {"type":"input_image","image_url":"data:image/png;base64,AQ==","detail":"auto",
                "prompt_cache_breakpoint":{"mode":"explicit"}},
            {"type":"input_file","file_id":"file_test","prompt_cache_breakpoint":{"mode":"explicit"}}
        ]);
        let request: RequestPayload = serde_json::from_value(json!({"model":"test",
            "input":[{"type":"message","role":"user","content":parts},
                {"type":output_kind,"call_id":"call_test","output":output}],
            "prompt_cache_options":{"mode":"explicit","ttl":"30m"}}))
        .unwrap();
        let canonical = serde_json::to_value(&request.input).unwrap();
        let upstream = serde_json::to_value(request.to_upstream_request(false).unwrap()).unwrap();
        let mut model_parts = parts.clone();
        model_parts[1]["detail"] = json!("auto");
        assert_eq!(upstream["input"][0]["content"], model_parts);
        assert_eq!(upstream["input"][1]["type"], "function_call_output");
        assert_eq!(upstream["input"][1]["output"], output);
        assert_eq!(upstream["prompt_cache_options"], json!({"mode":"explicit","ttl":"30m"}));
        // Constructing the model view does not mutate the content stored as canonical history.
        assert_eq!(serde_json::to_value(&request.input).unwrap(), canonical);
        assert_eq!(canonical[0]["content"], parts);
        let stored: ResponsesInput = serde_json::from_value(canonical.clone()).unwrap();
        assert_eq!(serde_json::to_value(stored.into_canonical_items()).unwrap(), canonical);
    }
}

#[test]
fn explicit_mode_without_breakpoints_does_not_create_boundaries_or_implicit_mode() {
    let request: RequestPayload = serde_json::from_value(json!({"model":"test",
        "input":[{"role":"user","content":[{"type":"input_text","text":"no boundary"}]}],
        "prompt_cache_options":{"mode":"explicit"}}))
    .unwrap();
    let upstream: Value = serde_json::to_value(request.to_upstream_request(false).unwrap()).unwrap();
    assert_eq!(upstream["prompt_cache_options"], json!({"mode":"explicit"}));
    assert_eq!(
        upstream["input"][0]["content"],
        json!([{"type":"input_text","text":"no boundary"}])
    );
}
