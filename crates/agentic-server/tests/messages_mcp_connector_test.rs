//! Handler-to-HTTPS acceptance tests. Requires Python 3 and OpenSSL on Unix test hosts.
#![cfg(unix)]
#[allow(dead_code)]
mod common;
#[path = "messages_mcp_connector/fixture.rs"]
mod fixture;

use std::sync::{Arc, Mutex};

use agentic_core::tool::mcp::client::test_support::with_root_certificate;
use agentic_server::app::{ServerConfig, build_router};
use axum::Router;
use axum::body::Body;
use http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;

fn request(url: &str, stream: bool, mode: &str) -> Value {
    let deferred = mode == "deferred";
    json!({"model":mode,"max_tokens":128,"stream":stream,"extension":{"preserved":true},
        "messages":[{"role":"user","content":"use echo"}],
        "mcp_servers":[{"type":"url","name":"counter","url":url,"authorization_token":"connector-secret"}],
        "tools":[{"type":"tool_search_tool_regex_20251119","name":"tool_search_tool_regex"},
            {"type":"mcp_toolset","mcp_server_name":"counter","default_config":{"enabled":false,"defer_loading":true},
            "configs":{"echo":{"enabled":true,"defer_loading":deferred},"fail":{"enabled":true,"defer_loading":false}}}]})
}

async fn post(router: &Router, path: &str, body: &Value) -> (StatusCode, String) {
    let response = router
        .clone()
        .oneshot(
            http::Request::post(path)
                .header(http::header::CONTENT_TYPE, "application/json")
                .header("x-api-key", "test-key")
                .header("anthropic-beta", "mcp-client-2025-11-20")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn trusted_post(router: &Router, path: &str, body: &Value, cert: &[u8]) -> (StatusCode, String) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = Arc::clone(&logs);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .with_writer(move || LogWriter(Arc::clone(&writer)))
        .finish();
    let response = with_root_certificate(cert.to_vec(), post(router, path, body).with_subscriber(subscriber))
        .await
        .unwrap();
    let log = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(!log.contains("connector-secret"));
    assert!(!log.contains("wrong-secret"));
    assert!(!response.1.contains("connector-secret"));
    assert!(!response.1.contains("wrong-secret"));
    response
}

fn blocks(response: &str, stream: bool) -> Vec<Value> {
    if !stream {
        let response: Value = serde_json::from_str(response).unwrap();
        assert_eq!(response["usage"]["output_tokens"], 6);
        return response["content"].as_array().unwrap().clone();
    }
    let events = response
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        events.iter().filter(|event| event["type"] == "message_start").count(),
        1
    );
    assert_eq!(events.iter().filter(|event| event["type"] == "message_stop").count(), 1);
    let starts = events
        .iter()
        .filter(|event| event["type"] == "content_block_start")
        .collect::<Vec<_>>();
    let stops = events
        .iter()
        .filter(|event| event["type"] == "content_block_stop")
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), stops.len());
    for (index, (start, stop)) in starts.iter().zip(stops).enumerate() {
        assert_eq!(start["index"], index);
        assert_eq!(stop["index"], index);
    }
    let terminal = events.iter().find(|event| event["type"] == "message_delta").unwrap();
    assert_eq!(terminal["usage"]["output_tokens"], 6);
    starts
        .into_iter()
        .map(|start| {
            let mut block = start["content_block"].clone();
            let input = events
                .iter()
                .filter(|event| event["type"] == "content_block_delta" && event["index"] == start["index"])
                .filter_map(|event| event["delta"]["partial_json"].as_str())
                .collect::<String>();
            if !input.is_empty() {
                block["input"] = serde_json::from_str(&input).unwrap();
            }
            block
        })
        .collect()
}

fn assert_normalized(body: &Value) {
    assert!(body.get("mcp_servers").is_none());
    assert!(!body.to_string().contains("connector-secret"));
    assert_eq!(body["extension"]["preserved"], true);
    let tools = body["tools"].as_array().unwrap();
    assert!(tools.iter().all(|tool| tool["type"] != "mcp_toolset"));
    assert!(tools.iter().any(|tool| tool["name"] == "mcp__counter__echo"));
    assert!(tools.iter().any(|tool| tool["name"] == "mcp__counter__fail"));
    assert!(!tools.iter().any(|tool| tool["name"] == "mcp__counter__disabled"));
}

