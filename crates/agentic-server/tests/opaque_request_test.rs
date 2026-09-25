mod common;

use http::StatusCode;
use serde_json::Value;

use agentic_core::types::reasoning_profile::OpaqueReasoningProfile;
use agentic_core::types::reasoning_replay::ReasoningReplayPolicy;

use common::{spawn_gateway, spawn_mock_llm, test_config, test_state};

#[tokio::test]
async fn selected_profile_rejects_unknown_wire_fields_before_routing() {
    let (llm_url, _llm) = spawn_mock_llm().await;
    let mut config = test_config(&llm_url);
    config.responses.reasoning_replay_policy = ReasoningReplayPolicy::OpaqueResponses;
    config.responses.reasoning_replay_profile = Some(OpaqueReasoningProfile::OpenAiGpt54_20260305V1);
    let (gateway_url, _gateway) = spawn_gateway(test_state(&config)).await;
    let client = reqwest::Client::new();

    for body in [
        r#"{"model":"gpt-5.4-2026-03-05","input":"hi","store":false,"unknown":true}"#,
        r#"{"model":"gpt-5.4-2026-03-05","input":"hi","store":false,"unknown":true,"unknown":false}"#,
        r#"{"model":"gpt-5.4-2026-03-05","input":"hi","store":false,"reasoning":{"effort":"low","unknown":true}}"#,
        r#"{"model":"gpt-5.4-2026-03-05","input":[{"type":"message","role":"user","content":"hi","unknown":true}],"store":false}"#,
        r#"{"model":"gpt-5.4-2026-03-05","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi","unknown":true}]}],"store":false}"#,
        r#"{"model":"gpt-5.4-2026-03-05","input":[{"type":"function_call_output","call_id":"c","output":"ok","unknown":true}],"store":false}"#,
        r#"{"model":"gpt-5.4-2026-03-05","input":"hi","tools":[{"type":"function","name":"lookup","unknown":true}],"store":false}"#,
        r#"{"model":"gpt-5.4-2026-03-05","input":"hi","tools":[{"type":"function","name":"lookup","parameters":{"type":"object","type":"array"}}],"store":false}"#,
        r#"{"model":"gpt-5.4-2026-03-05","input":"hi","tools":[{"type":"mcp","server_label":"local","require_approval":"never","unknown":true}],"store":false}"#,
        r#"{"model":"gpt-5.4-2026-03-05","input":"hi","tool_choice":{"type":"function","name":"lookup","unknown":true},"store":false}"#,
    ] {
        let response = client
            .post(format!("{gateway_url}/v1/responses"))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .expect("gateway response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: Value = response.json().await.expect("error JSON");
        assert_eq!(error["error"]["code"], "reasoning_replay_incompatible");
        assert!(!error.to_string().contains("unknown"));
    }
}

#[tokio::test]
async fn selected_profile_never_proxies_store_false_requests() {
    let (llm_url, _llm) = spawn_mock_llm().await;
    let mut config = test_config(&llm_url);
    config.responses.reasoning_replay_policy = ReasoningReplayPolicy::OpaqueResponses;
    config.responses.reasoning_replay_profile = Some(OpaqueReasoningProfile::OpenAiGpt54_20260305V1);
    let (gateway_url, _gateway) = spawn_gateway(test_state(&config)).await;
    let request = serde_json::json!({"model":"gpt-5.4-2026-03-05","input":"hi","store":false});
    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/responses"))
        .json(&request)
        .send()
        .await
        .expect("gateway response");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let error: Value = response.json().await.expect("error JSON");
    assert_eq!(error["error"]["code"], "reasoning_replay_incompatible");
    assert!(
        error["error"]["message"]
            .as_str()
            .expect("message")
            .contains("endpoint")
    );
}

#[tokio::test]
async fn default_profile_still_transparently_proxies_store_false_requests() {
    let (llm_url, _llm) = spawn_mock_llm().await;
    let (gateway_url, _gateway) = spawn_gateway(test_state(&test_config(&llm_url))).await;
    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/responses"))
        .header("content-type", "application/json")
        .body(r#"{"model":"test-model","input":"hi","store":false,"unknown":true}"#)
        .send()
        .await
        .expect("gateway response");
    // The mock has no Responses route; a 404 proves transparent proxying remained active.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
