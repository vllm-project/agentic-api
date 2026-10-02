//! Replays the recorded `web_fetch` Messages cassettes through the gateway
//! loops (#408).
//!
//! Each cassette holds real vLLM `/v1/messages` upstream turns for a request
//! that declared the native `web_fetch_20250910` tool, as the gateway rewrites
//! it: in turn 1 the model calls `web_fetch` with the page the user named; in
//! turn 2, after the gateway's `tool_result`, it answers with the verification
//! token the page carries. The replay serves the recorded responses from a mock
//! upstream, serves the page from a loopback origin, and runs the real loop:
//! the real fetch, the real extraction, the real continuation. It then checks
//! the loop's complete upstream requests against the recorded ones, the
//! call/result pairing, and that the client sees the answer and never the call.
//!
//! The recording named its origin `127.0.0.1:18080`; the replay binds an
//! ephemeral port, so the origin is re-based in both directions: the served
//! responses name the live origin (in the streaming recording the URL spans
//! several `input_json_delta` chunks, so a block's input deltas are re-emitted
//! as one), and the loop's requests are compared with the live origin mapped
//! back. `retrieved_at`, a timestamp by contract, is the only other field
//! normalized.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agentic_core::config::{DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS, WebFetchConfig};
use agentic_core::executor::{
    ConversationHandler, ExecutionContext, MessagesRequestContext, MessagesUpstream, ResponseHandler,
    run_messages_loop, run_messages_stream,
};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::registry_tools;
use agentic_core::tool::{GatewayExecutorRegistration, ToolRegistry, WebFetchHandler};
use agentic_core::types::messages::GatewayToolMap;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;

const CASSETTE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/messages");
/// The origin the recording named; the replay's origin stands in for it.
const RECORDED_ORIGIN: &str = "http://127.0.0.1:18080";
/// The page behind the recorded URL; its text is what the recording fed back.
const PAGE: &str = "<html><head><title>Release notes</title></head><body><h1>Agentic API 0.9.1</h1>\
                    <p>Verification token: AGENTIC-WEBFETCH-7F3A9C</p></body></html>";
const PAGE_TEXT: &str = "Agentic API 0.9.1\nVerification token: AGENTIC-WEBFETCH-7F3A9C";
const TOKEN: &str = "AGENTIC-WEBFETCH-7F3A9C";

/// One recorded upstream turn: the request the recorder sent and the response
/// vLLM gave, as JSON or as the raw event stream.
struct Turn {
    request: Value,
    response: Recorded,
}

enum Recorded {
    Json(Value),
    Sse(String),
}

fn load(cassette: &str) -> Vec<Turn> {
    let path = format!("{CASSETTE_DIR}/{cassette}");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let doc: Value = serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"));
    doc["turns"]
        .as_array()
        .expect("turns")
        .iter()
        .map(|turn| {
            let response = match turn["response"].get("sse").and_then(Value::as_array) {
                Some(lines) => Recorded::Sse(lines.iter().filter_map(Value::as_str).collect()),
                None => Recorded::Json(turn["response"]["body"].clone()),
            };
            Turn {
                request: turn["request"]["body"].clone(),
                response,
            }
        })
        .collect()
}

/// A recorded JSON response with the origin re-based onto the replay's.
fn rebase_json(body: &Value, live_origin: &str) -> Value {
    let text = serde_json::to_string(body)
        .expect("recorded body")
        .replace(RECORDED_ORIGIN, live_origin);
    serde_json::from_str(&text).expect("re-based body")
}

/// A recorded event stream with the origin re-based onto the replay's. The
/// `tool_use` input arrives in `input_json_delta` chunks that may split the
/// URL, so a block's input deltas are collected and re-emitted as one delta
/// before its `content_block_stop`; every other event is served as recorded.
fn rebase_sse(stream: &str, live_origin: &str) -> String {
    let mut out = String::new();
    let mut inputs: BTreeMap<u64, String> = BTreeMap::new();
    for frame in stream.split("\n\n").filter(|frame| !frame.trim().is_empty()) {
        let event = frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .and_then(|data| serde_json::from_str::<Value>(data).ok());
        let Some(event) = event else {
            out.push_str(frame);
            out.push_str("\n\n");
            continue;
        };
        let index = event["index"].as_u64().unwrap_or_default();
        if event["type"] == "content_block_delta" && event["delta"]["type"] == "input_json_delta" {
            inputs
                .entry(index)
                .or_default()
                .push_str(event["delta"]["partial_json"].as_str().unwrap_or_default());
            continue;
        }
        if event["type"] == "content_block_stop" {
            if let Some(input) = inputs.remove(&index) {
                let delta = json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": input.replace(RECORDED_ORIGIN, live_origin)}
                });
                out.push_str("event: content_block_delta\ndata: ");
                out.push_str(&delta.to_string());
                out.push_str("\n\n");
            }
        }
        out.push_str(frame);
        out.push_str("\n\n");
    }
    out
}