#[tokio::test]
async fn https_bearer_json_sse_search_replay_and_count_tokens() {
    let mcp = fixture::HttpsMcp::start().await;
    let (url, requests, task) = fixture::inference().await;
    let router = build_router(
        common::test_state(&common::test_config(&url)),
        &ServerConfig::from_env(),
    );
    for stream in [false, true] {
        for mode in ["echo", "fail", "deferred"] {
            let body = request(&mcp.url, stream, mode);
            let before = requests.lock().await.len();
            let (status, response) = trusted_post(&router, "/v1/messages", &body, &mcp.certificate).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            let content = blocks(&response, stream);
            let call = content.iter().find(|block| block["type"] == "mcp_tool_use").unwrap();
            assert_eq!(call["id"], "call");
            assert_eq!(call["server_name"], "counter");
            assert_eq!(call["input"]["text"], "hello");
            assert_eq!(call["name"], if mode == "fail" { "fail" } else { "echo" });
            let result = content.iter().find(|block| block["type"] == "mcp_tool_result").unwrap();
            assert_eq!(result["tool_use_id"], "call");
            assert_eq!(result["is_error"], mode == "fail");
            let output = result["content"][0]["text"].as_str().unwrap();
            if mode == "fail" {
                assert!(output.contains("fixture error"));
            } else {
                assert_eq!(output, "fixture output: hello");
            }
            let captured = requests.lock().await;
            assert_eq!(captured.len(), before + 2);
            assert_normalized(&captured[before]);
            assert_eq!(captured[before + 1]["messages"][2]["content"][0]["type"], "tool_result");
            if mode == "deferred" {
                assert_eq!(
                    captured[before + 1]["messages"][1]["content"][0]["input"]["pattern"],
                    "echo"
                );
                assert_eq!(
                    captured[before + 1]["messages"][1]["content"][1]["content"]["tool_references"][0]["tool_name"],
                    "mcp__counter__echo"
                );
            }
            drop(captured);
            verify_replay(&router, &mcp, &requests, &body, content, mode).await;
            verify_count(&router, &mcp, &requests, body).await;
        }
    }
    let observations = mcp.observations().await;
    assert!(observations.iter().all(|entry| entry["authorized"] == true));
    let calls = observations
        .iter()
        .filter(|entry| entry["method"] == "tools/call")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 6);
    assert!(
        calls
            .iter()
            .all(|entry| entry["params"]["arguments"]["text"] == "hello")
    );
    task.abort();
    let _ = task.await;
    mcp.stop().await;
}

