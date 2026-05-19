mod common;

use common::{
    proxy_state_with_short_timeout, spawn_gateway, spawn_llm, spawn_mid_stream_failure_llm, spawn_timeout_llm,
    test_config, test_config_no_key,
};

#[tokio::test]
async fn test_non_stream_passthrough() {
    let (llm_url, _h1) = spawn_llm().await;
    let config = test_config(&llm_url);
    let (gw_url, _, _h2) = spawn_gateway(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gw_url}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-llm").unwrap().to_str().unwrap(), "responses");
    assert!(!resp.headers().contains_key("connection"));

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "resp_test");
}

#[tokio::test]
async fn test_stream_passthrough() {
    let (llm_url, _h1) = spawn_llm().await;
    let config = test_config(&llm_url);
    let (gw_url, _, _h2) = spawn_gateway(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gw_url}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": [{"role": "user", "content": "hello"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("x-llm").unwrap().to_str().unwrap(),
        "responses-stream"
    );
    assert_eq!(resp.headers().get("x-accel-buffering").unwrap().to_str().unwrap(), "no");
    assert!(!resp.headers().contains_key("content-length"));

    let text = resp.text().await.unwrap();
    assert!(text.contains("data: [DONE]"));
    assert!(text.contains("response.output_text.delta"));
}

#[tokio::test]
async fn test_hop_by_hop_headers_stripped() {
    let (llm_url, _h1) = spawn_llm().await;
    let config = test_config(&llm_url);
    let (gw_url, _, _h2) = spawn_gateway(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gw_url}/v1/responses"))
        .json(&serde_json::json!({"model": "model-a", "input": []}))
        .header("proxy-authorization", "Basic abc123")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-llm").unwrap().to_str().unwrap(), "responses");
    assert!(!resp.headers().contains_key("connection"));
}

#[tokio::test]
async fn test_auth_injection() {
    let (llm_url, _h1) = spawn_llm().await;
    let config = test_config(&llm_url);
    let (gw_url, _, _h2) = spawn_gateway(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gw_url}/v1/responses"))
        .json(&serde_json::json!({"model": "model-a", "input": [], "echo_auth": true}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["authorization"], "Bearer env-llm-key");
}

#[tokio::test]
async fn test_client_auth_precedence() {
    let (llm_url, _h1) = spawn_llm().await;
    let config = test_config(&llm_url);
    let (gw_url, _, _h2) = spawn_gateway(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gw_url}/v1/responses"))
        .json(&serde_json::json!({"model": "model-a", "input": [], "echo_auth": true}))
        .header("authorization", "Bearer client-token")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["authorization"], "Bearer client-token");
}

#[tokio::test]
async fn test_llm_http_error_passthrough() {
    let (llm_url, _h1) = spawn_llm().await;
    let config = test_config(&llm_url);
    let (gw_url, _, _h2) = spawn_gateway(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gw_url}/v1/responses"))
        .json(&serde_json::json!({"model": "model-a", "input": [], "force_error": 429}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers().get("x-llm").unwrap().to_str().unwrap(), "error");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["message"], "rate limited");
    assert_eq!(body["error"]["code"], "rate_limit");
}

#[tokio::test]
async fn test_mid_stream_failure_closes_cleanly() {
    let (llm_url, _h1) = spawn_mid_stream_failure_llm().await;
    let config = test_config(&llm_url);
    let (gw_url, _, _h2) = spawn_gateway(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gw_url}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": [],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap_or_default();
    assert!(text.contains("response.output_text.delta"));
    assert!(!text.contains("data: [DONE]"));
    assert!(!text.contains("llm_timeout"));
    assert!(!text.contains("llm_unavailable"));
}

#[tokio::test]
async fn test_connect_error_maps_to_502() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = listener.local_addr().unwrap();
    drop(listener);

    let config = test_config_no_key(&format!("http://{dead_addr}"));
    let state = proxy_state_with_short_timeout(config);

    let router = agentic_server::app::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({"model": "model-a", "input": []}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 502);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "llm_unavailable");
}

#[tokio::test]
async fn test_timeout_maps_to_504() {
    let (llm_url, _h1) = spawn_timeout_llm().await;
    let config = test_config_no_key(&llm_url);
    let state = proxy_state_with_short_timeout(config);

    let router = agentic_server::app::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({"model": "model-a", "input": []}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 504);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "llm_timeout");
}
