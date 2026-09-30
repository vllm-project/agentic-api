// Runs only with the Linux code-interpreter worker feature.
#![cfg(all(feature = "embedded-code-interpreter", target_os = "linux"))]

#[allow(dead_code)]
mod common;

use std::collections::VecDeque;
use std::sync::Arc;

use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex};

use agentic_core::executor::ExecutionContext;
use common::{spawn_gateway, test_config, test_state};

/// Exercises the public HTTP route, upstream tool normalization, the tool loop,
/// and the actual isolated Eryx worker in one request.
#[tokio::test]
#[ignore = "requires an Eryx runtime, worker binary, private TMPDIR, and delegated cgroup v2"]
async fn http_code_interpreter_executes_in_isolated_worker() {
    let responses = Arc::new(Mutex::new(VecDeque::from([
        json!({
            "id": "upstream_call", "object": "response", "status": "completed",
            "model": "test", "created_at": 0,
            "output": [{
                "type": "function_call", "id": "fc_worker_http", "call_id": "call_worker_http",
                "name": "code_interpreter", "arguments": "{\"code\":\"print(6 * 7)\"}",
                "status": "completed"
            }]
        }),
        json!({
            "id": "upstream_final", "object": "response", "status": "completed",
            "model": "test", "created_at": 0,
            "output": [{
                "type": "message", "id": "msg_worker_http", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "The answer is 42."}]
            }]
        }),
    ])));
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let queued_responses = Arc::clone(&responses);
    let captured_requests = Arc::clone(&requests);
    let upstream = Router::new().route(
        "/v1/responses",
        post(move |Json(request): Json<Value>| {
            let queued_responses = Arc::clone(&queued_responses);
            let captured_requests = Arc::clone(&captured_requests);
            async move {
                captured_requests.lock().await.push(request);
                Json(
                    queued_responses
                        .lock()
                        .await
                        .pop_front()
                        .expect("unexpected upstream round"),
                )
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock upstream socket");
    let upstream_url = format!("http://{}", listener.local_addr().expect("mock upstream address"));
    let upstream_task =
        tokio::spawn(async move { axum::serve(listener, upstream).await.expect("mock upstream server") });

    let db = tempfile::NamedTempFile::new().expect("temporary test database");
    let mut config = test_config(&upstream_url);
    config.db_url = Some(format!("sqlite://{}", db.path().display()));
    config.tools.code_interpreter.enabled = true;
    let mut state = test_state(&config);
    state.exec_ctx = Arc::new(
        ExecutionContext::from_config(&config)
            .await
            .expect("ready Eryx executor"),
    );
    let (gateway_url, gateway_task) = spawn_gateway(state).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/responses"))
        .json(&json!({
            "model": "test", "input": "Calculate six times seven.",
            "tools": [{"type": "code_interpreter", "container": {"type": "auto"}}],
            "store": true, "stream": false
        }))
        .send()
        .await
        .expect("gateway HTTP response");
    let status = response.status();
    let body: Value = response.json().await.expect("gateway JSON response");
    assert_eq!(status, reqwest::StatusCode::OK, "gateway response: {body}");
    let call = body["output"]
        .as_array()
        .expect("response output")
        .iter()
        .find(|item| item["type"] == "code_interpreter_call")
        .expect("public code interpreter call");
    assert_eq!(call["id"], "ci_worker_http");
    assert_eq!(call["container_id"], "cntr_worker_http");
    assert_eq!(call["status"], "completed");
    assert_eq!(call["code"], "print(6 * 7)");
    assert!(call["outputs"].to_string().contains("42"), "worker output: {call}");

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2, "upstream must receive a second inference round");
    assert_eq!(requests[0]["tools"][0]["name"], "code_interpreter");
    assert!(
        requests[1]["input"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| {
                item["type"] == "function_call_output"
                    && item["call_id"] == "call_worker_http"
                    && item["output"].to_string().contains("42")
            })),
        "second round must include the worker result: {}",
        requests[1]
    );

    gateway_task.abort();
    upstream_task.abort();
}