async fn verify_replay(
    router: &Router,
    mcp: &fixture::HttpsMcp,
    requests: &fixture::Requests,
    body: &Value,
    content: Vec<Value>,
    mode: &str,
) {
    let tool = if mode == "fail" { "fail" } else { "echo" };
    let internal_name = format!("mcp__counter__{tool}");
    let mut replay = body.clone();
    replay["tools"][1]["configs"].as_object_mut().unwrap().remove(tool);
    replay["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"assistant","content":content}));
    let before = requests.lock().await.len();
    let (status, response) = trusted_post(router, "/v1/messages", &replay, &mcp.certificate).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let captured = requests.lock().await;
    assert_eq!(captured.len(), before + 1);
    let last = captured.last().unwrap();
    let history = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .collect::<Vec<_>>();
    let call = history.iter().find(|block| block["type"] == "tool_use").unwrap();
    assert_eq!(call["name"], internal_name);
    assert_eq!(call["input"]["text"], "hello");
    assert!(
        history
            .iter()
            .any(|block| block["type"] == "tool_result" && block["tool_use_id"] == "call")
    );
    assert!(
        !last["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == internal_name)
    );
    if mode == "deferred" {
        let search = history.iter().find(|block| block["type"] == "server_tool_use").unwrap();
        assert_eq!(search["input"]["pattern"], "echo");
        let result = history
            .iter()
            .find(|block| block["type"] == "tool_search_tool_result")
            .unwrap();
        assert_eq!(result["content"]["tool_references"], json!([]));
    }
}

async fn verify_count(router: &Router, mcp: &fixture::HttpsMcp, requests: &fixture::Requests, mut body: Value) {
    let calls_before = mcp
        .observations()
        .await
        .into_iter()
        .filter(|entry| entry["method"] == "tools/call")
        .count();
    body.as_object_mut().unwrap().remove("max_tokens");
    let (status, response) = trusted_post(router, "/v1/messages/count_tokens", &body, &mcp.certificate).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(serde_json::from_str::<Value>(&response).unwrap()["input_tokens"], 42);
    assert_normalized(requests.lock().await.last().unwrap());
    assert_eq!(
        mcp.observations()
            .await
            .into_iter()
            .filter(|entry| entry["method"] == "tools/call")
            .count(),
        calls_before
    );
}

#[tokio::test]
async fn tls_auth_and_deferred_configuration_fail_before_inference() {
    let mcp = fixture::HttpsMcp::start().await;
    let (url, requests, task) = fixture::inference().await;
    let router = build_router(
        common::test_state(&common::test_config(&url)),
        &ServerConfig::from_env(),
    );
    let body = request(&mcp.url, false, "deferred");
    let (status, response) = post(&router, "/v1/messages", &body).await;
    assert!(!status.is_success(), "an untrusted fixture certificate must fail");
    assert!(!response.contains("connector-secret"));
    let mut wrong = body.clone();
    wrong["mcp_servers"][0]["authorization_token"] = json!("wrong-secret");
    let (status, _) = trusted_post(&router, "/v1/messages", &wrong, &mcp.certificate).await;
    assert!(!status.is_success());
    assert!(
        mcp.observations()
            .await
            .iter()
            .any(|entry| entry["authorized"] == false)
    );
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        let mut missing = body.clone();
        missing["tools"].as_array_mut().unwrap().remove(0);
        let (status, response) = trusted_post(&router, path, &missing, &mcp.certificate).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert!(response.contains("upstream-hosted tool search"));
        let mut deferred_search = body.clone();
        deferred_search["tools"][0]["defer_loading"] = json!(true);
        let (status, response) = trusted_post(&router, path, &deferred_search, &mcp.certificate).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert!(response.contains("cannot be deferred"));
    }
    assert!(requests.lock().await.is_empty());
    task.abort();
    let _ = task.await;
    mcp.stop().await;
}

#[tokio::test]
async fn truncated_calls_are_public_but_never_executed() {
    let mcp = fixture::HttpsMcp::start().await;
    let (url, requests, task) = fixture::inference().await;
    let router = build_router(
        common::test_state(&common::test_config(&url)),
        &ServerConfig::from_env(),
    );
    for stream in [false, true] {
        for mode in ["truncated", "truncated_mixed"] {
            let body = request(&mcp.url, stream, mode);
            let (status, response) = trusted_post(&router, "/v1/messages", &body, &mcp.certificate).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            let content = if stream {
                let events = response
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .map(|data| serde_json::from_str::<Value>(data).unwrap())
                    .collect::<Vec<_>>();
                let terminal = events.iter().find(|event| event["type"] == "message_delta").unwrap();
                assert_eq!(terminal["delta"]["stop_reason"], "max_tokens");
                assert_eq!(terminal["usage"]["output_tokens"], 3);
                assert_eq!(events.iter().filter(|event| event["type"] == "message_stop").count(), 1);
                events
                    .into_iter()
                    .filter(|event| event["type"] == "content_block_start")
                    .map(|event| event["content_block"].clone())
                    .collect::<Vec<_>>()
            } else {
                let message: Value = serde_json::from_str(&response).unwrap();
                assert_eq!(message["stop_reason"], "max_tokens");
                assert_eq!(message["usage"]["output_tokens"], 3);
                message["content"].as_array().unwrap().clone()
            };
            let call = content.iter().find(|block| block["type"] == "mcp_tool_use").unwrap();
            assert_eq!(call["server_name"], "counter");
            assert_eq!(call["name"], "echo");
            assert!(!content.iter().any(|block| block["type"] == "mcp_tool_result"));
            assert_eq!(
                content.iter().any(|block| block["name"] == "client_echo"),
                mode == "truncated_mixed"
            );
        }
    }
    assert_eq!(requests.lock().await.len(), 4);
    assert!(
        !mcp.observations()
            .await
            .iter()
            .any(|entry| entry["method"] == "tools/call")
    );
    task.abort();
    let _ = task.await;
    mcp.stop().await;
}

#[tokio::test]
async fn malformed_streams_never_dispatch_mcp_calls() {
    let mcp = fixture::HttpsMcp::start().await;
    let (url, requests, task) = fixture::inference().await;
    let router = build_router(
        common::test_state(&common::test_config(&url)),
        &ServerConfig::from_env(),
    );
    for mode in [
        "missing_stop",
        "open_block",
        "missing_start",
        "duplicate_block",
        "delta_after_stop",
    ] {
        let body = request(&mcp.url, true, mode);
        let (status, response) = trusted_post(&router, "/v1/messages", &body, &mcp.certificate).await;
        assert_eq!(status, StatusCode::OK);
        assert!(response.contains("event: error"), "{mode}: {response}");
        assert!(!response.contains("event: message_stop"), "{mode}: {response}");
        assert!(!response.contains("mcp_tool_result"), "{mode}: {response}");
    }
    assert_eq!(requests.lock().await.len(), 5);
    assert!(
        !mcp.observations()
            .await
            .iter()
            .any(|entry| entry["method"] == "tools/call")
    );
    task.abort();
    let _ = task.await;
    mcp.stop().await;
}

/// Reconstruct the actual client-visible message, including fragmented MCP arguments.
fn response_content(response: &str, stream: bool) -> Vec<Value> {
    if !stream {
        return serde_json::from_str::<Value>(response).unwrap()["content"]
            .as_array()
            .unwrap()
            .clone();
    }
    let mut blocks = std::collections::BTreeMap::new();
    let mut inputs = std::collections::HashMap::<u64, String>::new();
    let mut stops = std::collections::HashSet::new();
    let mut terminal = false;
    for event in response
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str::<Value>(data).unwrap())
    {
        match event["type"].as_str().unwrap() {
            "content_block_start" => {
                assert!(
                    blocks
                        .insert(event["index"].as_u64().unwrap(), event["content_block"].clone())
                        .is_none()
                );
            }
            "content_block_delta" => {
                let index = event["index"].as_u64().unwrap();
                match event["delta"]["type"].as_str().unwrap() {
                    "input_json_delta" => inputs
                        .entry(index)
                        .or_default()
                        .push_str(event["delta"]["partial_json"].as_str().unwrap()),
                    "text_delta" => {
                        let block = blocks.get_mut(&index).unwrap();
                        let text = format!(
                            "{}{}",
                            block["text"].as_str().unwrap_or_default(),
                            event["delta"]["text"].as_str().unwrap()
                        );
                        block["text"] = json!(text);
                    }
                    other => panic!("unexpected delta {other}"),
                }
            }
            "content_block_stop" => {
                assert!(stops.insert(event["index"].as_u64().unwrap()));
            }
            "message_stop" => {
                assert!(!terminal);
                terminal = true;
            }
            "error" => panic!("unexpected stream error: {event}"),
            _ => {}
        }
    }
    assert!(terminal);
    assert_eq!(blocks.len(), stops.len());
    assert_eq!(
        blocks.keys().copied().collect::<Vec<_>>(),
        (0..blocks.len() as u64).collect::<Vec<_>>()
    );
    for (index, input) in inputs {
        blocks.get_mut(&index).unwrap()["input"] = serde_json::from_str(&input).unwrap();
    }
    blocks.into_values().collect()
}

