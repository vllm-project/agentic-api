#[allow(dead_code)]
mod common;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::Request;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use http::StatusCode;
use tokio::net::TcpListener;

use common::{spawn_ogx, spawn_vllm, start_gateway};

#[tokio::test]
async fn test_no_previous_response_id() {
    let (vllm_port, _h) = spawn_vllm().await;
    let (ogx_port, _h2) = spawn_ogx().await;
    let (gw_addr, _) = start_gateway(vllm_port, Some(ogx_port), None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "resp_test");
}

#[tokio::test]
async fn test_hydrates_conversation_history() {
    let (ogx_port, _h2) = spawn_ogx().await;

    let captured = Arc::new(Mutex::new(None::<serde_json::Value>));
    let captured_clone = Arc::clone(&captured);

    let app = Router::new().route("/health", get(|| async { StatusCode::OK })).route(
        "/v1/responses",
        post(move |req: Request| {
            let captured = Arc::clone(&captured_clone);
            async move {
                let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
                    .await
                    .unwrap_or_default();
                let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap_or_default();
                *captured.lock().unwrap() = Some(body);

                let resp = r#"{"id":"resp_hydrated","object":"response","status":"completed","output":[]}"#;
                (StatusCode::OK, [("content-type", "application/json")], resp).into_response()
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let vllm_port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (gw_addr, _) = start_gateway(vllm_port, Some(ogx_port), None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": [{"role": "user", "content": "follow-up question"}],
            "previous_response_id": "resp_prev"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "resp_hydrated");

    let captured_body = captured
        .lock()
        .unwrap()
        .clone()
        .expect("vLLM should have received a request");
    let input = captured_body["input"].as_array().expect("input should be an array");

    // Should have: OGx input items + OGx response output + original user message = 3 items
    assert!(
        input.len() >= 3,
        "expected at least 3 input items (hydrated history + user msg), got {}",
        input.len()
    );

    // First item should be the previous user message from OGx
    assert_eq!(input[0]["role"], "user");
    // Second should be the assistant output from OGx response
    assert_eq!(input[1]["role"], "assistant");
    // Last should be the new user message
    let last = input.last().unwrap();
    assert_eq!(last["role"], "user");

    // previous_response_id should have been stripped
    assert!(
        captured_body.get("previous_response_id").is_none(),
        "previous_response_id should be stripped from the request sent to vLLM"
    );
}
