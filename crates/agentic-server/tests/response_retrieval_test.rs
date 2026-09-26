//! Retrieval uses local durable snapshots and never returns continuation history as output.
#[allow(dead_code)]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agentic_core::executor::ExecutionContext;
use agentic_core::storage::{ResponseMetadata, ResponseStore};
use axum::{Json, Router, routing::post};
use http::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;

async fn assert_retrieval_requires_valid_key(client: &reqwest::Client, url: &str, id: &str) {
    for authorization in [None, Some("Bearer wrong-key"), Some("Basic test-key"), Some("Bearer ")] {
        for requested_id in [id, "resp_missing"] {
            let mut request = client.get(format!("{url}/v1/responses/{requested_id}"));
            if let Some(authorization) = authorization {
                request = request.header("authorization", authorization);
            }
            let denied = request.send().await.unwrap();
            assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(denied.headers()["www-authenticate"], "Bearer");
            assert_eq!(
                denied.json::<Value>().await.unwrap()["error"]["type"],
                "authentication_error"
            );
        }
    }
}

#[tokio::test]
async fn retrieval_preserves_each_turn_and_rejects_unstored_or_legacy_ids() {
    let next_message_id = Arc::new(AtomicUsize::new(0));
    let upstream = Router::new().route(
        "/v1/responses",
        post({
            let next_message_id = Arc::clone(&next_message_id);
            move || {
                let message_id = next_message_id.fetch_add(1, Ordering::Relaxed);
                async move {
                    Json(json!({"id":"resp_upstream", "object":"response", "created_at":123,
                    "model":"test-model", "status":"completed", "output":[{
                        "type":"message", "id":format!("msg_answer_{message_id}"), "role":"assistant", "status":"completed",
                        "content":[{"type":"output_text", "text":"answer"}]}],
                    "usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}}))
                }
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = common::test_config(&format!("http://{}", listener.local_addr().unwrap()));
    let upstream = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    config.db_url = Some("sqlite://?mode=memory".into());
    let exec = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&exec);
    let (url, gateway) = common::spawn_gateway(state).await;
    let client = reqwest::Client::new();
    let mut previous = Value::Null;
    let mut responses = Vec::new();
    for input in ["first", "second"] {
        let response = client
            .post(format!("{url}/v1/responses"))
            .json(&json!({"model":"test-model", "input":input, "store":true,
                "previous_response_id":previous}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response: Value = response.json().await.unwrap();
        previous = response["id"].clone();
        responses.push(response);
    }
    let unstored = client
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model":"test-model", "input":"transient", "store":false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let missing = client
        .get(format!("{url}/v1/responses/{}", unstored["id"].as_str().unwrap()))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    // GET must still work without an inference server.
    upstream.abort();
    let _ = upstream.await;
    for response in responses {
        let id = response["id"].as_str().unwrap();
        assert_retrieval_requires_valid_key(&client, &url, id).await;
        let retrieved = client
            .get(format!("{url}/v1/responses/{id}"))
            .bearer_auth("test-key")
            .send()
            .await
            .unwrap();
        assert_eq!(retrieved.status(), StatusCode::OK);
        assert_eq!(retrieved.json::<Value>().await.unwrap(), response);
    }
    let store = ResponseStore::new(Arc::new(exec.storage_pool().unwrap().clone()));
    store
        .persist("resp_legacy", None, Vec::new(), &ResponseMetadata::default())
        .await
        .unwrap();
    let legacy = client
        .get(format!("{url}/v1/responses/resp_legacy"))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(legacy.status(), StatusCode::CONFLICT);
    assert!(
        legacy.json::<Value>().await.unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no retrievable payload")
    );
    let missing = client
        .get(format!("{url}/v1/responses/resp_missing"))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert!(missing.json::<Value>().await.unwrap().get("error").is_some());
    gateway.abort();
    let _ = gateway.await;
    exec.storage_pool().unwrap().close().await;
}

#[tokio::test]
async fn retrieval_reports_disabled_storage_as_server_error() {
    let state = common::test_state(&common::test_config("http://127.0.0.1:1"));
    let (url, gateway) = common::spawn_gateway(state).await;
    let response = reqwest::Client::new()
        .get(format!("{url}/v1/responses/resp_missing"))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(response.json::<Value>().await.unwrap().get("error").is_some());
    gateway.abort();
    let _ = gateway.await;
}

#[tokio::test]
async fn retrieval_allows_unauthenticated_access_without_configured_authentication() {
    for key in [None, Some(String::new())] {
        let mut config = common::test_config("http://127.0.0.1:1");
        config.openai_api_key = key;
        config.db_url = Some("sqlite://?mode=memory".into());
        let exec = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
        let mut state = common::test_state(&config);
        state.exec_ctx = Arc::clone(&exec);
        let (url, gateway) = common::spawn_gateway(state).await;
        let response = reqwest::get(format!("{url}/v1/responses/resp_missing")).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        gateway.abort();
        let _ = gateway.await;
        exec.storage_pool().unwrap().close().await;
    }
}