async fn call_count(mcp: &fixture::HttpsMcp) -> usize {
    mcp.observations()
        .await
        .iter()
        .filter(|entry| entry["method"] == "tools/call")
        .count()
}

#[tokio::test]
async fn mixed_calls_wait_for_client_output_before_executing_mcp() {
    let mcp = fixture::HttpsMcp::start().await;
    let (url, requests, task) = fixture::inference().await;
    let router = build_router(
        common::test_state(&common::test_config(&url)),
        &ServerConfig::from_env(),
    );
    // Include transport changes between initial response and continuation.
    for initial_stream in [false, true] {
        for stream in [false, true] {
            let mut body = request(&mcp.url, initial_stream, "mixed");
            body["tools"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":"client_echo","input_schema":{"type":"object"}}));
            body["tool_choice"] = json!({"type":"any","disable_parallel_tool_use":false,"extension":"preserved"});
            let before_calls = call_count(&mcp).await;
            let (status, response) = trusted_post(&router, "/v1/messages", &body, &mcp.certificate).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            let content = response_content(&response, initial_stream);
            assert_eq!(
                content
                    .iter()
                    .map(|block| block["type"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["mcp_tool_use", "tool_use"]
            );
            assert_eq!(content[0]["input"], json!({"text":"hello"}));
            assert_eq!(
                call_count(&mcp).await,
                before_calls,
                "initial mixed response must not execute MCP"
            );
            if !initial_stream {
                assert_eq!(
                    serde_json::from_str::<Value>(&response).unwrap()["stop_reason"],
                    "tool_use"
                );
            }
            body["stream"] = json!(stream);
            body["messages"].as_array_mut().unwrap().extend([
                json!({"role":"assistant","content":content}),
                json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"client","content":"client output"}]}),
            ]);
            let before = requests.lock().await.len();
            let (status, response) = trusted_post(&router, "/v1/messages", &body, &mcp.certificate).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            assert_eq!(call_count(&mcp).await, before_calls + 1);
            let continued = response_content(&response, stream);
            assert_eq!(
                continued
                    .iter()
                    .map(|block| block["type"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["mcp_tool_result", "text"]
            );
            assert_eq!(continued[0]["tool_use_id"], "call");
            assert_eq!(continued[0]["is_error"], false);
            assert!(
                continued[0]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("fixture output: hello")
            );
            {
                let captured = requests.lock().await;
                assert_eq!(captured.len(), before + 1);
                assert_eq!(
                    captured.last().unwrap()["tool_choice"],
                    json!({"type":"auto","disable_parallel_tool_use":false,"extension":"preserved"})
                );
                let history = captured.last().unwrap()["messages"].as_array().unwrap();
                assert_eq!(history.len(), 3);
                assert_eq!(history[1]["content"][0]["name"], "mcp__counter__echo");
                assert_eq!(history[1]["content"][1]["name"], "client_echo");
                assert_eq!(history[2]["content"][0]["tool_use_id"], "client");
                assert_eq!(history[2]["content"][1]["tool_use_id"], "call");
            }
            body["messages"].as_array_mut().unwrap().extend([
                json!({"role":"assistant","content":continued}),
                json!({"role":"user","content":"continue"}),
            ]);
            let (status, response) = trusted_post(&router, "/v1/messages", &body, &mcp.certificate).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            assert_eq!(
                call_count(&mcp).await,
                before_calls + 1,
                "completed history must not execute MCP again"
            );
            assert!(
                !response_content(&response, stream)
                    .iter()
                    .any(|block| block["type"] == "mcp_tool_result")
            );
        }
    }
    task.abort();
    let _ = task.await;
    mcp.stop().await;
}

#[tokio::test]
async fn invalid_mixed_continuations_never_execute_mcp_or_start_inference() {
    let mcp = fixture::HttpsMcp::start().await;
    let (url, requests, task) = fixture::inference().await;
    let router = build_router(
        common::test_state(&common::test_config(&url)),
        &ServerConfig::from_env(),
    );
    for stream in [false, true] {
        for mode in [
            "missing",
            "wrong_id",
            "duplicate",
            "text",
            "removed",
            "disabled",
            "bad_input",
            "old_pending",
        ] {
            let mut body = request(&mcp.url, stream, "mixed");
            body["tools"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":"client_echo","input_schema":{"type":"object"}}));
            body["messages"].as_array_mut().unwrap().extend([
                json!({"role":"assistant","content":[
                    {"type":"mcp_tool_use","id":"call","server_name":"counter","name":"echo","input":{"text":"hello"}},
                    {"type":"tool_use","id":"client","name":"client_echo","input":{}}
                ]}),
                json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"client","content":"client output"}]}),
            ]);
            match mode {
                "missing" => body["messages"][2]["content"] = json!([]),
                "wrong_id" => body["messages"][2]["content"][0]["tool_use_id"] = json!("wrong"),
                "duplicate" => {
                    let result = body["messages"][2]["content"][0].clone();
                    body["messages"][2]["content"].as_array_mut().unwrap().push(result);
                }
                "text" => body["messages"][2]["content"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"type":"text","text":"extra"})),
                "removed" => {
                    body.as_object_mut().unwrap().remove("mcp_servers");
                    body["tools"]
                        .as_array_mut()
                        .unwrap()
                        .retain(|tool| tool["type"] != "mcp_toolset");
                }
                "disabled" => body["tools"][1]["configs"]["echo"]["enabled"] = json!(false),
                "bad_input" => body["messages"][1]["content"][0]["input"] = json!("broken"),
                "old_pending" => body["messages"].as_array_mut().unwrap().extend([
                    json!({"role":"assistant","content":"another turn"}),
                    json!({"role":"user","content":"continue"}),
                ]),
                _ => unreachable!(),
            }
            let before = requests.lock().await.len();
            let (status, response) = trusted_post(&router, "/v1/messages", &body, &mcp.certificate).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{mode}: {response}");
            assert_eq!(requests.lock().await.len(), before, "{mode}");
            assert_eq!(call_count(&mcp).await, 0, "{mode}");
        }
    }
    task.abort();
    let _ = task.await;
    mcp.stop().await;
}

