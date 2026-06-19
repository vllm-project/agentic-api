#[allow(dead_code)]
mod common;

use common::{spawn_mid_stream_failure_vllm, spawn_vllm, start_gateway};

#[tokio::test]
async fn test_non_stream_passthrough() {
    let (vllm_port, _h) = spawn_vllm().await;
    let (gw_addr, _) = start_gateway(vllm_port, None, Some("env-vllm-key")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": [{"role": "user", "content": "hello"}],
            "store": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "resp_test");
}

#[tokio::test]
async fn test_string_input_passthrough() {
    let (vllm_port, _h) = spawn_vllm().await;
    let (gw_addr, _) = start_gateway(vllm_port, None, Some("env-vllm-key")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": "hello",
            "store": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "resp_test");
}

#[tokio::test]
async fn test_stream_passthrough() {
    let (vllm_port, _h) = spawn_vllm().await;
    let (gw_addr, _) = start_gateway(vllm_port, None, Some("env-vllm-key")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": [{"role": "user", "content": "hello"}],
            "store": false,
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let text = resp.text().await.unwrap();
    assert!(text.contains("data: [DONE]"));
    assert!(text.contains("response.output_text.delta"));
}

#[tokio::test]
async fn test_auth_injection() {
    let (vllm_port, _h) = spawn_vllm().await;
    let (gw_addr, _) = start_gateway(vllm_port, None, Some("env-vllm-key")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({"model": "model-a", "input": [], "store": false, "echo_auth": true}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["authorization"], "Bearer env-vllm-key");
}

#[tokio::test]
async fn test_client_auth_precedence() {
    let (vllm_port, _h) = spawn_vllm().await;
    let (gw_addr, _) = start_gateway(vllm_port, None, Some("env-vllm-key")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({"model": "model-a", "input": [], "store": false, "echo_auth": true}))
        .header("authorization", "Bearer client-token")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["authorization"], "Bearer client-token");
}

#[tokio::test]
async fn test_vllm_http_error_passthrough() {
    let (vllm_port, _h) = spawn_vllm().await;
    let (gw_addr, _) = start_gateway(vllm_port, None, Some("env-vllm-key")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({"model": "model-a", "input": [], "store": false, "force_error": 429}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 429);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["message"], "rate limited");
    assert_eq!(body["error"]["code"], "rate_limit");
}

#[tokio::test]
async fn test_mid_stream_failure_closes_cleanly() {
    let (vllm_port, _h) = spawn_mid_stream_failure_vllm().await;
    let (gw_addr, _) = start_gateway(vllm_port, None, Some("env-vllm-key")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": [],
            "store": false,
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap_or_default();
    assert!(text.contains("response.output_text.delta"));
}

#[tokio::test]
async fn test_connect_error_maps_to_502() {
    let (gw_addr, _) = start_gateway(1, None, None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({"model": "model-a", "input": [], "store": false}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 502);
}
