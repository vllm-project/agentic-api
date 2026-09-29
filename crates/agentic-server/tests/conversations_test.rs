mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use http::StatusCode;
use serde_json::json;

use agentic_core::executor::{ConversationHandler, ExecutionContext, ResponseHandler};
use agentic_core::storage::{ConversationStore, ResponseStore, create_pool_with_schema};
use agentic_server::app::{AppState, DEFAULT_MAX_REQUEST_BODY_SIZE, ReadinessTracker, WebSocketTracker};
use common::{spawn_gateway, spawn_mock_llm, test_config};

// Counter for unique in-memory database names to avoid migration conflicts
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Create test state with in-memory SQLite storage enabled.
async fn test_state_with_storage(llm_url: &str) -> AppState {
    let config = test_config(llm_url);
    // Use unique in-memory database for each test to avoid migration conflicts and file descriptor exhaustion
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Use in-memory SQLite with unique name to allow parallel test execution without exhausting file descriptors
    let db_url = format!("sqlite:file:test_conv_{id}?mode=memory&cache=shared");
    let pool = create_pool_with_schema(Some(&db_url))
        .await
        .expect("failed to create test pool");

    let conv_handler = ConversationHandler::new(ConversationStore::new(pool.clone()));
    let resp_handler = ResponseHandler::new(ResponseStore::new(pool));
    let exec_ctx = ExecutionContext::new(
        conv_handler,
        resp_handler,
        Arc::new(reqwest::Client::new()),
        config.llm_api_base.clone(),
    );

    let proxy_state = agentic_core::proxy::ProxyState::new(config.clone()).expect("proxy state");

    AppState {
        proxy_state,
        exec_ctx: Arc::new(exec_ctx),
        llm_readiness_client: agentic_core::readiness::llm_readiness_client().expect("readiness client"),
        readiness_tracker: ReadinessTracker::default(),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        websocket_tracker: WebSocketTracker::default(),
        llm_api_base: config.llm_api_base.clone(),
        skip_llm_ready_check: config.skip_llm_ready_check,
        openai_api_key: config.openai_api_key.clone(),
        model_capabilities: Arc::default(),
        max_request_body_size: DEFAULT_MAX_REQUEST_BODY_SIZE,
    }
}

