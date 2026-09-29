use super::*;
use agentic_core::types::ConversationItem;

async fn create_conversation_with_items(url: &str, items: Vec<serde_json::Value>) -> String {
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/conversations"))
        .json(&json!({"items": items}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    body["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn regression_openai_batch_item_request() {
    let state = test_state_with_storage("http://127.0.0.1:1").await;
    let (url, handle) = spawn_gateway(state).await;
    let conv_id = create_conversation_with_items(&url, vec![]).await;
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/conversations/{conv_id}/items"))
        .json(&json!({"items": [{"type":"message", "role":"user", "content":"hello"}]}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    handle.abort();
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn regression_list_wire_shape() {
    let state = test_state_with_storage("http://127.0.0.1:1").await;
    let (url, handle) = spawn_gateway(state).await;
    let conv_id = create_conversation_with_items(
        &url,
        vec![json!({
            "type": "message", "role": "user", "content": "hello"
        })],
    )
    .await;
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{url}/v1/conversations/{conv_id}/items"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();
    assert_eq!(body["data"][0]["type"], "message", "{body}");
}

#[tokio::test]
async fn regression_list_descending_order() {
    let state = test_state_with_storage("http://127.0.0.1:1").await;
    let (url, handle) = spawn_gateway(state).await;
    let conv_id = create_conversation_with_items(
        &url,
        vec![
            json!({"type": "message", "role": "user", "content": "first"}),
            json!({"type": "message", "role": "user", "content": "last"}),
        ],
    )
    .await;
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{url}/v1/conversations/{conv_id}/items?order=desc&limit=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();
    assert_eq!(body["data"][0]["content"][0]["text"], "last", "{body}");
}

#[test]
fn regression_output_item_roundtrip() {
    let wire = json!({"type":"web_search_call", "id":"ws_1", "status":"completed",
        "action":{"type":"search", "query":"weather", "queries":["weather"], "sources":[{"url":"https://example.com"}]}});
    let parsed: ConversationItem = serde_json::from_value(wire.clone()).unwrap();
    let actual = serde_json::to_value(parsed).unwrap();
    assert_eq!(actual, wire);
}

#[tokio::test]
async fn regression_delete_item_returns_conversation() {
    let state = test_state_with_storage("http://127.0.0.1:1").await;
    let (url, handle) = spawn_gateway(state).await;
    let conv_id = create_conversation_with_items(
        &url,
        vec![json!({
            "type": "message", "role": "user", "content": "hello"
        })],
    )
    .await;
    let client = reqwest::Client::new();
    let listed: serde_json::Value = client
        .get(format!("{url}/v1/conversations/{conv_id}/items"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let item_id = listed["data"][0]["id"].as_str().unwrap();
    let body: serde_json::Value = client
        .delete(format!("{url}/v1/conversations/{conv_id}/items/{item_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();
    assert_eq!(body["object"], "conversation", "{body}");
    assert_eq!(body["id"], conv_id);
}

#[tokio::test]
async fn rejects_oversized_batches_and_invalid_limits() {
    let state = test_state_with_storage("http://127.0.0.1:1").await;
    let (url, handle) = spawn_gateway(state).await;
    let conv_id = create_conversation_with_items(&url, vec![]).await;
    let client = reqwest::Client::new();
    let items = vec![json!({"role":"user", "content":"hello"}); 21];
    for path in [
        "/v1/conversations".to_owned(),
        format!("/v1/conversations/{conv_id}/items"),
    ] {
        let response = client
            .post(format!("{url}{path}"))
            .json(&json!({"items": items}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    for query in ["limit=0", "limit=101", "order=invalid"] {
        let response = client
            .get(format!("{url}/v1/conversations/{conv_id}/items?{query}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    handle.abort();
}
