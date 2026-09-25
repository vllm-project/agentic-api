//! Execution and transport metrics through the production router, over HTTP
//! and WebSocket, against a scripted upstream.
//!
//! Each test owns its meter provider, so tests run in parallel. Hard process
//! shutdown is covered by `telemetry_metrics_e2e_test.rs`; here shutdown is
//! the gateway's drain signal.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::response::{IntoResponse as _, Response};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

use agentic_core::executor::{ConversationHandler, ExecutionContext, ResponseHandler};
use agentic_core::storage::{ConversationStore, ResponseStore, create_pool_with_schema};
use agentic_server::app::{AppState, ServerConfig, WebSocketTracker, build_router};
use agentic_server::telemetry::websocket::WebSocketMetrics;

#[allow(dead_code)]
mod common;
#[path = "../../agentic-server-core/tests/execution_metrics/harness.rs"]
mod harness;
use harness::{Metrics, Point, total};

static DATABASE: AtomicU64 = AtomicU64::new(0);

const USAGE: &str = r#"{"input_tokens":7,"input_tokens_details":{"cached_tokens":0},"output_tokens":2,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":9}"#;

fn sse_events(usage: &str) -> (String, String) {
    let message = r#"{"id":"msg_up","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hi","annotations":[]}]}"#;
    let head = r#"data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_up","status":"in_progress","output":[]}}

"#
    .to_owned();
    let tail = format!(
        "data: {{\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{{\"id\":\"msg_up\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}}}\n\n\
         data: {{\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"output_index\":0,\"content_index\":0,\"delta\":\"hi\"}}\n\n\
         data: {{\"type\":\"response.output_item.done\",\"sequence_number\":3,\"output_index\":0,\"item\":{message}}}\n\n\
         data: {{\"type\":\"response.completed\",\"sequence_number\":4,\"response\":{{\"id\":\"resp_up\",\"status\":\"completed\",\"output\":[{message}],\"usage\":{usage}}}}}\n\n\
         data: [DONE]\n\n"
    );
    (head, tail)
}

fn json_body() -> String {
    json!({
        "id": "resp_up", "object": "response", "created_at": 0, "model": "test-model", "status": "completed",
        "output": [{"id": "msg_up", "type": "message", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "hi", "annotations": []}]}],
        "usage": serde_json::from_str::<Value>(USAGE).unwrap()
    })
    .to_string()
}

/// An upstream scripted by the prompt: `FAIL` answers 502, `HANG` streams
/// `response.created` and then waits for `release`, anything else completes.
async fn upstream(release: Arc<Notify>) -> Server {
    let router = axum::Router::new().route(
        "/v1/responses",
        axum::routing::post(move |body: Bytes| {
            let release = Arc::clone(&release);
            async move { scripted(&body, release) }
        }),
    );
    Server::start(router).await
}

fn scripted(body: &[u8], release: Arc<Notify>) -> Response {
    let text = String::from_utf8_lossy(body);
    if text.contains("FAIL") {
        return (
            http::StatusCode::BAD_GATEWAY,
            r#"{"error":{"message":"upstream secret","type":"server_error"}}"#,
        )
            .into_response();
    }
    let stream = serde_json::from_slice::<Value>(body).unwrap()["stream"] == true;
    if !stream {
        return ([("content-type", "application/json")], json_body()).into_response();
    }
    let (head, tail) = sse_events(USAGE);
    let hang = text.contains("HANG");
    let rest = futures::stream::once(async move {
        if hang {
            release.notified().await;
        }
        Ok::<_, std::io::Error>(Bytes::from(tail))
    });
    let body = futures::stream::once(std::future::ready(Ok(Bytes::from(head)))).chain(rest);
    ([("content-type", "text/event-stream")], Body::from_stream(body)).into_response()
}

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(router: axum::Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, task }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn gateway_state(upstream: &str, metrics: &Metrics) -> AppState {
    let id = DATABASE.fetch_add(1, Ordering::Relaxed);
    let pool = create_pool_with_schema(Some(&format!("sqlite:file:metrics_{id}?mode=memory&cache=shared")))
        .await
        .unwrap();
    let mut state = common::test_state(&common::test_config(upstream));
    state.exec_ctx = Arc::new(
        ExecutionContext::new(
            ConversationHandler::new(ConversationStore::new(pool.clone())),
            ResponseHandler::new(ResponseStore::new(pool)),
            Arc::new(reqwest::Client::new()),
            upstream.to_owned(),
        )
        .with_metrics(metrics.executor()),
    );
    state.websocket_tracker = WebSocketTracker::with_metrics(WebSocketMetrics::new(&metrics.meter("agentic_server")));
    state
}

