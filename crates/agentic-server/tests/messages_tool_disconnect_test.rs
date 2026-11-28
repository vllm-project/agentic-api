//! Actual HTTP qualification for disconnects during inference and built-in tools (#110).
#[allow(dead_code)]
mod common;

use std::convert::Infallible;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentic_core::executor::ExecutionContext;
use axum::body::{Body, Bytes};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Notify;

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl Server {
    async fn stop(mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}

struct PendingBodyGuard(Arc<Notify>);
impl Drop for PendingBodyGuard {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// The tool has accepted the request and emitted a partial JSON body, but cannot
/// finish. Dropping the client must cancel that outbound request, not wait for its
/// timeout, replay the tool, or start another inference round.
#[tokio::test]
async fn messages_http_disconnect_during_search_cancels_outbound_body() {
    assert_disconnect(DisconnectPhase::Search).await;
}

#[tokio::test]
async fn messages_http_disconnect_during_inference_cancels_outbound_body() {
    assert_disconnect(DisconnectPhase::Inference).await;
}

#[tokio::test]
async fn messages_http_disconnect_after_search_cancels_continuation_without_replay() {
    assert_disconnect(DisconnectPhase::Continuation).await;
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DisconnectPhase {
    Inference,
    Search,
    Continuation,
}

async fn assert_disconnect(phase: DisconnectPhase) {
    let inference_calls = Arc::new(AtomicUsize::new(0));
    let search_calls = Arc::new(AtomicUsize::new(0));
    let outbound_started = Arc::new(Notify::new());
    let outbound_dropped = Arc::new(Notify::new());
    let app = upstream_routes(
        &inference_calls,
        &search_calls,
        &outbound_started,
        &outbound_dropped,
        phase,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = Server(tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }));
    let directory = tempfile::tempdir().unwrap();
    let mut config = common::test_config(&upstream_url);
    config.db_url = Some(format!("sqlite://{}", directory.path().join("history.db").display()));
    config.tools.web_search.api_key = Some("local-test-key".to_owned());
    config.tools.web_search.base_url = Some(upstream_url);
    let mut context = ExecutionContext::from_config(&config).await.unwrap();
    context.streaming_timeout = Duration::ZERO;
    let context = Arc::new(context);
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&context);
    let (url, gateway) = common::spawn_gateway(state).await;
    let gateway = Server(gateway);
    let client = reqwest::Client::new();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client
            .post(format!("{url}/v1/messages"))
            .json(&json!({"model":"test", "max_tokens":64, "stream":true,
            "messages":[{"role":"user", "content":"Search"}],
            "tools":[{"name":"web_search", "input_schema":{"type":"object"}}]}))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(5), outbound_started.notified())
        .await
        .expect("outbound request started");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), outbound_dropped.notified())
            .await
            .is_err(),
        "outbound request must remain pending while the client stays connected"
    );
    // This is a client HTTP disconnect, not a direct drop of a core executor stream.
    drop(response);
    tokio::time::timeout(Duration::from_secs(5), outbound_dropped.notified())
        .await
        .expect("outbound body cancelled");
    assert_eq!(
        search_calls.load(Ordering::SeqCst),
        usize::from(phase != DisconnectPhase::Inference),
        "no premature tool dispatch or replay"
    );
    assert_eq!(
        inference_calls.load(Ordering::SeqCst),
        if phase == DisconnectPhase::Continuation { 2 } else { 1 },
        "no continuation after disconnect"
    );
    for table in ["responses", "items", "conversations"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(context.storage_pool().unwrap())
            .await
            .unwrap();
        assert_eq!(count, 0, "cancelled Messages turn must not persist {table}");
    }
    gateway.stop().await;
    upstream.stop().await;
    context.storage_pool().unwrap().close().await;
}

fn upstream_routes(
    inference_calls: &Arc<AtomicUsize>,
    search_calls: &Arc<AtomicUsize>,
    outbound_started: &Arc<Notify>,
    outbound_dropped: &Arc<Notify>,
    phase: DisconnectPhase,
) -> Router {
    let route_inference = Arc::clone(inference_calls);
    let route_search = Arc::clone(search_calls);
    let started = Arc::clone(outbound_started);
    let dropped = Arc::clone(outbound_dropped);
    let inference_started = Arc::clone(outbound_started);
    let inference_dropped = Arc::clone(outbound_dropped);
    Router::new()
        .route("/v1/messages", post(move |Json(_): Json<Value>| {
            let round = route_inference.fetch_add(1, Ordering::SeqCst);
            let during_inference = phase == DisconnectPhase::Inference
                || (phase == DisconnectPhase::Continuation && round > 0);
            let started = Arc::clone(&inference_started);
            let dropped = Arc::clone(&inference_dropped);
            async move {
                let mut events = [
                    json!({"type":"message_start", "message":{"id":"m", "type":"message", "role":"assistant",
                        "content":[], "model":"test", "usage":{"input_tokens":1,"output_tokens":0}}}),
                    json!({"type":"content_block_start", "index":0, "content_block":{
                        "type":"tool_use", "id":"search_1", "name":"web_search", "input":{}}}),
                    json!({"type":"content_block_delta", "index":0, "delta":{
                        "type":"input_json_delta", "partial_json":"{\"query\":\"Rust\"}"}}),
                    json!({"type":"content_block_stop", "index":0}),
                    json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}, "usage":{"output_tokens":4}}),
                    json!({"type":"message_stop"}),
                ];
                if round > 0 {
                    events[1] = json!({"type":"content_block_start", "index":0,
                        "content_block":{"type":"text", "text":""}});
                    events[2] = json!({"type":"content_block_delta", "index":0,
                        "delta":{"type":"text_delta", "text":"Partial answer"}});
                }
                let mut body = String::new();
                for event in events.iter().take(if during_inference { 3 } else { events.len() }) {
                    write!(body, "data: {event}\n\n").unwrap();
                }
                let body = if during_inference {
                    started.notify_one();
                    let guard = PendingBodyGuard(dropped);
                    Body::from_stream(futures::stream::once(async move { Ok::<_, Infallible>(Bytes::from(body)) })
                        .chain(futures::stream::once(async move {
                            let _guard = guard;
                            std::future::pending::<Result<Bytes, Infallible>>().await
                        })))
                } else {
                    Body::from(body)
                };
                Response::builder().header("content-type", "text/event-stream").body(body).unwrap()
            }
        }))
        .route("/v1/search", get(move || {
            route_search.fetch_add(1, Ordering::SeqCst);
            let guard = (phase == DisconnectPhase::Search).then(|| PendingBodyGuard(Arc::clone(&dropped)));
            if guard.is_some() {
                started.notify_one();
            }
            async move {
                if phase == DisconnectPhase::Continuation {
                    return Response::builder().header("content-type", "application/json")
                        .body(Body::from(r#"{"results":{"web":[],"news":[]}}"#)).unwrap();
                }
                let body = futures::stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"{\"results\":")) })
                    .chain(futures::stream::once(async move {
                        let _guard = guard;
                        std::future::pending::<Result<Bytes, Infallible>>().await
                    }));
                Response::builder().header("content-type", "application/json").body(Body::from_stream(body)).unwrap()
            }
        }))
}
