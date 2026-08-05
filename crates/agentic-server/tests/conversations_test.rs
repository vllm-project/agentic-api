mod common;

use http::StatusCode;

use common::{spawn_gateway, spawn_mock_llm, storage_backed_state, test_config, test_state};

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_conversations_create_retrieve_update_items_and_delete() {
    let (llm_url, _llm) = spawn_mock_llm().await;
    let fixture = storage_backed_state(&llm_url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{gateway_url}/v1/conversations"))
        .json(&serde_json::json!({
            "metadata": {"project": "agentic"},
            "items": [
                {"type": "message", "role": "user", "content": "hello"},
                {
                    "type": "function_call",
                    "id": "fc_initial",
                    "call_id": "call_initial",
                    "name": "lookup",
                    "arguments": "{}",
                    "status": "completed"
                },
                {"type": "function_call_output", "call_id": "call_initial", "output": "done"},
                {
                    "type": "custom_tool_call",
                    "id": "ctc_initial",
                    "call_id": "call_custom",
                    "name": "echo",
                    "input": "hello",
                    "status": "completed"
                },
                {"type": "custom_tool_call_output", "call_id": "call_custom", "output": {"value": "done"}}
            ]
        }))
        .send()
        .await
        .expect("create conversation request");
    assert_eq!(response.status(), StatusCode::OK);
    let conversation: serde_json::Value = response.json().await.expect("conversation response");
    let conversation_id = conversation["id"].as_str().expect("conversation ID");
    assert_eq!(conversation["object"], "conversation");
    assert_eq!(conversation["metadata"]["project"], "agentic");

    let response = client
        .get(format!("{gateway_url}/v1/conversations/{conversation_id}"))
        .send()
        .await
        .expect("retrieve conversation request");
    assert_eq!(response.status(), StatusCode::OK);
    let retrieved: serde_json::Value = response.json().await.expect("retrieved conversation");
    assert_eq!(retrieved["id"], conversation_id);
    assert_eq!(retrieved["metadata"]["project"], "agentic");

    let response = client
        .get(format!(
            "{gateway_url}/v1/conversations/{conversation_id}/items?order=asc"
        ))
        .send()
        .await
        .expect("list initial items request");
    assert_eq!(response.status(), StatusCode::OK);
    let initial_items: serde_json::Value = response.json().await.expect("initial item list");
    let initial_data = initial_items["data"].as_array().expect("initial item data");
    assert_eq!(initial_data.len(), 5);
    assert_eq!(
        initial_data
            .iter()
            .map(|item| item["type"].as_str().expect("item type"))
            .collect::<Vec<_>>(),
        vec![
            "message",
            "function_call",
            "function_call_output",
            "custom_tool_call",
            "custom_tool_call_output"
        ]
    );
    let first_item_id = initial_items["data"][0]["id"].as_str().expect("initial item ID");
    let second_initial_item_id = initial_items["data"][1]["id"].as_str().expect("second initial item ID");
    assert_eq!(initial_items["data"][0]["type"], "message");
    assert!(!initial_items["has_more"].as_bool().expect("has_more"));

    let response = client
        .post(format!("{gateway_url}/v1/conversations/{conversation_id}"))
        .json(&serde_json::json!({"metadata": {"project": "updated"}}))
        .send()
        .await
        .expect("update conversation request");
    assert_eq!(response.status(), StatusCode::OK);
    let updated: serde_json::Value = response.json().await.expect("updated conversation");
    assert_eq!(updated["metadata"]["project"], "updated");

    let response = client
        .post(format!("{gateway_url}/v1/conversations/{conversation_id}/items"))
        .json(&serde_json::json!({
            "items": [{"type": "message", "role": "user", "content": "follow up"}]
        }))
        .send()
        .await
        .expect("append items request");
    assert_eq!(response.status(), StatusCode::OK);
    let appended: serde_json::Value = response.json().await.expect("appended item list");
    let appended_item_id = appended["data"][0]["id"].as_str().expect("appended item ID");
    assert_ne!(first_item_id, appended_item_id);

    let response = client
        .get(format!(
            "{gateway_url}/v1/conversations/{conversation_id}/items?order=asc&limit=1"
        ))
        .send()
        .await
        .expect("list paginated items request");
    assert_eq!(response.status(), StatusCode::OK);
    let page: serde_json::Value = response.json().await.expect("paginated item list");
    assert_eq!(page["data"].as_array().expect("page data").len(), 1);
    assert_eq!(page["data"][0]["id"], first_item_id);
    assert!(page["has_more"].as_bool().expect("page has_more"));

    let response = client
        .get(format!(
            "{gateway_url}/v1/conversations/{conversation_id}/items?order=asc&after={first_item_id}&limit=1"
        ))
        .send()
        .await
        .expect("list items after cursor request");
    assert_eq!(response.status(), StatusCode::OK);
    let after_page: serde_json::Value = response.json().await.expect("after cursor page");
    assert_eq!(after_page["data"].as_array().expect("after page data").len(), 1);
    assert_eq!(after_page["data"][0]["id"], second_initial_item_id);
    assert!(after_page["has_more"].as_bool().expect("after page has_more"));

    let response = client
        .get(format!(
            "{gateway_url}/v1/conversations/{conversation_id}/items?limit=1"
        ))
        .send()
        .await
        .expect("default descending item page request");
    assert_eq!(response.status(), StatusCode::OK);
    let descending_page: serde_json::Value = response.json().await.expect("descending page");
    assert_eq!(descending_page["data"][0]["id"], appended_item_id);

    let response = client
        .get(format!(
            "{gateway_url}/v1/conversations/{conversation_id}/items/{appended_item_id}"
        ))
        .send()
        .await
        .expect("retrieve item request");
    assert_eq!(response.status(), StatusCode::OK);
    let item: serde_json::Value = response.json().await.expect("retrieved item");
    assert_eq!(item["id"], appended_item_id);

    let response = client
        .delete(format!(
            "{gateway_url}/v1/conversations/{conversation_id}/items/{appended_item_id}"
        ))
        .send()
        .await
        .expect("delete item request");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .json::<serde_json::Value>()
            .await
            .expect("delete item response")["id"],
        conversation_id
    );

    let response = client
        .get(format!(
            "{gateway_url}/v1/conversations/{conversation_id}/items/{appended_item_id}"
        ))
        .send()
        .await
        .expect("retrieve deleted item request");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = client
        .delete(format!("{gateway_url}/v1/conversations/{conversation_id}"))
        .send()
        .await
        .expect("delete conversation request");
    assert_eq!(response.status(), StatusCode::OK);
    let deleted: serde_json::Value = response.json().await.expect("deleted conversation");
    assert_eq!(deleted["object"], "conversation.deleted");
    assert!(deleted["deleted"].as_bool().expect("deleted flag"));

    let response = client
        .get(format!("{gateway_url}/v1/conversations/{conversation_id}"))
        .send()
        .await
        .expect("retrieve deleted conversation request");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_unknown_conversation_is_not_created_by_responses() {
    let (llm_url, _llm) = spawn_mock_llm().await;
    let fixture = storage_backed_state(&llm_url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/responses"))
        .json(&serde_json::json!({
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "hello"}],
            "conversation": "conv_missing"
        }))
        .send()
        .await
        .expect("response request");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json().await.expect("error body");
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn test_conversation_and_item_not_found_errors_cover_public_routes() {
    let (llm_url, _llm) = spawn_mock_llm().await;
    let fixture = storage_backed_state(&llm_url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state).await;
    let client = reqwest::Client::new();
    let missing_conversation = "conv_missing_public";

    for response in [
        client
            .get(format!("{gateway_url}/v1/conversations/{missing_conversation}"))
            .send()
            .await
            .expect("retrieve missing conversation request"),
        client
            .post(format!("{gateway_url}/v1/conversations/{missing_conversation}"))
            .json(&serde_json::json!({"metadata": {}}))
            .send()
            .await
            .expect("update missing conversation request"),
        client
            .delete(format!("{gateway_url}/v1/conversations/{missing_conversation}"))
            .send()
            .await
            .expect("delete missing conversation request"),
        client
            .get(format!("{gateway_url}/v1/conversations/{missing_conversation}/items"))
            .send()
            .await
            .expect("list missing conversation items request"),
        client
            .post(format!("{gateway_url}/v1/conversations/{missing_conversation}/items"))
            .json(&serde_json::json!({"items": [{"type": "message", "role": "user", "content": "x"}]}))
            .send()
            .await
            .expect("append to missing conversation request"),
    ] {
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.json::<serde_json::Value>().await.expect("not found body")["error"]["code"],
            "not_found"
        );
    }

    let response = client
        .post(format!("{gateway_url}/v1/conversations"))
        .send()
        .await
        .expect("create conversation request");
    let conversation: serde_json::Value = response.json().await.expect("conversation response");
    let conversation_id = conversation["id"].as_str().expect("conversation ID");
    for response in [
        client
            .get(format!(
                "{gateway_url}/v1/conversations/{conversation_id}/items/item_missing_public"
            ))
            .send()
            .await
            .expect("retrieve missing item request"),
        client
            .delete(format!(
                "{gateway_url}/v1/conversations/{conversation_id}/items/item_missing_public"
            ))
            .send()
            .await
            .expect("delete missing item request"),
    ] {
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.json::<serde_json::Value>().await.expect("item not found body")["error"]["code"],
            "not_found"
        );
    }
}

#[tokio::test]
async fn test_conversations_without_storage_return_server_error_not_client_error() {
    let (llm_url, _llm) = spawn_mock_llm().await;
    let (gateway_url, _gateway) = spawn_gateway(test_state(&test_config(&llm_url))).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/conversations"))
        .json(&serde_json::json!({"metadata": {}}))
        .send()
        .await
        .expect("conversation request");

    assert!(!response.status().is_client_error());
}

#[tokio::test]
async fn test_conversations_empty_body_defaults_to_create_request() {
    let (llm_url, _llm) = spawn_mock_llm().await;
    let fixture = storage_backed_state(&llm_url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/conversations"))
        .send()
        .await
        .expect("conversation request");

    assert_eq!(response.status(), StatusCode::OK);
    let conversation: serde_json::Value = response.json().await.expect("conversation response");
    assert_eq!(conversation["object"], "conversation");
    assert_eq!(conversation["metadata"], serde_json::json!({}));
    assert!(conversation["id"].as_str().is_some());
}
