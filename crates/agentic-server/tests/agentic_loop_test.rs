#[allow(dead_code)]
mod common;

use common::{
    spawn_ogx, spawn_ogx_recording, spawn_vllm, spawn_vllm_recording, spawn_vllm_with_tool_calls, start_gateway,
};

#[tokio::test]
async fn test_passthrough_no_tools() {
    let (vllm_port, _h) = spawn_vllm().await;
    let (ogx_port, _h2) = spawn_ogx().await;
    let (gw_addr, _) = start_gateway(vllm_port, Some(ogx_port), None).await;

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
async fn test_single_file_search() {
    let tool_call_response = serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "file_search",
            "arguments": "{\"query\": \"test query\"}",
            "status": "completed"
        }]
    });

    let final_response = serde_json::json!({
        "id": "resp_2",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "Based on the search results..."}]
        }]
    });

    let (vllm_port, _h) = spawn_vllm_with_tool_calls(vec![tool_call_response, final_response]).await;
    let (ogx_port, _h2) = spawn_ogx().await;
    let (gw_addr, _) = start_gateway(vllm_port, Some(ogx_port), None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": [{"role": "user", "content": "search for something"}],
            "tools": [{"type": "file_search", "vector_store_ids": ["vs_123"]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["id"].as_str().unwrap_or("").starts_with("resp_"));
    assert_eq!(body["output"][0]["type"], "message");
}

#[tokio::test]
async fn test_file_search_backend_failure_returns_error() {
    let tool_call_response = serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "file_search",
            "arguments": "{\"query\": \"test query\"}",
            "status": "completed"
        }]
    });

    let final_response = serde_json::json!({
        "id": "resp_2",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "answer without search context"}]
        }]
    });

    let (vllm_port, _h) = spawn_vllm_with_tool_calls(vec![tool_call_response, final_response]).await;
    let (gw_addr, _) = start_gateway(vllm_port, None, None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": "search for something",
            "tools": [{"type": "file_search", "vector_store_ids": ["vs_123"]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 502);
    let body: serde_json::Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("file_search vector lookup failed"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn test_file_search_rejects_missing_query_argument() {
    let tool_call_response = serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "file_search",
            "arguments": "{}",
            "status": "completed"
        }]
    });

    let final_response = serde_json::json!({
        "id": "resp_2",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "answer without a query"}]
        }]
    });

    let (vllm_port, _h) = spawn_vllm_with_tool_calls(vec![tool_call_response, final_response]).await;
    let (ogx_port, _h2) = spawn_ogx().await;
    let (gw_addr, _) = start_gateway(vllm_port, Some(ogx_port), None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": "search for something",
            "tools": [{"type": "file_search", "vector_store_ids": ["vs_123"]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 502);
    let body: serde_json::Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("query"), "unexpected error: {msg}");
}

#[tokio::test]
async fn test_file_search_rejects_empty_vector_store_ids_before_vllm() {
    let response = serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "should not be called"}]
        }]
    });

    let (vllm_port, requests, _h) = spawn_vllm_recording(vec![response]).await;
    let (ogx_port, _h2) = spawn_ogx().await;
    let (gw_addr, _) = start_gateway(vllm_port, Some(ogx_port), None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": "search for something",
            "tools": [{"type": "file_search", "vector_store_ids": []}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("vector_store_ids"), "unexpected error: {msg}");
    assert!(
        requests.lock().await.is_empty(),
        "gateway should reject before calling vLLM"
    );
}

#[tokio::test]
async fn test_file_search_preserves_search_options() {
    let tool_call_response = serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "file_search",
            "arguments": "{\"query\": \"test query\"}",
            "status": "completed"
        }]
    });
    let final_response = serde_json::json!({
        "id": "resp_2",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "Based on filtered search results..."}]
        }]
    });

    let (vllm_port, _h) = spawn_vllm_with_tool_calls(vec![tool_call_response, final_response]).await;
    let (ogx_port, ogx_requests, _h2) = spawn_ogx_recording().await;
    let (gw_addr, _) = start_gateway(vllm_port, Some(ogx_port), None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": "search for something",
            "tools": [{
                "type": "file_search",
                "vector_store_ids": ["vs_123"],
                "max_num_results": 3,
                "filters": {"type": "eq", "key": "tenant_id", "value": "tenant-a"},
                "ranking_options": {"ranker": "default", "score_threshold": 0.25}
            }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let requests = ogx_requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["query"], "test query");
    assert_eq!(requests[0]["max_num_results"], 3);
    assert_eq!(requests[0]["filters"]["key"], "tenant_id");
    assert_eq!(requests[0]["ranking_options"]["score_threshold"], 0.25);
}

