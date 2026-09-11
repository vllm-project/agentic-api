//! Terminal status through HTTP/WebSocket delivery and durable continuation.

#[allow(dead_code)]
mod common;

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentic_core::executor::ExecutionContext;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const ANSWER: &str = "Partial output 雪";

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn response_body(status: &str, tool: bool, empty: bool) -> Value {
    let output = if tool {
        vec![json!({"id":"fc_search", "type":"function_call", "status":"completed",
            "call_id":"call_search", "name":"web_search", "arguments":"{\"query\":\"weather\"}"})]
    } else if empty {
        Vec::new()
    } else {
        vec![
            json!({"id":"msg_answer", "type":"message", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":ANSWER}]}),
        ]
    };
    json!({"id":"resp_upstream", "object":"response", "model":"test-model", "status":status,
        "output":output, "usage":{"input_tokens":3,"output_tokens":5,"total_tokens":8},
        "incomplete_details":if status == "incomplete" {json!({"reason":"max_output_tokens"})} else {Value::Null}})
}

fn upstream_sse(response: &Value, terminal_type: &str) -> String {
    let mut events = vec![
        json!({"type":"response.created", "response":{"id":"resp_upstream", "status":"in_progress"}}),
        json!({"type":"response.in_progress", "response":{"id":"resp_upstream", "status":"in_progress"}}),
    ];
    for (index, item) in response["output"].as_array().unwrap().iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        if item["type"] == "message" {
            added["content"] = json!([]);
        }
        events.push(json!({"type":"response.output_item.added", "output_index":index, "item":added}));
        if item["type"] == "message" {
            events.push(json!({"type":"response.output_text.delta", "output_index":index,
                "item_id":item["id"], "content_index":0, "delta":ANSWER}));
        }
        events.push(json!({"type":"response.output_item.done", "output_index":index, "item":item}));
    }
    events.push(json!({"type":terminal_type, "response":response}));
    let mut body = String::new();
    for (sequence, mut event) in events.into_iter().enumerate() {
        event["sequence_number"] = json!(sequence);
        write!(body, "event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).unwrap();
    }
    body.push_str("data: [DONE]\n\n");
    body
}

#[derive(Clone, Copy)]
enum Transport {
    HttpJson,
    HttpStream,
    WebSocket,
}

async fn first_response(client: &reqwest::Client, url: &str, transport: Transport, tool: bool) -> (Value, Vec<Value>) {
    let mut request = json!({"model":"test-model", "input":"first turn", "store":true,
        "stream":!matches!(transport, Transport::HttpJson), "max_output_tokens":5});
    if tool {
        request["tools"] = json!([{"type":"web_search"}]);
    }
    let mut events = Vec::new();
    if matches!(transport, Transport::WebSocket) {
        let (mut socket, _) = connect_async(format!("{}/v1/responses", url.replace("http:", "ws:")))
            .await
            .unwrap();
        request["type"] = json!("response.create");
        request["stream_id"] = json!("terminal-status");
        socket.send(Message::Text(request.to_string().into())).await.unwrap();
        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
                .await
                .expect("WebSocket deadline")
                .expect("WebSocket event")
                .unwrap();
            let Message::Text(text) = message else { continue };
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(event["stream_id"], "terminal-status");
            let terminal = matches!(
                event["type"].as_str(),
                Some("response.completed" | "response.incomplete" | "response.failed" | "error")
            );
            events.push(event);
            if terminal {
                break;
            }
        }
        socket.close(None).await.unwrap();
    } else {
        let response = client
            .post(format!("{url}/v1/responses"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        if matches!(transport, Transport::HttpJson) {
            return (response.json().await.unwrap(), events);
        }
        let body = response.text().await.unwrap();
        events = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
    }
    (events.last().expect("terminal event")["response"].clone(), events)
}

#[allow(
    clippy::too_many_lines,
    reason = "keeps one fixture through shutdown, restart, and downstream continuation assertions"
)]
async fn check_delivery_and_restart(
    transport: Transport,
    terminal_type: &'static str,
    status: &'static str,
    tool: bool,
    empty: bool,
) {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let searches = Arc::new(AtomicUsize::new(0));
    let route_requests = Arc::clone(&requests);
    let route_searches = Arc::clone(&searches);
    let app = Router::new().route("/v1/responses", post(move |Json(request): Json<Value>| {
        let requests = Arc::clone(&route_requests);
        async move {
            let streaming = request["stream"] == true;
            let first = {
                let mut requests = requests.lock().await;
                requests.push(request);
                requests.len() == 1
            };
            // A mistaken extra inference round returns a completed response, making
            // status loss observable instead of hanging or exhausting the round cap.
            let response = if first {response_body(status, tool, empty)} else {response_body("completed", false, false)};
            if streaming {
                let event_type = if first {terminal_type} else {"response.completed"};
                Response::builder().header("content-type", "text/event-stream")
                    .body(Body::from(upstream_sse(&response, event_type))).unwrap()
            } else {
                Json(response).into_response()
            }
        }
    })).route("/v1/search", get(move || {
        route_searches.fetch_add(1, Ordering::SeqCst);
        async {Json(json!({"results":{"web":[{"url":"https://example.com/weather", "title":"weather proof", "description":"sunny"}]}}))}
    }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _upstream_guard = AbortOnDrop(upstream.abort_handle());
    let directory = tempfile::tempdir().unwrap();
    let mut config = common::test_config(&upstream_url);
    config.db_url = Some(format!("sqlite://{}", directory.path().join("history.db").display()));
    config.tools.web_search.api_key = Some("local-search-key".to_owned());
    config.tools.web_search.base_url = Some(upstream_url);
    let exec_ctx = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&exec_ctx);
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let _gateway_guard = AbortOnDrop(gateway.abort_handle());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let (response, events) = first_response(&client, &gateway_url, transport, tool).await;
    gateway.abort();
    let _ = gateway.await;
    exec_ctx.storage_pool().unwrap().close().await;
    drop(exec_ctx);

    assert_eq!(
        requests.lock().await.len(),
        1,
        "terminal incomplete must stop inference rounds"
    );
    assert_eq!(response["status"], status);
    assert_eq!(
        response["incomplete_details"],
        response_body(status, tool, empty)["incomplete_details"]
    );
    assert_eq!(response["usage"]["output_tokens"], 5);
    assert_eq!(response["usage"]["input_tokens"], 3);
    assert_eq!(response["usage"]["total_tokens"], 8);
    assert_eq!(searches.load(Ordering::SeqCst), usize::from(tool));
    if !events.is_empty() {
        assert_eq!(events.last().unwrap()["type"], format!("response.{status}"));
        for (sequence, event) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], sequence);
        }
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e["type"].as_str(), Some("response.completed" | "response.incomplete")))
                .count(),
            1
        );
    }
    if !tool && !empty {
        assert_eq!(response["output"][0]["content"][0]["text"], ANSWER);
    } else if empty {
        assert_eq!(response["output"], json!([]));
    } else {
        assert_eq!(response["output"][0]["type"], "web_search_call");
        assert_eq!(response["output"][0]["status"], "completed");
    }
    let response_id = response["id"].as_str().unwrap();
    assert_ne!(response_id, "resp_upstream");

    // Persistence stores item history and effective settings, not terminal status
    // or usage. Verify that accepted partial history really reaches later inference.
    let exec_ctx = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&exec_ctx);
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let _gateway_guard = AbortOnDrop(gateway.abort_handle());
    let followup = client
        .post(format!("{gateway_url}/v1/responses"))
        .json(
            &json!({"model":"test-model", "input":"next turn", "previous_response_id":response_id,
            "tools":[], "tool_choice":"none"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(followup.status(), http::StatusCode::OK);
    assert_eq!(
        followup.json::<Value>().await.unwrap()["output"][0]["content"][0]["text"],
        ANSWER
    );
    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["max_output_tokens"], 5);
    let input = requests[1]["input"].as_array().unwrap();
    assert_eq!(input.first().unwrap()["content"], "first turn");
    assert_eq!(input.last().unwrap()["content"], "next turn");
    if tool {
        assert_eq!(input.len(), 4);
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], input[2]["call_id"]);
        assert!(input[2]["output"].as_str().unwrap().contains("weather proof"));
    } else if !empty {
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["content"][0]["text"], ANSWER);
    } else {
        assert_eq!(input.len(), 2);
    }
    assert!(requests[1].get("previous_response_id").is_none());
    drop(requests);
    gateway.abort();
    upstream.abort();
    let _ = tokio::join!(gateway, upstream);
    exec_ctx.storage_pool().unwrap().close().await;
}

