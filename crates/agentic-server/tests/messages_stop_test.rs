//! Messages completion must follow the protocol terminal, independently of HTTP EOF.
#[allow(dead_code)]
mod common;

use std::convert::Infallible;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentic_core::executor::{ExecutionContext, MessagesRequestContext, MessagesUpstream, run_messages_stream};
use agentic_core::tool::ToolRegistry;
use axum::body::{Body, Bytes};
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

struct Server(tokio::task::JoinHandle<()>);
impl Server {
    async fn stop(mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}
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

async fn spawn(app: Router) -> (String, Server) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        Server(tokio::spawn(async move { axum::serve(listener, app).await.unwrap() })),
    )
}

fn request() -> Value {
    json!({"model":"test-model", "max_tokens":64, "stream":true,
        "messages":[{"role":"user", "content":"hello"}],
        "tools":[{"name":"web_search", "input_schema":{"type":"object"}}]})
}

fn encode(events: &[Value]) -> String {
    let mut body = String::new();
    for event in events {
        write!(body, "event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).unwrap();
    }
    body
}

fn completed_message() -> Vec<Value> {
    vec![
        json!({"type":"message_start", "message":{"id":"msg_test", "type":"message", "role":"assistant", "model":"test-model", "content":[], "stop_reason":null, "usage":{"input_tokens":2,"output_tokens":0}}}),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":"Hello 雪"}}),
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"message_delta", "delta":{"stop_reason":"end_turn", "stop_sequence":null}, "usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ]
}

fn open_response(body: String, dropped: Arc<AtomicUsize>) -> Response {
    open_response_chunks(vec![Bytes::from(body)], dropped)
}

fn open_response_chunks(chunks: Vec<Bytes>, dropped: Arc<AtomicUsize>) -> Response {
    let guard = BodyGuard(dropped);
    let stream =
        futures::stream::iter(chunks.into_iter().map(Ok::<_, Infallible>)).chain(futures::stream::once(async move {
            let _guard = guard;
            std::future::pending::<Result<Bytes, Infallible>>().await
        }));
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn http_events(url: &str, request: &Value) -> Vec<Value> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .post(format!("{url}/v1/messages"))
        .json(request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    parse_events(&response.text().await.unwrap())
}

async fn http_open_body(body: String) -> Vec<Value> {
    http_open_body_for(body, &request()).await
}

async fn http_open_body_for(body: String, request: &Value) -> Vec<Value> {
    let (url, dropped, upstream) = open_upstream(body).await;
    let mut state = common::test_state(&common::test_config(&url));
    Arc::make_mut(&mut state.exec_ctx).streaming_timeout = Duration::from_millis(100);
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let gateway = Server(gateway);
    let events = http_events(&gateway_url, request).await;
    wait_for_drop(&dropped).await;
    gateway.stop().await;
    upstream.stop().await;
    events
}

#[tokio::test]
async fn messages_stop_keeps_mixed_client_and_gateway_calls_terminal() {
    let message = completed_message();
    let mut input = vec![message[0].clone()];
    for (index, name) in [(0, "web_search"), (1, "client_echo")] {
        let arguments = if name == "web_search" {
            r#"{"query":"Rust"}"#
        } else {
            "{}"
        };
        input.extend([
            json!({"type":"content_block_start", "index":index, "content_block":{"type":"tool_use", "id":format!("call_{name}"), "name":name, "input":{}}}),
            json!({"type":"content_block_delta", "index":index, "delta":{"type":"input_json_delta", "partial_json":arguments}}),
            json!({"type":"content_block_stop", "index":index}),
        ]);
    }
    let terminal = json!({"type":"message_delta", "delta":{"stop_reason":"tool_use", "stop_sequence":null}, "usage":{"output_tokens":4}});
    input.extend([terminal.clone(), message[5].clone()]);
    let mut request = request();
    request["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"client_echo", "input_schema":{"type":"object"}}));
    let mut expected = vec![message[0].clone()];
    for event in &input[4..7] {
        let mut event = event.clone();
        event["index"] = json!(0);
        expected.push(event);
    }
    expected.extend([terminal, message[5].clone()]);
    assert_eq!(http_open_body_for(encode(&input), &request).await, expected);
}

#[tokio::test]
async fn messages_stop_http_disconnect_before_terminal_releases_body() {
    let mut input = completed_message();
    input.pop();
    let (url, dropped, upstream) = open_upstream(encode(&input)).await;
    let mut state = common::test_state(&common::test_config(&url));
    Arc::make_mut(&mut state.exec_ctx).streaming_timeout = Duration::ZERO;
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let gateway = Server(gateway);
    let mut response = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
        .post(format!("{gateway_url}/v1/messages"))
        .json(&request())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let first = response.chunk().await.unwrap().unwrap();
    assert!(std::str::from_utf8(&first).unwrap().contains("message_start"));
    drop(response);
    wait_for_drop(&dropped).await;
    gateway.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn messages_stop_ignores_duplicate_terminals_and_trailing_errors() {
    let expected = completed_message();
    let mut input = expected.clone();
    input.push(json!({"type":"message_stop"}));
    input.push(json!({"type":"error", "error":{"type":"overloaded_error", "message":"after terminal"}}));
    input.extend(completed_message());
    assert_eq!(http_open_body(encode(&input)).await, expected);
}

#[tokio::test]
async fn messages_stop_completes_empty_and_token_limited_messages() {
    let message = completed_message();
    for reason in ["end_turn", "max_tokens", "pause_turn", "stop_sequence"] {
        let mut expected = vec![message[0].clone(), message[4].clone(), message[5].clone()];
        expected[1]["delta"]["stop_reason"] = json!(reason);
        assert_eq!(http_open_body(encode(&expected)).await, expected, "{reason}");
    }
}

#[tokio::test]
async fn messages_stop_requires_a_complete_matching_data_event() {
    let mut unfinished = completed_message();
    unfinished.pop();
    for suffix in [
        "",
        "event: message_stop\n\n",
        "event: message_stop\ndata: {invalid}\n\n",
        "data: {\"type\":\"message_stopped\"}\n\n",
        "data: {}\n\ndata: null\n\ndata:\n\n",
        "data: {\"type\":\"ping\"}\n\n",
    ] {
        let events = http_open_body(format!("{}{suffix}", encode(&unfinished))).await;
        assert_eq!(events.last().unwrap()["type"], "error", "{suffix}");
        assert!(
            events.last().unwrap()["error"]["message"]
                .as_str()
                .unwrap()
                .contains("chunk timeout")
        );
        assert!(!events.iter().any(|event| event["type"] == "message_stop"));
    }
}

#[tokio::test]
async fn messages_stop_preserves_errors_before_the_terminal() {
    let mut input = completed_message();
    let error = json!({"type":"error", "error":{"type":"overloaded_error", "message":"before terminal"}});
    input.insert(input.len() - 1, error.clone());
    let events = http_open_body(encode(&input)).await;
    assert_eq!(events.last().unwrap(), &error);
    assert_eq!(events.iter().filter(|event| event["type"] == "error").count(), 1);
    assert!(!events.iter().any(|event| event["type"] == "message_stop"));
}

#[tokio::test]
async fn messages_stop_preserves_clean_eof_compatibility() {
    let expected = completed_message();
    let mut input = expected.clone();
    input.pop();
    let body = encode(&input);
    let (url, upstream) = spawn(Router::new().route(
        "/v1/messages",
        post(move || {
            let body = body.clone();
            async move {
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(body))
                    .unwrap()
            }
        }),
    ))
    .await;
    let (gateway_url, gateway) = common::spawn_gateway(common::test_state(&common::test_config(&url))).await;
    let gateway = Server(gateway);
    assert_eq!(http_events(&gateway_url, &request()).await, expected);
    gateway.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn messages_stop_accepts_fragmented_utf8_and_terminal_with_optional_data_space() {
    let expected = completed_message();
    let body = encode(&expected).replace("data: ", "data:").replace('\n', "\r\n");
    let dropped = Arc::new(AtomicUsize::new(0));
    let route_dropped = Arc::clone(&dropped);
    let (url, upstream) = spawn(Router::new().route(
        "/v1/messages",
        post(move || {
            let chunks = body
                .as_bytes()
                .iter()
                .map(|byte| Bytes::copy_from_slice(&[*byte]))
                .collect();
            let dropped = Arc::clone(&route_dropped);
            async move { open_response_chunks(chunks, dropped) }
        }),
    ))
    .await;
    let mut state = common::test_state(&common::test_config(&url));
    Arc::make_mut(&mut state.exec_ctx).streaming_timeout = Duration::ZERO;
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let gateway = Server(gateway);
    assert_eq!(http_events(&gateway_url, &request()).await, expected);
    wait_for_drop(&dropped).await;
    gateway.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn messages_stop_cancellation_before_terminal_releases_body() {
    let mut input = completed_message();
    input.pop();
    let (url, dropped, upstream) = open_upstream(encode(&input)).await;
    let mut state = common::test_state(&common::test_config(&url));
    Arc::make_mut(&mut state.exec_ctx).streaming_timeout = Duration::ZERO;
    let context = MessagesRequestContext::from_value(request()).unwrap();
    let upstream_request = MessagesUpstream::new(&url, None, reqwest::header::HeaderMap::new());
    let mut response = run_messages_stream(
        context,
        Arc::new(ToolRegistry::default()),
        state.exec_ctx,
        upstream_request,
    )
    .await
    .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), response.body.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parse_events(&first)[0]["type"], "message_start");
    drop(response);
    wait_for_drop(&dropped).await;
    upstream.stop().await;
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "keeps recorded rounds, upstream cleanup, real search and actual replay input together"
)]
async fn messages_stop_advances_recorded_tool_rounds_and_keeps_requests_stateless() {
    let cassette: Value = serde_yml::from_str(include_str!(
        "../../agentic-server-core/tests/cassettes/messages/messages-web-search-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml"
    ))
    .unwrap();
    let streams: Vec<String> = cassette["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| {
            turn["response"]["sse"]
                .as_array()
                .unwrap()
                .iter()
                .map(|line| line.as_str().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(streams.len(), 2);
    for body in &streams {
        assert_eq!(parse_events(body).last().unwrap()["type"], "message_stop");
        assert!(!body.contains("[DONE]"));
    }
    let first_expected = parse_events(&streams[0]);
    let final_expected = parse_events(&streams[1]);
    let mut expected_assistant = cassette["turns"][1]["request"]["body"]["messages"][1].clone();
    // The recorder's replay request omitted the streamed thinking signature.
    // The gateway must preserve it along with the recorded content.
    let signature: String = first_expected
        .iter()
        .filter_map(|event| event["delta"]["signature"].as_str())
        .collect();
    assert!(!signature.is_empty());
    expected_assistant["content"][0]["signature"] = json!(signature);
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let route_requests = Arc::clone(&requests);
    let dropped = Arc::new(AtomicUsize::new(0));
    let route_dropped = Arc::clone(&dropped);
    let search_dropped = Arc::clone(&dropped);
    let searches = Arc::new(AtomicUsize::new(0));
    let route_searches = Arc::clone(&searches);
    let app = Router::new().route("/v1/messages", post(move |Json(request): Json<Value>| {
        let requests = Arc::clone(&route_requests);
        let dropped = Arc::clone(&route_dropped);
        let streams = streams.clone();
        async move {
            let index = { let mut requests = requests.lock().await; let index = requests.len(); requests.push(request); index };
            let body = streams.get(index).cloned().unwrap_or_else(|| encode(&completed_message()));
            open_response(body, dropped)
        }
    })).route("/v1/search", axum::routing::get(move || {
        let dropped = Arc::clone(&search_dropped);
        route_searches.fetch_add(1, Ordering::SeqCst);
        async move {
            // If the old response stream is still owned during dispatch, this
            // cannot complete: the search is waiting for that body to close.
            wait_for_drop(&dropped).await;
            Json(json!({"results":{"web":[{"url":"https://www.rust-lang.org/", "title":"Rust", "description":"cleanup proof 雪"}]}}))
        }
    }));
    let (url, upstream) = spawn(app).await;
    let directory = tempfile::tempdir().unwrap();
    let mut config = common::test_config(&url);
    config.db_url = Some(format!("sqlite://{}", directory.path().join("history.db").display()));
    config.tools.web_search.api_key = Some("local-search-key".to_owned());
    config.tools.web_search.base_url = Some(url);
    let mut context = ExecutionContext::from_config(&config).await.unwrap();
    context.streaming_timeout = Duration::from_millis(100);
    let context = Arc::new(context);
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&context);
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let gateway = Server(gateway);
    let input = cassette["turns"][0]["request"]["body"].clone();
    let events = http_events(&gateway_url, &input).await;
    assert_eq!(events.last().unwrap()["type"], "message_stop", "{events:?}");
    for kind in ["message_start", "message_delta", "message_stop"] {
        assert_eq!(events.iter().filter(|event| event["type"] == kind).count(), 1, "{kind}");
    }
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "error" || event["content_block"]["type"] == "tool_use")
    );
    assert_eq!(events.first().unwrap(), first_expected.first().unwrap());
    assert_eq!(events[events.len() - 2], final_expected[final_expected.len() - 2]);
    let indexes: Vec<u64> = events
        .iter()
        .filter(|event| event["type"] == "content_block_start")
        .map(|event| event["index"].as_u64().unwrap())
        .collect();
    assert_eq!(indexes, (0..u64::try_from(indexes.len()).unwrap()).collect::<Vec<_>>());
    let expected_text: String = first_expected
        .iter()
        .chain(&final_expected)
        .filter_map(|event| event["delta"]["text"].as_str())
        .collect();
    let actual_text: String = events
        .iter()
        .filter_map(|event| event["delta"]["text"].as_str())
        .collect();
    assert!(!expected_text.is_empty());
    assert_eq!(actual_text, expected_text);
    {
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], input);
        let history = requests[1]["messages"].as_array().unwrap();
        let output = &history.last().unwrap()["content"][0];
        assert_eq!(output["type"], "tool_result");
        assert_ne!(output["is_error"], true, "{output}");
        assert!(output["content"].to_string().contains("cleanup proof 雪"));
        let assistant = &history[history.len() - 2];
        assert_eq!(assistant, &expected_assistant);
        let call = assistant["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "tool_use")
            .unwrap();
        assert_eq!(call["id"], output["tool_use_id"]);
        assert_eq!(call["input"]["query"], "latest stable Rust release");
    }
    assert_eq!(searches.load(Ordering::SeqCst), 1);
    // A new HTTP request supplies its own history; the completed tool loop
    // must not leave replay state or a duplicate search in the next request.
    assert_eq!(http_events(&gateway_url, &request()).await, completed_message());
    assert_eq!(requests.lock().await[2], request());
    assert_eq!(searches.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(2), async {
        while dropped.load(Ordering::SeqCst) != 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    for table in ["responses", "items", "conversations"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(context.storage_pool().unwrap())
            .await
            .unwrap();
        assert_eq!(count, 0, "Messages must not persist {table}");
    }
    gateway.stop().await;
    upstream.stop().await;
    context.storage_pool().unwrap().close().await;
}

async fn open_upstream(body: String) -> (String, Arc<AtomicUsize>, Server) {
    let dropped = Arc::new(AtomicUsize::new(0));
    let route_dropped = Arc::clone(&dropped);
    let (url, server) = spawn(Router::new().route(
        "/v1/messages",
        post(move || {
            let body = body.clone();
            let dropped = Arc::clone(&route_dropped);
            async move { open_response(body, dropped) }
        }),
    ))
    .await;
    (url, dropped, server)
}

fn parse_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

async fn wait_for_drop(dropped: &AtomicUsize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while dropped.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the completed round must release its upstream body");
}

#[tokio::test]
async fn messages_stop_completes_core_stream_with_timeout_disabled() {
    let expected = completed_message();
    let (url, dropped, upstream_server) = open_upstream(encode(&expected)).await;
    let mut state = common::test_state(&common::test_config(&url));
    Arc::make_mut(&mut state.exec_ctx).streaming_timeout = Duration::ZERO;
    let context = MessagesRequestContext::from_value(request()).unwrap();
    let upstream = MessagesUpstream::new(&url, None, reqwest::header::HeaderMap::new());
    let response = run_messages_stream(context, Arc::new(ToolRegistry::default()), state.exec_ctx, upstream)
        .await
        .unwrap();
    let body = tokio::time::timeout(Duration::from_secs(2), response.body.collect::<Vec<_>>())
        .await
        .expect("message_stop must complete without waiting for HTTP EOF or an idle timeout")
        .join("");
    assert_eq!(parse_events(&body), expected);
    wait_for_drop(&dropped).await;
    upstream_server.stop().await;
}

#[tokio::test]
async fn messages_stop_completes_http_stream_before_idle_timeout() {
    let expected = completed_message();
    let (url, dropped, upstream) = open_upstream(encode(&expected)).await;
    let mut state = common::test_state(&common::test_config(&url));
    Arc::make_mut(&mut state.exec_ctx).streaming_timeout = Duration::from_millis(50);
    let (gateway_url, gateway) = common::spawn_gateway(state).await;
    let gateway = Server(gateway);
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
        .post(format!("{gateway_url}/v1/messages"))
        .json(&request())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let body = response.text().await.unwrap();
    assert_eq!(
        parse_events(&body),
        expected,
        "a completed message must not become an idle-timeout error"
    );
    wait_for_drop(&dropped).await;
    gateway.stop().await;
    upstream.stop().await;
}