#[derive(Clone)]
struct Backend {
    turns: Arc<Vec<Arc<Turn>>>,
    /// The replay origin the recorded responses are re-based onto.
    origin: String,
    requests: Arc<Mutex<Vec<Value>>>,
    pages: Arc<Mutex<Vec<String>>>,
}

async fn infer(State(backend): State<Backend>, Json(request): Json<Value>) -> axum::response::Response {
    let round = {
        let mut requests = backend.requests.lock().unwrap();
        requests.push(request);
        requests.len() - 1
    };
    match backend.turns.get(round).map(|turn| &turn.response) {
        Some(Recorded::Json(body)) => Json(rebase_json(body, &backend.origin)).into_response(),
        Some(Recorded::Sse(stream)) => axum::response::Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from(rebase_sse(stream, &backend.origin)))
            .unwrap(),
        None => (StatusCode::BAD_GATEWAY, "cassette exhausted").into_response(),
    }
}

async fn page(State(backend): State<Backend>) -> axum::response::Response {
    backend.pages.lock().unwrap().push("release-notes.html".to_owned());
    ([("content-type", "text/html; charset=utf-8")], PAGE).into_response()
}

struct Harness {
    backend: Backend,
    origin: String,
    exec_ctx: ExecutionContext,
    turns: Vec<Arc<Turn>>,
}

async fn harness(cassette: &str) -> Harness {
    let turns: Vec<Arc<Turn>> = load(cassette).into_iter().map(Arc::new).collect();
    assert_eq!(turns.len(), 2, "{cassette}: one fetch round, then the answer");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let backend = Backend {
        turns: Arc::new(turns.clone()),
        origin: origin.clone(),
        requests: Arc::new(Mutex::new(Vec::new())),
        pages: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/v1/messages", post(infer))
        .route("/page/release-notes.html", get(page))
        .with_state(backend.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let exec_ctx = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        origin.clone(),
    )
    .with_gateway_executor(GatewayExecutorRegistration::WebFetch(Arc::new(
        WebFetchHandler::from_config(
            &WebFetchConfig::default().with_allow_private_networks(true),
            DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS,
        ),
    )));
    Harness {
        backend,
        origin,
        exec_ctx,
        turns,
    }
}

impl Harness {
    /// The client request the recording answers: the recorded user prompt with
    /// the replay's origin, and the native declaration the gateway rewrites.
    fn client_request(&self, stream: bool) -> Value {
        let recorded = &self.turns[0].request;
        let prompt = recorded["messages"][0]["content"]
            .as_str()
            .expect("recorded prompt")
            .replace(RECORDED_ORIGIN, &self.origin);
        json!({
            "model": recorded["model"],
            "max_tokens": recorded["max_tokens"],
            "stream": stream,
            "messages": [{"role": "user", "content": prompt}],
            "tools": [{"type": "web_fetch_20250910", "name": "web_fetch"}]
        })
    }

    fn upstream_requests(&self) -> Vec<Value> {
        self.backend.requests.lock().unwrap().clone()
    }

    /// Every upstream request the loop sent equals the recorded one, after the
    /// two recording-time artifacts are normalized.
    fn assert_upstream_requests_match_the_recording(&self) {
        let requests = self.upstream_requests();
        assert_eq!(
            requests.len(),
            self.turns.len(),
            "one upstream request per recorded turn"
        );
        for (round, (sent, turn)) in requests.iter().zip(&self.turns).enumerate() {
            assert_eq!(
                normalized(sent, &self.origin),
                normalized(&turn.request, &self.origin),
                "upstream request {round} differs from the recording"
            );
        }
    }

    /// The fed-back `tool_result` pairs with the recorded `tool_use` and
    /// carries the page's text; the page was fetched exactly once.
    fn assert_call_and_result_pair(&self) {
        let recorded_call = self.recorded_tool_use();
        let requests = self.upstream_requests();
        let messages = requests[1]["messages"].as_array().expect("messages");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["id"], recorded_call["id"]);
        let fed_back = &messages[2]["content"][0];
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(fed_back["type"], "tool_result");
        assert_eq!(fed_back["tool_use_id"], recorded_call["id"]);
        assert_eq!(fed_back["is_error"], false);
        let result: Value = serde_json::from_str(fed_back["content"].as_str().expect("string content")).unwrap();
        assert_eq!(result["type"], "web_fetch_result");
        assert_eq!(result["title"], "Release notes");
        assert_eq!(result["content_type"], "text/html");
        assert_eq!(result["content"], PAGE_TEXT);
        assert_eq!(result["truncated"], false);
        assert_eq!(*self.backend.pages.lock().unwrap(), vec!["release-notes.html"]);
    }

    fn recorded_tool_use(&self) -> Value {
        match &self.turns[0].response {
            Recorded::Json(body) => body["content"][0].clone(),
            Recorded::Sse(stream) => stream
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                .find(|event| event["type"] == "content_block_start" && event["content_block"]["type"] == "tool_use")
                .map(|event| event["content_block"].clone())
                .expect("recorded tool_use block"),
        }
    }
}

