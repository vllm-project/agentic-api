#[allow(dead_code)]
mod common;

use common::{spawn_gateway, spawn_mock_llm, test_config, test_state};

#[tokio::test]
async fn test_cors_preflight_returns_200() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let (gw_url, _h2) = spawn_gateway(test_state(&test_config(&llm_url))).await;

    let resp = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{gw_url}/v1/responses"))
        .header("Origin", "http://example.com")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "Content-Type,Authorization")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert!(resp.headers().contains_key("access-control-allow-origin"));
    assert!(resp.headers().contains_key("access-control-allow-methods"));
    assert!(resp.headers().contains_key("access-control-allow-headers"));
}

#[tokio::test]
async fn test_cors_headers_on_regular_request() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let (gw_url, _h2) = spawn_gateway(test_state(&test_config(&llm_url))).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw_url}/v1/responses"))
        .header("Origin", "http://example.com")
        .header("Content-Type", "application/json")
        .body(r#"{"model":"test","input":"hi"}"#)
        .send()
        .await
        .unwrap();

    assert!(resp.headers().contains_key("access-control-allow-origin"));
}