async fn gateway(state: AppState) -> Server {
    Server::start(build_router(state, &ServerConfig::from_env())).await
}

fn request(prompt: &str, stream: bool, store: bool) -> Value {
    json!({"model": "test-model", "input": prompt, "stream": stream, "store": store})
}

fn outcome(points: &[Point], outcome: &str) -> i64 {
    total(
        points,
        "agentic.execution.count",
        &[("agentic.execution.outcome", outcome)],
    )
}

fn delivery(points: &[Point], delivery: &str) -> i64 {
    total(
        points,
        "agentic.delivery.count",
        &[("agentic.delivery.outcome", delivery)],
    )
}

fn executions_total(points: &[Point]) -> i64 {
    total(points, "agentic.execution.count", &[])
}

#[tokio::test]
async fn http_terminal_paths_each_finalize_once() {
    let metrics = Metrics::new();
    let release = Arc::new(Notify::new());
    let upstream = upstream(Arc::clone(&release)).await;
    let gateway = gateway(gateway_state(&upstream.url, &metrics).await).await;
    let client = reqwest::Client::new();
    let url = format!("{}/v1/responses", gateway.url);

    let completed = client
        .post(&url)
        .json(&request("hello", true, true))
        .send()
        .await
        .unwrap();
    let body = completed.text().await.unwrap();
    assert!(body.contains("response.completed"), "{body}");

    let failed = client
        .post(&url)
        .json(&request("FAIL", true, true))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), 200, "the failure is delivered in-band");
    assert!(failed.text().await.unwrap().contains("event: error"));

    let blocking = client
        .post(&url)
        .json(&request("hello", false, true))
        .send()
        .await
        .unwrap();
    assert_eq!(blocking.status(), 200);
    blocking.bytes().await.unwrap();

    let proxied = client
        .post(&url)
        .json(&request("hello", false, false))
        .send()
        .await
        .unwrap();
    assert_eq!(proxied.status(), 200);
    proxied.bytes().await.unwrap();

    let mut hanging = client
        .post(&url)
        .json(&request("HANG", true, true))
        .send()
        .await
        .unwrap()
        .bytes_stream();
    let first = hanging.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("response.created"));
    // The client disconnects mid-stream.
    drop(hanging);

    let points = metrics
        .wait_for("five executions", |points| executions_total(points) >= 5)
        .await;
    assert_eq!(executions_total(&points), 5, "{points:#?}");
    assert_eq!(total(&points, "agentic.delivery.count", &[]), 5);
    assert_eq!(total(&points, "agentic.execution.active", &[]), 0);
    assert_eq!(outcome(&points, "completed"), 3);
    assert_eq!(outcome(&points, "failed"), 1);
    assert_eq!(outcome(&points, "cancelled"), 1);
    assert_eq!(delivery(&points, "delivered"), 4, "the in-band failure was delivered");
    assert_eq!(delivery(&points, "disconnected"), 1);
    assert_eq!(
        total(
            &points,
            "agentic.execution.count",
            &[("agentic.route", "proxy"), ("agentic.execution.outcome", "completed")]
        ),
        1
    );
    assert_eq!(
        total(
            &points,
            "agentic.execution.count",
            &[
                ("agentic.execution.outcome", "failed"),
                ("error.type", "upstream_status")
            ]
        ),
        1
    );
}

async fn socket(
    gateway: &Server,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("{}/v1/responses", gateway.url.replace("http:", "ws:"));
    tokio_tungstenite::connect_async(url).await.unwrap().0
}

fn create(prompt: &str) -> Message {
    let mut event = request(prompt, true, false);
    event["type"] = json!("response.create");
    Message::Text(event.to_string().into())
}

