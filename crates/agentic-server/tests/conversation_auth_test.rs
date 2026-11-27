//! Conversation management routes act on local storage, so they require the
//! configured API key when OIDC is disabled, just like response retrieval.
#[allow(dead_code)]
mod common;

use std::sync::Arc;

use agentic_core::executor::ExecutionContext;
use http::StatusCode;
use serde_json::{Value, json};

fn conversation_requests(url: &str) -> Vec<(reqwest::Method, String, Option<Value>)> {
    let id = "conv_missing";
    let item_id = "item_missing";
    vec![
        (
            reqwest::Method::POST,
            format!("{url}/v1/conversations"),
            Some(json!({})),
        ),
        (reqwest::Method::GET, format!("{url}/v1/conversations/{id}"), None),
        (
            reqwest::Method::POST,
            format!("{url}/v1/conversations/{id}"),
            Some(json!({"metadata": {}})),
        ),
        (reqwest::Method::DELETE, format!("{url}/v1/conversations/{id}"), None),
        (reqwest::Method::GET, format!("{url}/v1/conversations/{id}/items"), None),
        (
            reqwest::Method::POST,
            format!("{url}/v1/conversations/{id}/items"),
            Some(json!({"items": []})),
        ),
        (
            reqwest::Method::GET,
            format!("{url}/v1/conversations/{id}/items/{item_id}"),
            None,
        ),
        (
            reqwest::Method::DELETE,
            format!("{url}/v1/conversations/{id}/items/{item_id}"),
            None,
        ),
    ]
}

async fn assert_conversations_require_valid_key(client: &reqwest::Client, url: &str) {
    for authorization in [None, Some("Bearer wrong-key"), Some("Basic test-key"), Some("Bearer ")] {
        for (method, target, body) in conversation_requests(url) {
            let mut request = client.request(method.clone(), &target);
            if let Some(authorization) = authorization {
                request = request.header("authorization", authorization);
            }
            if let Some(body) = body {
                request = request.json(&body);
            }
            let denied = request.send().await.unwrap();
            assert_eq!(
                denied.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {target} with {authorization:?} must require a credential"
            );
            assert_eq!(denied.headers()["www-authenticate"], "Bearer");
            assert_eq!(
                denied.json::<Value>().await.unwrap()["error"]["type"],
                "authentication_error"
            );
        }
    }
}

async fn spawn_conversation_gateway(
    key: Option<String>,
) -> (String, tokio::task::JoinHandle<()>, Arc<ExecutionContext>) {
    let mut config = common::test_config("http://127.0.0.1:1");
    config.openai_api_key = key;
    config.db_url = Some("sqlite://?mode=memory".into());
    let exec = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&exec);
    let (url, gateway) = common::spawn_gateway(state).await;
    (url, gateway, exec)
}

#[tokio::test]
async fn conversations_require_valid_key_with_oidc_disabled() {
    let (url, gateway, exec) = spawn_conversation_gateway(Some("test-key".to_owned())).await;
    let client = reqwest::Client::new();
    assert_conversations_require_valid_key(&client, &url).await;

    // A valid key reaches the handler; an unknown id is a 404 rather than a 401.
    let response = client
        .get(format!("{url}/v1/conversations/conv_missing"))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // A valid key can create, read, and delete a conversation.
    let created: Value = client
        .post(format!("{url}/v1/conversations"))
        .bearer_auth("test-key")
        .json(&json!({"metadata": {"owner": "alice"}}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();
    let listed = client
        .get(format!("{url}/v1/conversations/{id}/items"))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let deleted = client
        .delete(format!("{url}/v1/conversations/{id}"))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);

    gateway.abort();
    let _ = gateway.await;
    exec.storage_pool().unwrap().close().await;
}

#[tokio::test]
async fn conversations_are_open_without_configured_authentication() {
    for key in [None, Some(String::new())] {
        let (url, gateway, exec) = spawn_conversation_gateway(key).await;
        let response = reqwest::get(format!("{url}/v1/conversations/conv_missing"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        gateway.abort();
        let _ = gateway.await;
        exec.storage_pool().unwrap().close().await;
    }
}