#[tokio::test]
async fn http_incomplete_completion_survives_restart() {
    check_delivery_and_restart(Transport::HttpStream, "response.completed", "incomplete", false, false).await;
}

#[tokio::test]
async fn websocket_incomplete_completion_survives_restart() {
    check_delivery_and_restart(Transport::WebSocket, "response.completed", "incomplete", false, false).await;
}

#[tokio::test]
async fn incomplete_completion_stops_tool_rounds_and_replays_results() {
    for transport in [Transport::HttpStream, Transport::WebSocket] {
        check_delivery_and_restart(transport, "response.completed", "incomplete", true, false).await;
    }
}

#[tokio::test]
async fn incomplete_empty_output_survives_restart() {
    check_delivery_and_restart(Transport::HttpStream, "response.done", "incomplete", false, true).await;
}

#[tokio::test]
async fn native_incomplete_and_json_controls() {
    for transport in [Transport::HttpStream, Transport::WebSocket, Transport::HttpJson] {
        check_delivery_and_restart(transport, "response.incomplete", "incomplete", false, false).await;
    }
    check_delivery_and_restart(Transport::HttpJson, "response.completed", "incomplete", true, false).await;
}

#[tokio::test]
async fn completion_at_token_limit_remains_completed() {
    for transport in [Transport::HttpStream, Transport::WebSocket, Transport::HttpJson] {
        check_delivery_and_restart(transport, "response.completed", "completed", false, false).await;
    }
}
