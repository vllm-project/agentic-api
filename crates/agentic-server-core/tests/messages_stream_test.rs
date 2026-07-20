//! Acceptance test for the streaming Messages-native gateway tool loop (#115).
//!
//! Drives `run_messages_stream` against a mock vLLM `/v1/messages` that replays
//! the recorded #123 **streaming** cassette (turn 0 streams a `web_search`
//! `tool_use`; turn 1 streams the final text) as `text/event-stream`, plus a
//! mock You.com backend. Asserts the client sees ONE logical message
//! (`message_start`/`message_stop` once), the gateway `tool_use` is suppressed,
//! block indices stay contiguous across rounds, and no raw per-round terminal
//! leaks.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agentic_core::executor::{ConversationHandler, ExecutionContext, ResponseHandler, run_messages_stream};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::{ToolRegistry, WebSearchHandler};
use agentic_core::types::messages::{GatewayToolMap, ToolParam, registry_tools};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use http::StatusCode;
use serde_json::Value;
use tokio::net::TcpListener;

mod support;

const CASSETTE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/cassettes/messages/messages-web-search-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml"
);

const MULTIROUND: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/cassettes/messages_multiround/multiround-web-search-qwen3-streaming.yaml"
);

/// Load each streaming turn's SSE body (the raw event-stream text) from the cassette.
fn cassette_turn_streams() -> Vec<String> {
    streams_at(CASSETTE)
}

fn streams_at(path: &str) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let doc: Value = serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"));
    doc["turns"]
        .as_array()
        .expect("turns")
        .iter()
        .map(|t| {
            let mut body = t["response"]["sse"]
                .as_array()
                .expect("sse array")
                .iter()
                .map(|l| l.as_str().unwrap_or_default())
                .collect::<String>();
            if !body.contains("data: [DONE]") {
                body.push_str("data: [DONE]\n\n");
            }
            body
        })
        .collect()
}

#[derive(Clone)]
struct UpstreamState {
    streams: Arc<Vec<String>>,
    calls: Arc<AtomicUsize>,
}

async fn spawn_mock_vllm_stream(streams: Vec<String>) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let state = UpstreamState {
        streams: Arc::new(streams),
        calls: Arc::clone(&calls),
    };
    let app = Router::new()
        .route(
            "/v1/messages",
            post(|State(st): State<UpstreamState>, _body: axum::body::Bytes| async move {
                let n = st.calls.fetch_add(1, Ordering::SeqCst);
                let body = st.streams.get(n).cloned().unwrap_or_default();
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from(body))
                    .unwrap()
                    .into_response()
            }),
        )
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), calls, handle)
}

async fn spawn_mock_search() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/v1/search",
        post(|Json(_body): Json<Value>| async move {
            Json(serde_json::json!({
                "results": {"web": [{"url": "https://www.rust-lang.org/", "title": "Rust",
                    "description": "d", "snippets": ["Rust 1.89.0 is the latest stable release."]}], "news": []},
                "metadata": {"query": "q", "search_uuid": "s1", "latency": 0.1}
            }))
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), handle)
}

async fn build_exec_ctx(vllm_url: &str, search_url: &str) -> Arc<ExecutionContext> {
    let pool = support::setup_pool().await;
    let conv = ConversationHandler::new(ConversationStore::new(Arc::clone(&pool)));
    let resp = ResponseHandler::new(ResponseStore::new(pool));
    let client = Arc::new(reqwest::Client::new());
    Arc::new(
        ExecutionContext::new(conv, resp, Arc::clone(&client), vllm_url.to_owned()).with_gateway_executor(Arc::new(
            WebSearchHandler::with_api_key(client, "test-key".to_owned(), search_url),
        )),
    )
}