/// Read events until `count` of them have type `event_type`.
async fn read_until(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    event_type: &str,
    count: usize,
) {
    let mut seen = 0;
    tokio::time::timeout(Duration::from_secs(5), async {
        while seen < count {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                let event: Value = serde_json::from_str(&text).unwrap();
                seen += usize::from(event["type"] == event_type);
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("saw {seen} of {count} {event_type} events"));
}

#[tokio::test]
async fn websocket_requests_count_once_each_on_one_connection() {
    let metrics = Metrics::new();
    let upstream = upstream(Arc::new(Notify::new())).await;
    let gateway = gateway(gateway_state(&upstream.url, &metrics).await).await;
    let mut socket = socket(&gateway).await;

    for _ in 0..3 {
        socket.send(create("hello")).await.unwrap();
    }
    socket.send(create("FAIL")).await.unwrap();
    read_until(&mut socket, "response.completed", 3).await;
    read_until(&mut socket, "error", 1).await;

    let open = metrics
        .wait_for("four executions", |points| executions_total(points) >= 4)
        .await;
    assert_eq!(executions_total(&open), 4, "N requests, N executions");
    assert_eq!(outcome(&open, "completed"), 3);
    assert_eq!(outcome(&open, "failed"), 1);
    assert_eq!(
        total(&open, "agentic.websocket.connections.active", &[]),
        1,
        "one connection"
    );
    assert_eq!(total(&open, "agentic.websocket.queue.wait.duration", &[]), 4);
    assert_eq!(total(&open, "agentic.execution.active", &[]), 0);

    socket.close(None).await.unwrap();
    let closed = metrics
        .wait_for("closed connection", |points| {
            total(points, "agentic.websocket.connections.active", &[]) == 0
        })
        .await;
    assert_eq!(executions_total(&closed), 4, "closing counts nothing twice");
}

#[tokio::test]
async fn websocket_disconnect_cancels_active_and_queued_requests_once() {
    let metrics = Metrics::new();
    let upstream = upstream(Arc::new(Notify::new())).await;
    let gateway = gateway(gateway_state(&upstream.url, &metrics).await).await;
    let mut socket = socket(&gateway).await;

    socket.send(create("HANG")).await.unwrap();
    read_until(&mut socket, "response.created", 1).await;
    // Queued behind the hanging request on the default stream.
    socket.send(create("hello")).await.unwrap();
    socket.send(Message::Ping("barrier".into())).await.unwrap();
    while !matches!(socket.next().await.unwrap().unwrap(), Message::Pong(_)) {}
    drop(socket);

    let points = metrics
        .wait_for("two executions", |points| executions_total(points) >= 2)
        .await;
    assert_eq!(outcome(&points, "cancelled"), 2);
    assert_eq!(delivery(&points, "disconnected"), 2);
    assert_eq!(total(&points, "agentic.execution.active", &[]), 0);
    let closed = metrics
        .wait_for("closed connection", |points| {
            total(points, "agentic.websocket.connections.active", &[]) == 0
        })
        .await;
    assert_eq!(executions_total(&closed), 2);
}

#[tokio::test]
async fn websocket_drain_cancels_queued_requests_once_and_finishes_the_active_one() {
    let metrics = Metrics::new();
    let release = Arc::new(Notify::new());
    let upstream = upstream(Arc::clone(&release)).await;
    let state = gateway_state(&upstream.url, &metrics).await;
    let shutdown = state.shutdown_token.clone();
    let gateway = gateway(state).await;
    let mut socket = socket(&gateway).await;

    socket.send(create("HANG")).await.unwrap();
    read_until(&mut socket, "response.created", 1).await;
    socket.send(create("hello")).await.unwrap();
    socket.send(Message::Ping("barrier".into())).await.unwrap();
    while !matches!(socket.next().await.unwrap().unwrap(), Message::Pong(_)) {}

    // Shutdown discards queued work; the active request may still finish.
    shutdown.cancel();
    let points = metrics
        .wait_for("queued execution", |points| executions_total(points) >= 1)
        .await;
    assert_eq!(outcome(&points, "cancelled"), 1);
    assert_eq!(delivery(&points, "disconnected"), 1);
    assert_eq!(total(&points, "agentic.execution.active", &[]), 1, "one still running");

    release.notify_one();
    read_until(&mut socket, "response.completed", 1).await;
    let points = metrics
        .wait_for("both executions", |points| executions_total(points) >= 2)
        .await;
    assert_eq!(executions_total(&points), 2);
    assert_eq!(outcome(&points, "completed"), 1);
    assert_eq!(delivery(&points, "delivered"), 1);
    assert_eq!(total(&points, "agentic.execution.active", &[]), 0);
}