#[tokio::test]
async fn multiple_pending_calls_resume_in_order_with_failures_and_complete_client_results() {
    let mcp = fixture::HttpsMcp::start().await;
    let (url, requests, task) = fixture::inference().await;
    let router = build_router(
        common::test_state(&common::test_config(&url)),
        &ServerConfig::from_env(),
    );
    for stream in [false, true] {
        let mut body = request(&mcp.url, stream, "mixed");
        for name in ["client_echo", "client_other"] {
            body["tools"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":name,"input_schema":{"type":"object"}}));
        }
        body["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":[
                {"type":"mcp_tool_use","id":"call","server_name":"counter","name":"echo","input":{"text":"first"}},
                {"type":"tool_use","id":"client","name":"client_echo","input":{}},
                {"type":"mcp_tool_use","id":"failure","server_name":"counter","name":"fail","input":{"text":"second"}},
                {"type":"tool_use","id":"client2","name":"client_other","input":{}}
            ]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"client","content":"one"}]}),
        ]);
        let before = call_count(&mcp).await;
        let (status, response) = trusted_post(&router, "/v1/messages", &body, &mcp.certificate).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "partial client results: {response}");
        assert_eq!(call_count(&mcp).await, before);
        body["messages"][2]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"tool_result","tool_use_id":"client2","content":"two","is_error":true}));
        let (status, response) = trusted_post(&router, "/v1/messages", &body, &mcp.certificate).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(call_count(&mcp).await, before + 2);
        let content = response_content(&response, stream);
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "mcp_tool_result");
        assert_eq!(content[0]["tool_use_id"], "call");
        assert_eq!(content[0]["is_error"], false);
        assert_eq!(content[1]["type"], "mcp_tool_result");
        assert_eq!(content[1]["tool_use_id"], "failure");
        assert_eq!(content[1]["is_error"], true);
        let captured = requests.lock().await;
        let results = captured.last().unwrap()["messages"][2]["content"].as_array().unwrap();
        assert_eq!(
            results
                .iter()
                .map(|result| result["tool_use_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["client", "client2", "call", "failure"]
        );
        assert_eq!(results[1]["is_error"], true);
        assert_eq!(results[3]["is_error"], true);
    }
    task.abort();
    let _ = task.await;
    mcp.stop().await;
}
