//! Verify SSE data fields through public transports, persistence, and restart.

#[allow(dead_code)]
mod common;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use agentic_core::executor::ExecutionContext;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const ANSWER: &str = "雪 ☃: data: preserved";

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn response_body() -> Value {
    json!({
        "id":"resp_upstream", "object":"response", "model":"test-model", "status":"completed",
        "output":[{"id":"msg_answer", "type":"message", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":ANSWER}]}]
    })
}

fn upstream_sse(unspaced: bool) -> String {
    let events = [
        json!({"type":"response.created", "response":{"id":"resp_upstream", "status":"in_progress"}}),
        json!({"type":"response.output_item.added", "output_index":0,
            "item":{"id":"msg_answer", "type":"message", "role":"assistant", "status":"in_progress"}}),
        json!({"type":"response.output_text.delta", "item_id":"msg_answer", "output_index":0,
            "content_index":0, "delta":ANSWER}),
        json!({"type":"response.output_item.done", "output_index":0, "item":response_body()["output"][0]}),
        json!({"type":"response.completed", "response":response_body()}),
    ];
    let separator = if unspaced { "" } else { " " };
    let mut body = String::new();
    for event in events {
        write!(
            body,
            "event: {}\r\ndata:{separator}{event}\r\n\r\n",
            event["type"].as_str().unwrap()
        )
        .unwrap();
    }
    write!(body, "data:{separator}[DONE]\r\n\r\n").unwrap();
    body
}

async fn stream_events(client: &reqwest::Client, gateway_url: &str, websocket: bool) -> Vec<Value> {
    let request = json!({"model":"test-model", "input":"first turn", "store":true, "stream":true});
    let mut events = Vec::new();
    if websocket {
        let (mut socket, _) = connect_async(format!("{}/v1/responses", gateway_url.replace("http:", "ws:")))
            .await
            .unwrap();
        let mut request = request;
        request["type"] = json!("response.create");
        request["stream_id"] = json!("spacing");
        socket.send(Message::Text(request.to_string().into())).await.unwrap();
        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
                .await
                .expect("WebSocket response deadline")
                .expect("WebSocket event")
                .unwrap();
            let Message::Text(text) = message else { continue };
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(event["stream_id"], "spacing");
            let terminal = matches!(event["type"].as_str(), Some("response.completed" | "error"));
            events.push(event);
            if terminal {
                break;
            }
        }
        socket.close(None).await.unwrap();
    } else {
        let response = client
            .post(format!("{gateway_url}/v1/responses"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        let body = response.text().await.unwrap();
        events = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
    }
    events
}

async fn check_stream_and_restart(websocket: bool, unspaced: bool) {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let route_requests = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(request): Json<Value>| {
            let requests = Arc::clone(&route_requests);
            async move {
                let streaming = request["stream"] == true;
                requests.lock().await.push(request);
                if streaming {
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from(upstream_sse(unspaced)))
                        .unwrap()
                } else {
                    Json(response_body()).into_response()
                }
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _upstream_guard = AbortOnDrop(upstream.abort_handle());

    let directory = tempfile::tempdir().unwrap();
    let mut config = common::test_config(&upstream_url);
    config.db_url = Some(format!("sqlite://{}", directory.path().join("history.db").display()));
    let exec_ctx = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&exec_ctx);
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let _gateway_guard = AbortOnDrop(gateway.abort_handle());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let events = stream_events(&client, &gateway_url, websocket).await;
    gateway.abort();
    let _ = gateway.await;
    exec_ctx.storage_pool().unwrap().close().await;
    drop(exec_ctx);

    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.output_text.delta" && event["delta"] == ANSWER),
        "upstream text delta must reach the client: {events:?}"
    );
    let completed = events.last().expect("terminal event");
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["output"][0]["content"][0]["text"], ANSWER);
    let response_id = completed["response"]["id"].as_str().unwrap();
    assert_ne!(response_id, "resp_upstream");

    // A new executor and pool must reload the accepted turn from SQLite.
    let exec_ctx = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&exec_ctx);
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let _gateway_guard = AbortOnDrop(gateway.abort_handle());
    let followup = client
        .post(format!("{gateway_url}/v1/responses"))
        .json(&json!({"model":"test-model", "input":"next turn", "previous_response_id":response_id}))
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
    assert_eq!(requests[1]["input"][0]["content"], "first turn");
    assert_eq!(requests[1]["input"][1]["role"], "assistant");
    assert_eq!(requests[1]["input"][1]["content"][0]["text"], ANSWER);
    assert_eq!(requests[1]["input"][2]["content"], "next turn");
    assert_eq!(requests[1]["input"].as_array().unwrap().len(), 3);
    assert!(requests[1].get("previous_response_id").is_none());
    drop(requests);
    gateway.abort();
    upstream.abort();
    let _ = tokio::join!(gateway, upstream);
    exec_ctx.storage_pool().unwrap().close().await;
}

#[tokio::test]
async fn http_unspaced_sse_data_survives_persistence_and_restart() {
    check_stream_and_restart(false, true).await;
}

#[tokio::test]
async fn websocket_unspaced_sse_data_survives_persistence_and_restart() {
    check_stream_and_restart(true, true).await;
}

#[tokio::test]
async fn spaced_sse_data_preserves_http_and_websocket_behavior() {
    check_stream_and_restart(false, false).await;
    check_stream_and_restart(true, false).await;
}