#[tokio::test]
async fn test_create_conversation_preserves_store_true_contract() {
    let (llm_url, _llm) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gateway_url, _gateway) = spawn_gateway(state).await;
    let client = reqwest::Client::new();

    let created = client
        .post(format!("{gateway_url}/v1/conversations"))
        .json(&json!({"store": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    let body: serde_json::Value = created.json().await.unwrap();
    let id = body["id"].as_str().unwrap();
    assert_eq!(body["object"], "conversation");
    assert_eq!(body["metadata"], json!({}));

    let items = client
        .get(format!("{gateway_url}/v1/conversations/{id}/items"))
        .send()
        .await
        .unwrap();
    assert_eq!(items.status(), StatusCode::OK);

    let rejected = client
        .post(format!("{gateway_url}/v1/conversations"))
        .json(&json!({"store": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_conversation_with_metadata() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({
            "metadata": {"user_id": "test123", "session": "abc"}
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "conversation");
    assert!(body["id"].as_str().unwrap().starts_with("conv_"));
    assert!(body["created_at"].as_i64().is_some());
    assert_eq!(body["metadata"]["user_id"], "test123");
    assert_eq!(body["metadata"]["session"], "abc");
}

#[tokio::test]
async fn test_create_conversation_with_initial_items() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({
            "items": [
                {
                    "type": "message",
                    "role": "user",
                    "content": "Hello, world!"
                }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["id"].as_str().unwrap().starts_with("conv_"));
}

#[tokio::test]
async fn test_retrieve_conversation() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let client = reqwest::Client::new();

    // Create conversation
    let create_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({"metadata": {"test": "data"}}))
        .send()
        .await
        .unwrap();
    let create_body: serde_json::Value = create_resp.json().await.unwrap();
    let conv_id = create_body["id"].as_str().unwrap();

    // Retrieve it
    let retrieve_resp = client
        .get(format!("{gw_url}/v1/conversations/{conv_id}"))
        .send()
        .await
        .unwrap();

    assert_eq!(retrieve_resp.status(), StatusCode::OK);
    let retrieve_body: serde_json::Value = retrieve_resp.json().await.unwrap();
    assert_eq!(retrieve_body["id"], conv_id);
    assert_eq!(retrieve_body["metadata"]["test"], "data");
}

#[tokio::test]
async fn test_retrieve_nonexistent_conversation_returns_404() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let resp = reqwest::Client::new()
        .get(format!("{gw_url}/v1/conversations/conv_nonexistent"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "not_found");
}

#[tokio::test]
async fn test_update_conversation_metadata() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let client = reqwest::Client::new();

    // Create conversation
    let create_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({"metadata": {"original": "value"}}))
        .send()
        .await
        .unwrap();
    let create_body: serde_json::Value = create_resp.json().await.unwrap();
    let conv_id = create_body["id"].as_str().unwrap();

    // Update metadata
    let update_resp = client
        .post(format!("{gw_url}/v1/conversations/{conv_id}"))
        .json(&json!({"metadata": {"updated": "new_value"}}))
        .send()
        .await
        .unwrap();

    assert_eq!(update_resp.status(), StatusCode::OK);
    let update_body: serde_json::Value = update_resp.json().await.unwrap();
    assert_eq!(update_body["id"], conv_id);
    assert_eq!(update_body["metadata"]["updated"], "new_value");
    assert!(update_body["metadata"]["original"].is_null());
}

#[tokio::test]
async fn test_update_nonexistent_conversation_returns_404() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw_url}/v1/conversations/conv_nonexistent"))
        .json(&json!({"metadata": {"test": "value"}}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_conversation() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let client = reqwest::Client::new();

    // Create conversation
    let create_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let create_body: serde_json::Value = create_resp.json().await.unwrap();
    let conv_id = create_body["id"].as_str().unwrap();

    // Delete it
    let delete_resp = client
        .delete(format!("{gw_url}/v1/conversations/{conv_id}"))
        .send()
        .await
        .unwrap();

    assert_eq!(delete_resp.status(), StatusCode::OK);
    let delete_body: serde_json::Value = delete_resp.json().await.unwrap();
    assert_eq!(delete_body["id"], conv_id);
    assert_eq!(delete_body["object"], "conversation.deleted");
    assert_eq!(delete_body["deleted"], true);

    // Verify it's gone
    let retrieve_resp = client
        .get(format!("{gw_url}/v1/conversations/{conv_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(retrieve_resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_nonexistent_conversation_returns_404() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let resp = reqwest::Client::new()
        .delete(format!("{gw_url}/v1/conversations/conv_nonexistent"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_create_item_in_conversation() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let client = reqwest::Client::new();

    // Create conversation
    let create_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let create_body: serde_json::Value = create_resp.json().await.unwrap();
    let conv_id = create_body["id"].as_str().unwrap();

    // Add item
    let item_resp = client
        .post(format!("{gw_url}/v1/conversations/{conv_id}/items"))
        .json(&json!({
            "items": [{
                "type": "message",
                "role": "user",
                "content": "Hello!"
            }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(item_resp.status(), StatusCode::OK);
    let item_body: serde_json::Value = item_resp.json().await.unwrap();
    assert_eq!(item_body["object"], "list");
    assert_eq!(item_body["data"].as_array().unwrap().len(), 1);
    assert!(item_body["data"][0]["id"].as_str().unwrap().starts_with("msg_"));
    assert_eq!(item_body["data"][0]["type"], "message");
    assert_eq!(item_body["data"][0]["role"], "user");
    assert_eq!(item_body["data"][0]["content"][0]["text"], "Hello!");
}

#[tokio::test]
async fn test_create_item_in_nonexistent_conversation_returns_404() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw_url}/v1/conversations/conv_nonexistent/items"))
        .json(&json!({
            "items": [{
                "type": "message",
                "role": "user",
                "content": "test"
            }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_list_items_in_conversation() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let client = reqwest::Client::new();

    // Create conversation
    let create_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({
            "items": [
                {"type": "message", "role": "user", "content": "First"},
                {"type": "message", "role": "assistant", "content": "Second"},
                {"type": "message", "role": "user", "content": "Third"}
            ]
        }))
        .send()
        .await
        .unwrap();
    let create_body: serde_json::Value = create_resp.json().await.unwrap();
    let conv_id = create_body["id"].as_str().unwrap();

    // List items
    let list_resp = client
        .get(format!("{gw_url}/v1/conversations/{conv_id}/items"))
        .send()
        .await
        .unwrap();

    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    assert_eq!(list_body["object"], "list");
    assert_eq!(list_body["data"].as_array().unwrap().len(), 3);
    assert_eq!(list_body["has_more"], false);
    assert_eq!(list_body["data"][0]["content"][0]["text"], "Third");
    assert_eq!(list_body["data"][1]["content"][0]["text"], "Second");
    assert_eq!(list_body["data"][2]["content"][0]["text"], "First");

    let included = client
        .get(format!("{gw_url}/v1/conversations/{conv_id}/items"))
        .query(&[("order", "desc"), ("include[]", "message.output_text.logprobs")])
        .send()
        .await
        .unwrap();
    assert_eq!(included.status(), StatusCode::OK);
    let included_body: serde_json::Value = included.json().await.unwrap();
    assert_eq!(included_body["data"], list_body["data"]);

    let unsupported = client
        .get(format!("{gw_url}/v1/conversations/{conv_id}/items"))
        .query(&[("include[]", "file_search_call.results")])
        .send()
        .await
        .unwrap();
    assert_eq!(unsupported.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_list_items_pagination() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let client = reqwest::Client::new();

    // Create conversation with many items
    let mut items = Vec::new();
    for i in 0..20 {
        items.push(json!({
            "type": "message",
            "role": if i % 2 == 0 { "user" } else { "assistant" },
            "content": format!("Message {}", i)
        }));
    }

    let create_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({"items": items}))
        .send()
        .await
        .unwrap();
    let create_body: serde_json::Value = create_resp.json().await.unwrap();
    let conv_id = create_body["id"].as_str().unwrap();

    // List with limit
    let list_resp = client
        .get(format!("{gw_url}/v1/conversations/{conv_id}/items?limit=10"))
        .send()
        .await
        .unwrap();

    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    assert_eq!(list_body["data"].as_array().unwrap().len(), 10);
    assert_eq!(list_body["has_more"], true);

    // Get second page
    let last_id = list_body["last_id"].as_str().unwrap();
    let page2_resp = client
        .get(format!(
            "{gw_url}/v1/conversations/{conv_id}/items?limit=10&after={last_id}"
        ))
        .send()
        .await
        .unwrap();

    let page2_body: serde_json::Value = page2_resp.json().await.unwrap();
    assert_eq!(page2_body["data"].as_array().unwrap().len(), 10);
    assert_eq!(page2_body["has_more"], false);
}

#[tokio::test]
async fn test_list_items_in_nonexistent_conversation_returns_404() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let resp = reqwest::Client::new()
        .get(format!("{gw_url}/v1/conversations/conv_nonexistent/items"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_retrieve_item() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let client = reqwest::Client::new();

    // Create conversation with item
    let create_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({
            "items": [{"type": "message", "role": "user", "content": "Test message"}]
        }))
        .send()
        .await
        .unwrap();
    let create_body: serde_json::Value = create_resp.json().await.unwrap();
    let conv_id = create_body["id"].as_str().unwrap();

    // List to get item ID
    let list_resp = client
        .get(format!("{gw_url}/v1/conversations/{conv_id}/items"))
        .send()
        .await
        .unwrap();
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    let item_id = list_body["data"][0]["id"].as_str().unwrap();

    // Retrieve specific item
    let retrieve_resp = client
        .get(format!("{gw_url}/v1/conversations/{conv_id}/items/{item_id}"))
        .send()
        .await
        .unwrap();

    assert_eq!(retrieve_resp.status(), StatusCode::OK);
    let retrieve_body: serde_json::Value = retrieve_resp.json().await.unwrap();
    assert_eq!(retrieve_body["id"], item_id);
    assert_eq!(retrieve_body["content"][0]["text"], "Test message");
}

#[tokio::test]
async fn test_retrieve_nonexistent_item_returns_404() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let client = reqwest::Client::new();

    // Create conversation
    let create_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let create_body: serde_json::Value = create_resp.json().await.unwrap();
    let conv_id = create_body["id"].as_str().unwrap();

    // Try to retrieve nonexistent item
    let resp = client
        .get(format!("{gw_url}/v1/conversations/{conv_id}/items/item_nonexistent"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_item() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let client = reqwest::Client::new();

    // Create conversation with item
    let create_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({
            "items": [{"type": "message", "role": "user", "content": "To be deleted"}]
        }))
        .send()
        .await
        .unwrap();
    let create_body: serde_json::Value = create_resp.json().await.unwrap();
    let conv_id = create_body["id"].as_str().unwrap();

    // List to get item ID
    let list_resp = client
        .get(format!("{gw_url}/v1/conversations/{conv_id}/items"))
        .send()
        .await
        .unwrap();
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    let item_id = list_body["data"][0]["id"].as_str().unwrap();

    // Delete item
    let delete_resp = client
        .delete(format!("{gw_url}/v1/conversations/{conv_id}/items/{item_id}"))
        .send()
        .await
        .unwrap();

    assert_eq!(delete_resp.status(), StatusCode::OK);
    let delete_body: serde_json::Value = delete_resp.json().await.unwrap();
    assert_eq!(delete_body["id"], conv_id);
    assert_eq!(delete_body["object"], "conversation");
    assert!(delete_body["created_at"].is_number());

    // Verify it's gone
    let retrieve_resp = client
        .get(format!("{gw_url}/v1/conversations/{conv_id}/items/{item_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(retrieve_resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_nonexistent_item_returns_404() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let client = reqwest::Client::new();

    // Create conversation
    let create_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let create_body: serde_json::Value = create_resp.json().await.unwrap();
    let conv_id = create_body["id"].as_str().unwrap();

    // Try to delete nonexistent item
    let resp = client
        .delete(format!("{gw_url}/v1/conversations/{conv_id}/items/item_nonexistent"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_item_belongs_to_conversation_validation() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _h2) = spawn_gateway(state).await;

    let client = reqwest::Client::new();

    // Create two conversations
    let conv1_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({
            "items": [{"type": "message", "role": "user", "content": "Conv1 item"}]
        }))
        .send()
        .await
        .unwrap();
    let conv1_body: serde_json::Value = conv1_resp.json().await.unwrap();
    let conv1_id = conv1_body["id"].as_str().unwrap();

    let conv2_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let conv2_body: serde_json::Value = conv2_resp.json().await.unwrap();
    let conv2_id = conv2_body["id"].as_str().unwrap();

    // Get item from conv1
    let list_resp = client
        .get(format!("{gw_url}/v1/conversations/{conv1_id}/items"))
        .send()
        .await
        .unwrap();
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    let item_id = list_body["data"][0]["id"].as_str().unwrap();

    // Try to access conv1's item through conv2 - should be 404
    let wrong_conv_resp = client
        .get(format!("{gw_url}/v1/conversations/{conv2_id}/items/{item_id}"))
        .send()
        .await
        .unwrap();

    assert_eq!(wrong_conv_resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_create_items_rejects_empty_id() {
    let (llm_url, _llm) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _gateway) = spawn_gateway(state).await;
    let client = reqwest::Client::new();

    // Create conversation
    let conv_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let conv_body: serde_json::Value = conv_resp.json().await.unwrap();
    let conv_id = conv_body["id"].as_str().unwrap();

    // Try to create item with empty ID
    let create_resp = client
        .post(format!("{gw_url}/v1/conversations/{conv_id}/items"))
        .json(&json!({
            "items": [{
                "type": "message",
                "id": "",
                "role": "user",
                "content": "test"
            }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(create_resp.status(), StatusCode::BAD_REQUEST);
    let error_body: serde_json::Value = create_resp.json().await.unwrap();
    assert_eq!(error_body["error"]["type"], "invalid_request_error");
    assert_eq!(error_body["error"]["code"], "invalid_value");
    assert_eq!(error_body["error"]["param"], "items[0].id");
    assert_eq!(
        error_body["error"]["message"],
        "Invalid 'items[0].id': ''. Expected an ID that begins with 'msg'."
    );
}

#[tokio::test]
async fn test_create_items_rejects_item_already_in_conversation() {
    let (llm_url, _llm) = spawn_mock_llm().await;
    let state = test_state_with_storage(&llm_url).await;
    let (gw_url, _gateway) = spawn_gateway(state).await;
    let client = reqwest::Client::new();

    // Create conversation
    let conv_resp = client
        .post(format!("{gw_url}/v1/conversations"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let conv_body: serde_json::Value = conv_resp.json().await.unwrap();
    let conv_id = conv_body["id"].as_str().unwrap();

    let items_url = format!("{gw_url}/v1/conversations/{conv_id}/items");
    let original = client
        .post(&items_url)
        .json(&json!({"items": [{"role":"user", "content":"original"}]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let create_resp = client
        .post(&items_url)
        .json(&json!({"items": [
            {"role":"user", "content":"must not be inserted"},
            {"type":"message", "id":original["data"][0]["id"], "role":"user", "content":"replacement"}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::BAD_REQUEST);
    let error_body: serde_json::Value = create_resp.json().await.unwrap();
    assert_eq!(
        error_body,
        json!({"error": {
            "type":"invalid_request_error", "code":"item_already_in_conversation",
            "param":"items", "message":"Item already in conversation"
        }})
    );
    let listed = client
        .get(&items_url)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(listed["data"], original["data"]);
}

#[path = "conversations/regressions.rs"]
mod regressions;