#[tokio::test]
async fn test_file_search_rejects_mixed_tool_calls() {
    let mixed_response = serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [
            {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "file_search",
                "arguments": "{\"query\": \"test query\"}",
                "status": "completed"
            },
            {
                "type": "function_call",
                "id": "fc_2",
                "call_id": "call_2",
                "name": "get_weather",
                "arguments": "{\"city\": \"SF\"}",
                "status": "completed"
            }
        ]
    });
    let final_response = serde_json::json!({
        "id": "resp_2",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "answer after dropping get_weather"}]
        }]
    });

    let (vllm_port, _h) = spawn_vllm_with_tool_calls(vec![mixed_response, final_response]).await;
    let (ogx_port, _h2) = spawn_ogx().await;
    let (gw_addr, _) = start_gateway(vllm_port, Some(ogx_port), None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": "search and call another function",
            "tools": [
                {"type": "file_search", "vector_store_ids": ["vs_123"]},
                {
                    "type": "function",
                    "name": "get_weather",
                    "description": "Get weather.",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 502);
    let body: serde_json::Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("mixed tool calls"), "unexpected error: {msg}");
}

#[tokio::test]
async fn test_file_search_streaming_rejected() {
    let (vllm_port, _h) = spawn_vllm().await;
    let (ogx_port, _h2) = spawn_ogx().await;
    let (gw_addr, _) = start_gateway(vllm_port, Some(ogx_port), None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": "search for something",
            "stream": true,
            "tools": [{"type": "file_search", "vector_store_ids": ["vs_123"]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("streaming file_search"), "unexpected error: {msg}");
}

#[tokio::test]
async fn test_previous_response_id_hydrates_history() {
    let first_response = serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "first answer"}]
        }]
    });

    let second_response = serde_json::json!({
        "id": "resp_2",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "msg_2",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "second answer"}]
        }]
    });

    let (vllm_port, requests, _h) = spawn_vllm_recording(vec![first_response, second_response]).await;
    let (ogx_port, _h2) = spawn_ogx().await;
    let (gw_addr, _) = start_gateway(vllm_port, Some(ogx_port), None).await;

    let client = reqwest::Client::new();
    let first = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": "first question",
            "tools": [{"type": "file_search", "vector_store_ids": ["vs_123"]}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let first_body: serde_json::Value = first.json().await.unwrap();
    let first_id = first_body["id"].as_str().expect("first response id");

    let second = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": "follow up",
            "previous_response_id": first_id,
            "tools": [{"type": "file_search", "vector_store_ids": ["vs_123"]}]
        }))
        .send()
        .await
        .unwrap();
    let second_status = second.status();
    let second_text = second.text().await.unwrap();
    assert_eq!(second_status, 200, "second response body: {second_text}");

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(requests[1].get("previous_response_id").is_none());
    let input = requests[1]["input"]
        .as_array()
        .expect("hydrated input should be an array");
    assert!(
        input.len() >= 3,
        "expected prior user/output plus follow-up input, got {input:?}"
    );
    assert!(input.iter().any(|item| item["content"] == "first question"));
    assert!(input.iter().any(|item| item["content"] == "follow up"));
}

#[tokio::test]
async fn test_multi_turn_tool_calls() {
    let turn1 = serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "file_search",
            "arguments": "{\"query\": \"first query\"}",
            "status": "completed"
        }]
    });

    let turn2 = serde_json::json!({
        "id": "resp_2",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "id": "fc_2",
            "call_id": "call_2",
            "name": "file_search",
            "arguments": "{\"query\": \"second query\"}",
            "status": "completed"
        }]
    });

    let final_resp = serde_json::json!({
        "id": "resp_3",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "final answer"}]
        }]
    });

    let (vllm_port, _h) = spawn_vllm_with_tool_calls(vec![turn1, turn2, final_resp]).await;
    let (ogx_port, _h2) = spawn_ogx().await;
    let (gw_addr, _) = start_gateway(vllm_port, Some(ogx_port), None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": [{"role": "user", "content": "multi-turn search"}],
            "tools": [{"type": "file_search", "vector_store_ids": ["vs_123"]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["id"].as_str().unwrap_or("").starts_with("resp_"));
}

#[tokio::test]
async fn test_max_iterations_reached() {
    let tool_call = serde_json::json!({
        "id": "resp_loop",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "id": "fc_loop",
            "call_id": "call_loop",
            "name": "file_search",
            "arguments": "{\"query\": \"infinite loop\"}",
            "status": "completed"
        }]
    });

    let (vllm_port, _h) = spawn_vllm_with_tool_calls(vec![tool_call]).await;
    let (ogx_port, _h2) = spawn_ogx().await;
    let (gw_addr, _) = start_gateway(vllm_port, Some(ogx_port), None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": [{"role": "user", "content": "search forever"}],
            "tools": [{"type": "file_search", "vector_store_ids": ["vs_123"]}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 502);
    let body: serde_json::Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("exceeded"), "expected max iterations error, got: {msg}");
}
