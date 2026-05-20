#[allow(dead_code)]
mod common;

use common::{spawn_ogx, spawn_vllm, spawn_vllm_with_tool_calls, start_gateway};

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
    assert_eq!(body["id"], "resp_2");
    assert_eq!(body["output"][0]["type"], "message");
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
    assert_eq!(body["id"], "resp_3");
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