/// Map the replay origin back to the recorded one in every string, and read
/// each `tool_result` content as JSON with its `retrieved_at` fixed, so the
/// comparison is structural and independent of serialization.
fn normalized(value: &Value, live_origin: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(live_origin, RECORDED_ORIGIN)),
        Value::Array(items) => Value::Array(items.iter().map(|item| normalized(item, live_origin)).collect()),
        Value::Object(map) => {
            let mut map: Map<String, Value> = map
                .iter()
                .map(|(key, value)| (key.clone(), normalized(value, live_origin)))
                .collect();
            if map.get("type") == Some(&json!("tool_result")) {
                let parsed = map
                    .get("content")
                    .and_then(Value::as_str)
                    .and_then(|content| serde_json::from_str::<Value>(content).ok());
                if let Some(mut parsed) = parsed {
                    if parsed.get("retrieved_at").is_some() {
                        parsed["retrieved_at"] = json!("<retrieved_at>");
                    }
                    map.insert("content".to_owned(), parsed);
                }
            }
            Value::Object(map)
        }
        other => other.clone(),
    }
}

async fn registry(harness: &Harness, ctx: &MessagesRequestContext) -> ToolRegistry {
    let mut tools = registry_tools(ctx.tools(), &GatewayToolMap::default());
    let mut executors = harness.exec_ctx.gateway_executors.clone();
    ToolRegistry::build_with_handlers(&mut tools, &mut executors)
        .await
        .expect("registry")
}

#[tokio::test]
async fn recorded_nonstreaming_web_fetch_turn_replays_through_the_json_loop() {
    let harness = harness("messages-web-fetch-Qwen-Qwen3-8B-nonstreaming.yaml").await;
    let ctx = MessagesRequestContext::from_value(harness.client_request(false)).expect("context");
    let registry = registry(&harness, &ctx).await;
    let upstream = MessagesUpstream::new(&harness.origin, None, reqwest::header::HeaderMap::new());

    let message = run_messages_loop(ctx, &registry, &harness.exec_ctx, &upstream)
        .await
        .expect("loop")
        .body;

    harness.assert_upstream_requests_match_the_recording();
    harness.assert_call_and_result_pair();
    let Recorded::Json(final_turn) = &harness.turns[1].response else {
        panic!("nonstreaming cassette")
    };
    assert_eq!(
        message["content"], final_turn["content"],
        "the client sees the recorded answer"
    );
    assert_eq!(message["content"][0]["text"], TOKEN);
    assert_eq!(message["stop_reason"], "end_turn");
    assert!(
        !message.to_string().contains("web_fetch"),
        "the gateway call never reaches the client: {message}"
    );
    let Recorded::Json(first_turn) = &harness.turns[0].response else {
        panic!("nonstreaming cassette")
    };
    let recorded_input =
        first_turn["usage"]["input_tokens"].as_u64().unwrap() + final_turn["usage"]["input_tokens"].as_u64().unwrap();
    assert_eq!(
        message["usage"]["input_tokens"], recorded_input,
        "usage totals both recorded rounds"
    );
}

#[tokio::test]
async fn recorded_streaming_web_fetch_turn_replays_through_the_sse_loop() {
    let harness = harness("messages-web-fetch-Qwen-Qwen3-8B-streaming.yaml").await;
    let ctx = MessagesRequestContext::from_value(harness.client_request(true)).expect("context");
    let registry = registry(&harness, &ctx).await;
    let upstream = MessagesUpstream::new(&harness.origin, None, reqwest::header::HeaderMap::new());

    let response = run_messages_stream(ctx, Arc::new(registry), Arc::new(harness.exec_ctx.clone()), upstream)
        .await
        .expect("stream");
    let mut body = response.body;
    let mut frames = String::new();
    while let Some(frame) = body.next().await {
        frames.push_str(&frame);
    }

    harness.assert_upstream_requests_match_the_recording();
    harness.assert_call_and_result_pair();
    let events: Vec<Value> = frames
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect();
    assert_eq!(
        events.iter().filter(|event| event["type"] == "message_start").count(),
        1,
        "one client-visible message: {frames}"
    );
    assert_eq!(events.iter().filter(|event| event["type"] == "message_stop").count(), 1);
    let answer: String = events
        .iter()
        .filter(|event| event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta")
        .filter_map(|event| event["delta"]["text"].as_str())
        .collect();
    assert_eq!(answer, TOKEN, "the client receives the recorded answer: {frames}");
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "content_block_start" && event["content_block"]["type"] == "tool_use"),
        "the gateway call never reaches the client: {frames}"
    );
    assert!(!frames.contains("web_fetch"), "{frames}");
    assert!(!frames.contains("event: error"), "{frames}");
}
