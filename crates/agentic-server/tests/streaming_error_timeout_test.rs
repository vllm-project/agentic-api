//! Upstream error-body timeouts through public transports and durable recovery.
#[allow(dead_code)]
mod common;

use std::convert::Infallible;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentic_core::executor::ExecutionContext;
use axum::body::{Body, Bytes};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use http::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct BodyGuard(Arc<AtomicUsize>);
impl Drop for BodyGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

async fn wait_for_body_drop(dropped: &AtomicUsize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while dropped.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the upstream body must be released");
}

async fn spawn(app: Router) -> (String, Server) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        Server(tokio::spawn(async move { axum::serve(listener, app).await.unwrap() })),
    )
}

fn stalled_response(dropped: Arc<AtomicUsize>) -> Response {
    let guard = BodyGuard(dropped);
    let body = futures::stream::once(std::future::ready(Ok::<_, Infallible>(Bytes::from_static(
        b"partial diagnostic",
    ))))
    .chain(futures::stream::once(async move {
        let _guard = guard;
        std::future::pending::<Result<Bytes, Infallible>>().await
    }));
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("content-type", "text/plain")
        .header("retry-after", "7")
        .header("x-request-id", "req_stalled")
        .body(Body::from_stream(body))
        .unwrap()
}

fn successful_response() -> Value {
    json!({"id":"resp_upstream", "object":"response", "model":"test-model", "status":"completed",
        "output":[{"id":"msg_success", "type":"message", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":"recovered 雪"}]}]})
}

fn messages_request() -> Value {
    json!({"model":"test-model", "max_tokens":64, "stream":true,
        "messages":[{"role":"user", "content":"search"}],
        "tools":[{"name":"web_search", "input_schema":{"type":"object"}}]})
}

#[tokio::test]
async fn messages_streaming_error_timeout_preserves_initial_http_error() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let route_dropped = Arc::clone(&dropped);
    let requests = Arc::new(AtomicUsize::new(0));
    let route_requests = Arc::clone(&requests);
    let (upstream_url, _upstream) = spawn(Router::new().route(
        "/v1/messages",
        post(move || {
            route_requests.fetch_add(1, Ordering::SeqCst);
            let dropped = Arc::clone(&route_dropped);
            async move { stalled_response(dropped) }
        }),
    ))
    .await;
    let mut state = common::test_state(&common::test_config(&upstream_url));
    Arc::make_mut(&mut state.exec_ctx).streaming_timeout = Duration::from_millis(50);
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let _gateway = Server(gateway);
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
        .post(format!("{gateway_url}/v1/messages"))
        .json(&messages_request())
        .send()
        .await
        .expect("a stalled upstream error must not hang the Messages handler");
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "7");
    assert_eq!(response.headers()["x-request-id"], "req_stalled");
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert!(response.bytes().await.unwrap().is_empty());
    wait_for_body_drop(&dropped).await;
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[derive(Clone, Copy)]
enum Transport {
    Http,
    WebSocket,
}

#[allow(
    clippy::too_many_lines,
    reason = "keeps failure, retry, restart, and actual continuation in one fixture"
)]
async fn response_recovery(transport: Transport) {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let route_requests = Arc::clone(&requests);
    let dropped = Arc::new(AtomicUsize::new(0));
    let route_dropped = Arc::clone(&dropped);
    let (upstream_url, _upstream) = spawn(Router::new().route(
        "/v1/responses",
        post(move |Json(request): Json<Value>| {
            let requests = Arc::clone(&route_requests);
            let dropped = Arc::clone(&route_dropped);
            async move {
                let first = {
                    let mut requests = requests.lock().await;
                    requests.push(request);
                    requests.len() == 1
                };
                if first {
                    stalled_response(dropped)
                } else {
                    Json(successful_response()).into_response()
                }
            }
        }),
    ))
    .await;
    let directory = tempfile::tempdir().unwrap();
    let mut config = common::test_config(&upstream_url);
    config.db_url = Some(format!("sqlite://{}", directory.path().join("history.db").display()));
    let mut context = ExecutionContext::from_config(&config).await.unwrap();
    context.streaming_timeout = Duration::from_millis(50);
    let context = Arc::new(context);
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&context);
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let mut gateway = Server(gateway);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let request = json!({"model":"test-model", "input":"failed input", "store":true, "stream":true});
    let error = match transport {
        Transport::Http => {
            let response = client
                .post(format!("{gateway_url}/v1/responses"))
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response
                .text()
                .await
                .expect("a stalled error must terminate the Responses stream");
            assert!(body.ends_with("data: [DONE]\n\n"));
            let events: Vec<Value> = body
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter(|data| *data != "[DONE]")
                .map(|data| serde_json::from_str(data).unwrap())
                .collect();
            assert_eq!(events.len(), 1);
            events[0].clone()
        }
        Transport::WebSocket => {
            let (mut socket, _) = connect_async(format!("{}/v1/responses", gateway_url.replace("http:", "ws:")))
                .await
                .unwrap();
            let mut request = request;
            request["type"] = json!("response.create");
            request["stream_id"] = json!("stalled-request");
            socket.send(Message::Text(request.to_string().into())).await.unwrap();
            let event = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("a stalled error must terminate the WebSocket lane")
                .unwrap()
                .unwrap();
            let Message::Text(text) = event else {
                panic!("expected an error event");
            };
            let error: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(error["stream_id"], "stalled-request");
            socket.close(None).await.unwrap();
            error
        }
    };
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], 429);
    wait_for_body_drop(&dropped).await;
    assert_eq!(requests.lock().await.len(), 1, "no inference retry after an HTTP error");
    let pool = context.storage_pool().unwrap();
    for table in ["responses", "items"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "failed requests cannot persist {table}");
    }
    let success: Value = client
        .post(format!("{gateway_url}/v1/responses"))
        .json(&json!({"model":"test-model", "input":"successful input", "store":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(success["output"][0]["content"][0]["text"], "recovered 雪");
    gateway.0.abort();
    let _ = (&mut gateway.0).await;
    context.storage_pool().unwrap().close().await;
    drop(gateway);
    drop(context);
    let context = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&context);
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let _gateway = Server(gateway);
    let response: Value = client
        .post(format!("{gateway_url}/v1/responses"))
        .json(&json!({"model":"test-model", "input":"continue", "previous_response_id":success["id"], "store":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["status"], "completed");
    let requests = requests.lock().await;
    assert_eq!(requests.len(), 3);
    let downstream = requests[2]["input"].to_string();
    assert!(downstream.contains("successful input"));
    assert!(downstream.contains("recovered 雪"));
    assert!(downstream.contains("continue"));
    assert!(!downstream.contains("failed input"));
    assert!(!downstream.contains("partial diagnostic"));
    context.storage_pool().unwrap().close().await;
}

#[tokio::test]
async fn responses_http_streaming_error_timeout_allows_durable_recovery() {
    response_recovery(Transport::Http).await;
}

#[tokio::test]
async fn responses_websocket_streaming_error_timeout_allows_durable_recovery() {
    response_recovery(Transport::WebSocket).await;
}

#[tokio::test]
async fn messages_streaming_error_timeout_bounds_a_later_tool_round() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let route_requests = Arc::clone(&requests);
    let dropped = Arc::new(AtomicUsize::new(0));
    let route_dropped = Arc::clone(&dropped);
    let searches = Arc::new(AtomicUsize::new(0));
    let route_searches = Arc::clone(&searches);
    let events = [
        json!({"type":"message_start", "message":{"id":"msg_tool", "type":"message", "role":"assistant", "content":[],
            "model":"test-model", "stop_reason":null, "usage":{"input_tokens":2,"output_tokens":0}}}),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"call_search", "name":"web_search", "input":{}}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"{\"query\":\"weather\"}"}}),
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"message_delta", "delta":{"stop_reason":"tool_use", "stop_sequence":null}, "usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ];
    let mut body = String::new();
    for event in events {
        write!(body, "event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).unwrap();
    }
    let app = Router::new().route("/v1/messages", post(move |Json(request): Json<Value>| {
        let requests = Arc::clone(&route_requests);
        let dropped = Arc::clone(&route_dropped);
        let body = body.clone();
        async move {
            let first = { let mut requests = requests.lock().await; requests.push(request); requests.len() == 1 };
            if first { Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap() }
            else { stalled_response(dropped) }
        }
    })).route("/v1/search", axum::routing::get(move || {
        route_searches.fetch_add(1, Ordering::SeqCst);
        async {Json(json!({"results":{"web":[{"url":"https://example.com/weather", "title":"weather", "description":"sunny proof"}]}}))}
    }));
    let (upstream_url, _upstream) = spawn(app).await;
    let directory = tempfile::tempdir().unwrap();
    let mut config = common::test_config(&upstream_url);
    config.db_url = Some(format!("sqlite://{}", directory.path().join("history.db").display()));
    config.tools.web_search.api_key = Some("local-search-key".to_owned());
    config.tools.web_search.base_url = Some(upstream_url);
    let mut context = ExecutionContext::from_config(&config).await.unwrap();
    context.streaming_timeout = Duration::from_millis(50);
    let context = Arc::new(context);
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&context);
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let _gateway = Server(gateway);
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
        .post(format!("{gateway_url}/v1/messages"))
        .json(&messages_request())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .text()
        .await
        .expect("a later stalled upstream error must terminate the Messages stream");
    let events: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    assert_eq!(events.first().unwrap()["type"], "message_start");
    assert_eq!(events.last().unwrap()["type"], "error");
    assert_eq!(events.iter().filter(|event| event["type"] == "error").count(), 1);
    assert!(!events.iter().any(|event| event["type"] == "message_stop"));
    wait_for_body_drop(&dropped).await;
    assert_eq!(searches.load(Ordering::SeqCst), 1);
    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    let continuation = requests[1]["messages"].to_string();
    assert!(continuation.contains("call_search"));
    assert!(continuation.contains("sunny proof"));
    assert!(continuation.contains("tool_result"));
    context.storage_pool().unwrap().close().await;
}