#[tokio::test]
async fn messages_stream_presents_one_message_and_hides_gateway_tool() {
    let (vllm_url, calls, _v) = spawn_mock_vllm_stream(cassette_turn_streams()).await;
    let (search_url, _s) = spawn_mock_search().await;
    let exec_ctx = build_exec_ctx(&vllm_url, &search_url).await;

    let request = serde_json::json!({
        "model": "qwen3", "max_tokens": 1024, "stream": true,
        "messages": [{"role": "user", "content": "What is the latest stable Rust release? Use web_search."}],
        "tools": [{"name": "web_search", "description": "s",
            "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}}]
    });
    let tools: Vec<ToolParam> = serde_json::from_value(request["tools"].clone()).unwrap();
    let registry = Arc::new(
        ToolRegistry::build_with_handlers(
            &registry_tools(Some(&tools), &GatewayToolMap::default()),
            &exec_ctx.gateway_executors,
        )
        .await
        .unwrap(),
    );

    let stream = run_messages_stream(request, registry, Arc::clone(&exec_ctx), None);
    let chunks: Vec<String> = stream.collect().await;
    let sse = chunks.join("");

    // Two upstream rounds ran (tool round + final).
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one tool round + one final round");

    // Exactly one logical message lifecycle.
    assert_eq!(
        sse.matches("event: message_start").count(),
        1,
        "one message_start: {sse:?}"
    );
    assert_eq!(sse.matches("event: message_stop").count(), 1, "one message_stop");
    assert_eq!(
        sse.matches("event: message_delta").count(),
        1,
        "one terminal message_delta (intermediate suppressed)"
    );

    // Gateway tool_use suppressed — no tool_use content block surfaces.
    assert!(
        !sse.contains(r#""type":"tool_use""#),
        "gateway tool_use must be hidden from the client stream"
    );

    // Final terminal is end_turn (not the intermediate tool_use).
    assert!(sse.contains(r#""stop_reason":"end_turn""#), "terminal is end_turn");

    // Block indices contiguous across rounds (no reset/collision): parse every
    // content_block_start index and assert 0..N with no dup.
    let mut indices: Vec<u64> = Vec::new();
    for line in sse.lines() {
        if let Some(d) = line.strip_prefix("data: ") {
            if let Ok(ev) = serde_json::from_str::<Value>(d) {
                if ev["type"] == "content_block_start" {
                    indices.push(ev["index"].as_u64().expect("index"));
                }
            }
        }
    }
    assert!(!indices.is_empty(), "some blocks surfaced");
    assert_eq!(
        indices,
        (0..indices.len() as u64).collect::<Vec<_>>(),
        "surfaced block indices contiguous across rounds: {indices:?}"
    );
}

// Multi-round streaming: replay the live-recorded multi-round streaming cassette
// and assert the same single-lifecycle / contiguous-index / hidden-tool
// invariants hold across a tool round + a final round.
#[tokio::test]
async fn messages_stream_multiround_single_lifecycle() {
    let (vllm_url, calls, _v) = spawn_mock_vllm_stream(streams_at(MULTIROUND)).await;
    let (search_url, _s) = spawn_mock_search().await;
    let exec_ctx = build_exec_ctx(&vllm_url, &search_url).await;

    let request = serde_json::json!({
        "model": "qwen3", "max_tokens": 1024, "stream": true,
        "messages": [{"role": "user", "content": "Use web_search for the latest rust version, then its date."}],
        "tools": [{"name": "web_search", "description": "s",
            "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}}]
    });
    let tools: Vec<ToolParam> = serde_json::from_value(request["tools"].clone()).unwrap();
    let registry = Arc::new(
        ToolRegistry::build_with_handlers(
            &registry_tools(Some(&tools), &GatewayToolMap::default()),
            &exec_ctx.gateway_executors,
        )
        .await
        .unwrap(),
    );

    let stream = run_messages_stream(request, registry, Arc::clone(&exec_ctx), None);
    let sse = stream.collect::<Vec<_>>().await.join("");

    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "at least a tool round + a final round"
    );
    assert_eq!(
        sse.matches("event: message_start").count(),
        1,
        "one message_start across rounds"
    );
    assert_eq!(
        sse.matches("event: message_stop").count(),
        1,
        "one message_stop across rounds"
    );
    assert!(
        !sse.contains(r#""type":"tool_use""#),
        "gateway tool_use suppressed in the client stream"
    );
    assert!(
        !sse.contains("response.output_text.delta"),
        "no raw Responses SSE leaks"
    );
    // Contiguous surfaced indices across rounds.
    let mut idx = Vec::new();
    for line in sse.lines() {
        if let Some(d) = line.strip_prefix("data: ") {
            if let Ok(ev) = serde_json::from_str::<Value>(d) {
                if ev["type"] == "content_block_start" {
                    idx.push(ev["index"].as_u64().expect("index"));
                }
            }
        }
    }
    assert_eq!(
        idx,
        (0..idx.len() as u64).collect::<Vec<_>>(),
        "contiguous indices: {idx:?}"
    );
}
