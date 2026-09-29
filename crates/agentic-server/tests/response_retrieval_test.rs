//! Retrieval uses local durable snapshots and never returns continuation history as output.
#[allow(dead_code)]
mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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

/// Mock inference upstream that records every request body it receives.
fn response_retrieval_upstream() -> (Router, Arc<Mutex<Vec<Value>>>) {
    let next_message_id = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new().route(
        "/v1/responses",
        post({
            let next_message_id = Arc::clone(&next_message_id);
            let requests = Arc::clone(&requests);
            move |Json(body): Json<Value>| {
                let message_id = next_message_id.fetch_add(1, Ordering::Relaxed);
                requests.lock().unwrap().push(body);
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
    (router, requests)
}

fn input_texts(request: &Value) -> Vec<&str> {
    request["input"]
        .as_array()
        .expect("upstream input is an item array")
        .iter()
        .map(|item| {
            item["content"]
                .as_array()
                .and_then(|content| content.first())
                .and_then(|part| part["text"].as_str())
                .or_else(|| item["content"].as_str())
                .expect("text item")
        })
        .collect()
}

#[tokio::test]
async fn retrieval_preserves_each_turn_and_rejects_unstored_or_legacy_ids() {
    let (upstream, _requests) = response_retrieval_upstream();
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
async fn store_false_continuation_of_a_stored_response_is_not_retained() {
    let (upstream, requests) = response_retrieval_upstream();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = common::test_config(&format!("http://{}", listener.local_addr().unwrap()));
    let upstream = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    config.db_url = Some("sqlite://?mode=memory".into());
    let exec = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&exec);
    let (url, gateway) = common::spawn_gateway(state).await;
    let client = reqwest::Client::new();
    let create = |body: Value| {
        let client = &client;
        let url = &url;
        async move {
            client
                .post(format!("{url}/v1/responses"))
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };
    let retrieve = |id: &str| {
        let request = client.get(format!("{url}/v1/responses/{id}")).bearer_auth("test-key");
        async move { request.send().await.unwrap() }
    };

    let parent = create(json!({"model":"test-model", "input":"first", "store":true})).await;
    assert_eq!(parent.status(), StatusCode::OK);
    let parent: Value = parent.json().await.unwrap();
    let parent_id = parent["id"].as_str().unwrap();
    let child = create(json!({"model":"test-model", "input":"second", "store":false,
        "previous_response_id":parent_id}))
    .await;
    assert_eq!(child.status(), StatusCode::OK);
    let child: Value = child.json().await.unwrap();
    let child_id = child["id"].as_str().unwrap();
    assert_ne!(child_id, parent_id);
    // The child was hydrated from the stored parent...
    assert_eq!(input_texts(&requests.lock().unwrap()[1]), ["first", "answer", "second"]);
    // ...but it is not retained: neither retrieval nor continuation finds it.
    assert_eq!(retrieve(child_id).await.status(), StatusCode::NOT_FOUND);
    let orphaned = create(json!({"model":"test-model", "input":"third", "store":true,
        "previous_response_id":child_id}))
    .await;
    assert_eq!(orphaned.status(), StatusCode::NOT_FOUND);
    assert!(orphaned.json::<Value>().await.unwrap().get("error").is_some());
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "no inference may run from an unstored response"
    );
    // The stored parent is unaffected: still retrievable and continuable.
    let retrieved = retrieve(parent_id).await;
    assert_eq!(retrieved.status(), StatusCode::OK);
    assert_eq!(retrieved.json::<Value>().await.unwrap(), parent);
    let sibling = create(json!({"model":"test-model", "input":"third", "store":true,
        "previous_response_id":parent_id}))
    .await;
    assert_eq!(sibling.status(), StatusCode::OK);
    assert_eq!(input_texts(&requests.lock().unwrap()[2]), ["first", "answer", "third"]);
    upstream.abort();
    let _ = upstream.await;
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
