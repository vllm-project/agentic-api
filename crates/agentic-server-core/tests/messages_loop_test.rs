//! Acceptance test for the Messages-native gateway tool loop (#115, non-streaming).
//!
//! Drives `run_messages_loop` against a mock vLLM `/v1/messages` upstream that
//! replays the recorded #123 cassette (turn 0: model emits a `web_search`
//! `tool_use`; turn 1: final text after the fed-back `tool_result`) and a mock
//! You.com search backend. Asserts the gateway tool is executed server-side,
//! hidden from the client, and only the final assistant message surfaces.
//!
//! The #123 cassette records real vLLM `/v1/messages` upstream turns — exactly
//! what this loop consumes — so replaying it is a faithful acceptance test, not
//! a hand-authored mock.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agentic_core::executor::{ConversationHandler, ExecutionContext, ResponseHandler, run_messages_loop};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::{ToolRegistry, WebSearchHandler};
use agentic_core::types::messages::{GatewayToolMap, ToolParam, registry_tools};
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

mod support;

const CASSETTE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/cassettes/messages/messages-web-search-Qwen-Qwen3-30B-A3B-FP8-nonstreaming.yaml"
);

/// Load the recorded assistant response bodies (one per turn) from a cassette.
fn cassette_turn_bodies() -> Vec<Value> {
    cassette_bodies_at(CASSETTE)
}

fn cassette_bodies_at(path: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let doc: Value = serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"));
    doc["turns"]
        .as_array()
        .expect("turns array")
        .iter()
        .map(|t| t["response"]["body"].clone())
        .collect()
}

const MULTIROUND_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/messages_multiround");

/// Mock vLLM `/v1/messages` — serves the recorded response bodies in order and
/// records each request body it received (to assert the loop fed the
/// `tool_result` back on round 2).
#[derive(Clone)]
struct UpstreamState {
    bodies: Arc<Vec<Value>>,
    calls: Arc<AtomicUsize>,
    requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
}

async fn spawn_mock_vllm_messages(bodies: Vec<Value>) -> (String, UpstreamState, tokio::task::JoinHandle<()>) {
    let state = UpstreamState {
        bodies: Arc::new(bodies),
        calls: Arc::new(AtomicUsize::new(0)),
        requests: Arc::new(tokio::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route(
            "/v1/messages",
            post(|State(st): State<UpstreamState>, Json(req): Json<Value>| async move {
                let n = st.calls.fetch_add(1, Ordering::SeqCst);
                st.requests.lock().await.push(req);
                let body = st.bodies.get(n).cloned().unwrap_or_else(|| {
                    serde_json::json!({"type": "error", "error": {"type": "api_error", "message": "mock exhausted"}})
                });
                Json(body)
            }),
        )
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), state, handle)
}

struct CapturedSearch {
    body: Value,
}

/// Mock You.com search backend the `web_search` executor calls. Uses an
/// unbounded channel so a test that doesn't drain captures (e.g. the max-rounds
/// cap, which fires ~10 searches) never blocks the handler.
async fn spawn_mock_search() -> (
    String,
    mpsc::UnboundedReceiver<CapturedSearch>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let app = Router::new()
        .route(
            "/v1/search",
            post(|State(tx): State<mpsc::UnboundedSender<CapturedSearch>>, Json(body): Json<Value>| async move {
                let _ = tx.send(CapturedSearch { body });
                Json(serde_json::json!({
                    "results": {"web": [{"url": "https://www.rust-lang.org/", "title": "Rust",
                        "description": "d", "snippets": ["Rust 1.89.0 is the latest stable release."]}], "news": []},
                    "metadata": {"query": "q", "search_uuid": "s1", "latency": 0.1}
                }))
            }),
        )
        .with_state(tx);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), rx, handle)
}

/// Search backend that always 500s — drives a gateway tool dispatch failure (E5).
async fn spawn_failing_search() -> (String, mpsc::Receiver<()>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(8);
    let app = Router::new()
        .route(
            "/v1/search",
            post(
                |State(tx): State<mpsc::Sender<()>>, _body: axum::body::Bytes| async move {
                    let _ = tx.send(()).await;
                    (http::StatusCode::INTERNAL_SERVER_ERROR, "search backend down")
                },
            ),
        )
        .with_state(tx);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), rx, handle)
}

async fn build_exec_ctx(vllm_url: &str, search_url: &str) -> ExecutionContext {
    let pool = support::setup_pool().await;
    let conv = ConversationHandler::new(ConversationStore::new(Arc::clone(&pool)));
    let resp = ResponseHandler::new(ResponseStore::new(pool));
    let client = Arc::new(reqwest::Client::new());
    ExecutionContext::new(conv, resp, Arc::clone(&client), vllm_url.to_owned()).with_gateway_executor(Arc::new(
        WebSearchHandler::with_api_key(client, "test-key".to_owned(), search_url),
    ))
}

fn web_search_request() -> Value {
    serde_json::json!({
        "model": "qwen3",
        "max_tokens": 1024,
        "stream": false,
        "messages": [{"role": "user", "content": "What is the latest stable Rust release? Use web_search."}],
        "tools": [{"name": "web_search", "description": "Search the web.",
            "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}}]
    })
}

#[tokio::test]
async fn messages_loop_hides_gateway_tool_and_surfaces_final_text() {
    let (vllm_url, upstream, _v) = spawn_mock_vllm_messages(cassette_turn_bodies()).await;
    let (search_url, mut captured, _s) = spawn_mock_search().await;
    let exec_ctx = build_exec_ctx(&vllm_url, &search_url).await;

    let request = web_search_request();
    let tools: Vec<ToolParam> = serde_json::from_value(request["tools"].clone()).unwrap();
    let registry = ToolRegistry::build_with_handlers(
        &registry_tools(Some(&tools), &GatewayToolMap::default()),
        &exec_ctx.gateway_executors,
    )
    .await
    .unwrap();

    let result = run_messages_loop(request, &registry, &exec_ctx, None)
        .await
        .expect("loop runs");

    // The gateway executed the web_search server-side.
    let search = captured.recv().await.expect("search backend hit");
    assert!(search.body.get("query").is_some(), "web_search dispatched with a query");

    // Two upstream rounds, and round 2 carried the fed-back tool_result.
    assert_eq!(
        upstream.calls.load(Ordering::SeqCst),
        2,
        "one tool round + one final round"
    );
    let reqs = upstream.requests.lock().await;
    let round2_msgs = reqs[1]["messages"].as_array().expect("round-2 messages");
    let has_tool_result = round2_msgs.iter().any(|m| {
        m["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
    });
    assert!(has_tool_result, "round 2 fed the tool_result back to the model");

    // Hide-the-call: the returned message has NO tool_use block — only the final
    // thinking/text the client should see.
    let content = result["content"].as_array().expect("final content");
    assert!(
        !content.iter().any(|b| b["type"] == "tool_use"),
        "gateway tool_use must be hidden: {content:?}"
    );
    let text: String = content
        .iter()
        .filter(|b| b["type"] == "text")
        .map(|b| b["text"].as_str().unwrap_or_default())
        .collect();
    assert!(text.contains("1.89.0"), "final answer surfaces: {text}");
    assert_eq!(result["stop_reason"], "end_turn");
}

// ── Repro tests for Maral's #131 review (currently FAILING — proves each bug) ──

// F3: the assistant turn fed back on the next round must preserve preceding
// thinking/text/signature blocks, not just the gateway tool_use. Dropping them
// loses conversation state and breaks extended-thinking round-tripping.
#[tokio::test]
async fn repro_f3_next_round_preserves_thinking_and_text_blocks() {
    // Round 0: assistant emits thinking + text + a gateway tool_use.
    let round0 = serde_json::json!({
        "id": "m", "type": "message", "role": "assistant", "model": "qwen3",
        "content": [
            {"type": "thinking", "thinking": "let me search", "signature": "sig_abc"},
            {"type": "text", "text": "I'll look that up."},
            {"type": "tool_use", "id": "t1", "name": "web_search", "input": {"query": "rust"}}
        ],
        "stop_reason": "tool_use", "usage": {"input_tokens": 5, "output_tokens": 3}
    });
    let round1 = serde_json::json!({
        "id": "m2", "type": "message", "role": "assistant", "model": "qwen3",
        "content": [{"type": "text", "text": "Rust 1.89.0."}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 5, "output_tokens": 3}
    });
    let (vllm_url, upstream, _v) = spawn_mock_vllm_messages(vec![round0, round1]).await;
    let (search_url, _rx, _s) = spawn_mock_search().await;
    let exec_ctx = build_exec_ctx(&vllm_url, &search_url).await;
    let request = web_search_request();
    let tools: Vec<ToolParam> = serde_json::from_value(request["tools"].clone()).unwrap();
    let registry = ToolRegistry::build_with_handlers(
        &registry_tools(Some(&tools), &GatewayToolMap::default()),
        &exec_ctx.gateway_executors,
    )
    .await
    .unwrap();
    run_messages_loop(request, &registry, &exec_ctx, None).await.unwrap();

    // Inspect the assistant turn the loop appended for round 2.
    let reqs = upstream.requests.lock().await;
    let round2_msgs = reqs[1]["messages"].as_array().expect("round-2 messages");
    let assistant = round2_msgs
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant")
        .expect("round-2 has a reconstructed assistant turn");
    let block_types: Vec<&str> = assistant["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["type"].as_str())
        .collect();
    // The model's thinking + text must be carried forward alongside the tool_use.
    assert!(
        block_types.contains(&"thinking"),
        "thinking preserved in history: {block_types:?}"
    );
    assert!(
        block_types.contains(&"text"),
        "text preserved in history: {block_types:?}"
    );
    assert!(block_types.contains(&"tool_use"), "tool_use present: {block_types:?}");
}

// F5: mixed gateway + client tool_use — the non-streaming path must NOT expose
// the gateway tool_use (hide-the-call), matching streaming. One consistent
// policy: surface the client tool_use, suppress the gateway one, stop the loop.
#[tokio::test]
async fn repro_f5_mixed_call_hides_gateway_tool_use() {
    let body = serde_json::json!({
        "id": "m", "type": "message", "role": "assistant", "model": "qwen3",
        "content": [
            {"type": "tool_use", "id": "g1", "name": "web_search", "input": {"query": "x"}},
            {"type": "tool_use", "id": "c1", "name": "get_weather", "input": {"city": "SF"}}
        ],
        "stop_reason": "tool_use", "usage": {"input_tokens": 5, "output_tokens": 3}
    });
    let mut request = web_search_request();
    request["tools"] = serde_json::json!([
        {"name": "web_search", "description": "s", "input_schema": {"type": "object"}},
        {"name": "get_weather", "description": "w", "input_schema": {"type": "object"}}
    ]);
    let (result, _calls) = run_against(vec![body], request).await;
    let names: Vec<&str> = result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .filter_map(|b| b["name"].as_str())
        .collect();
    assert!(names.contains(&"get_weather"), "client tool_use surfaces: {names:?}");
    assert!(
        !names.contains(&"web_search"),
        "gateway tool_use must be hidden: {names:?}"
    );
}

/// Build a registry + `exec_ctx` for a canned single-turn upstream (no search
/// hit expected).
async fn run_against(bodies: Vec<Value>, request: Value) -> (Value, usize) {
    let (vllm_url, upstream, _v) = spawn_mock_vllm_messages(bodies).await;
    let (search_url, _rx, _s) = spawn_mock_search().await;
    let exec_ctx = build_exec_ctx(&vllm_url, &search_url).await;
    let tools: Vec<ToolParam> = serde_json::from_value(request["tools"].clone()).unwrap_or_default();
    let registry = ToolRegistry::build_with_handlers(
        &registry_tools(Some(&tools), &GatewayToolMap::default()),
        &exec_ctx.gateway_executors,
    )
    .await
    .unwrap();
    let result = run_messages_loop(request, &registry, &exec_ctx, None).await.unwrap();
    (result, upstream.calls.load(Ordering::SeqCst))
}

// E2: gateway tool declared but the model answers directly (end_turn) — one
// round, returned as-is, no loop.
#[tokio::test]
async fn messages_loop_returns_immediately_when_model_does_not_call_tool() {
    let body = serde_json::json!({
        "id": "m", "type": "message", "role": "assistant", "model": "qwen3",
        "content": [{"type": "text", "text": "Rust 1.89.0."}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 5, "output_tokens": 3}
    });
    let (result, calls) = run_against(vec![body], web_search_request()).await;
    assert_eq!(calls, 1, "one round only");
    assert_eq!(result["stop_reason"], "end_turn");
    assert_eq!(result["content"][0]["text"], "Rust 1.89.0.");
}

// E7: a client-owned tool_use in the turn must be returned to the client (the
// loop can't execute it server-side), even if a gateway tool is also declared.
#[tokio::test]
async fn messages_loop_returns_client_owned_tool_use_to_client() {
    let body = serde_json::json!({
        "id": "m", "type": "message", "role": "assistant", "model": "qwen3",
        "content": [{"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "SF"}}],
        "stop_reason": "tool_use", "usage": {"input_tokens": 5, "output_tokens": 3}
    });
    // Declare BOTH a gateway tool and the client tool.
    let mut request = web_search_request();
    request["tools"] = serde_json::json!([
        {"name": "web_search", "description": "s", "input_schema": {"type": "object"}},
        {"name": "get_weather", "description": "w", "input_schema": {"type": "object"}}
    ]);
    let (result, calls) = run_against(vec![body], request).await;
    assert_eq!(calls, 1, "client tool_use ends the loop in one round");
    assert_eq!(result["stop_reason"], "tool_use");
    assert_eq!(
        result["content"][0]["name"], "get_weather",
        "client tool_use surfaces to the client"
    );
}

// Multi-round (3 rounds): replay the live-recorded sequential cassette
// (tool_use -> tool_use -> text). The loop must run three upstream rounds,
// hit the search backend twice, and surface only the final text.
#[tokio::test]
async fn messages_loop_multi_round_sequential() {
    let bodies = cassette_bodies_at(&format!(
        "{MULTIROUND_DIR}/sequential-web-search-qwen3-nonstreaming.yaml"
    ));
    assert_eq!(bodies.len(), 3, "cassette has 3 upstream rounds");
    let (vllm_url, upstream, _v) = spawn_mock_vllm_messages(bodies).await;
    let (search_url, mut captured, _s) = spawn_mock_search().await;
    let exec_ctx = build_exec_ctx(&vllm_url, &search_url).await;
    let request = web_search_request();
    let tools: Vec<ToolParam> = serde_json::from_value(request["tools"].clone()).unwrap();
    let registry = ToolRegistry::build_with_handlers(
        &registry_tools(Some(&tools), &GatewayToolMap::default()),
        &exec_ctx.gateway_executors,
    )
    .await
    .unwrap();

    let result = run_messages_loop(request, &registry, &exec_ctx, None).await.unwrap();

    assert_eq!(upstream.calls.load(Ordering::SeqCst), 3, "three upstream rounds");
    // Two gateway searches executed (rounds 0 and 1).
    captured.recv().await.expect("first search");
    captured.recv().await.expect("second search");
    let content = result["content"].as_array().unwrap();
    assert!(!content.iter().any(|b| b["type"] == "tool_use"), "gateway tools hidden");
    assert_eq!(result["stop_reason"], "end_turn");
}

// Parallel: replay the live-recorded parallel cassette (two tool_use blocks in
// one turn). Both gateway calls execute; neither surfaces.
#[tokio::test]
async fn messages_loop_parallel_tool_use() {
    let bodies = cassette_bodies_at(&format!("{MULTIROUND_DIR}/parallel-web-search-qwen3-nonstreaming.yaml"));
    let (vllm_url, upstream, _v) = spawn_mock_vllm_messages(bodies).await;
    let (search_url, mut captured, _s) = spawn_mock_search().await;
    let exec_ctx = build_exec_ctx(&vllm_url, &search_url).await;
    let request = web_search_request();
    let tools: Vec<ToolParam> = serde_json::from_value(request["tools"].clone()).unwrap();
    let registry = ToolRegistry::build_with_handlers(
        &registry_tools(Some(&tools), &GatewayToolMap::default()),
        &exec_ctx.gateway_executors,
    )
    .await
    .unwrap();

    let result = run_messages_loop(request, &registry, &exec_ctx, None).await.unwrap();

    // Two parallel gateway calls both executed against the backend.
    captured.recv().await.expect("first parallel search");
    captured.recv().await.expect("second parallel search");
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2, "tool round + final round");
    let content = result["content"].as_array().unwrap();
    assert!(!content.iter().any(|b| b["type"] == "tool_use"), "gateway tools hidden");
    // Round 2 fed BOTH tool_results back.
    let reqs = upstream.requests.lock().await;
    let round2 = reqs[1]["messages"].as_array().unwrap();
    let tool_results: usize = round2
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "tool_result")
        .count();
    assert_eq!(tool_results, 2, "both parallel tool_results fed back");
}

// E5: a gateway tool that fails to dispatch (search backend returns 500) becomes
// an error tool_result fed back to the model — never a whole-request failure.
#[tokio::test]
async fn messages_loop_tool_failure_becomes_error_tool_result() {
    // Round 0: web_search call; round 1: final text (model recovers from the error).
    let round0 = serde_json::json!({
        "id": "m", "type": "message", "role": "assistant", "model": "qwen3",
        "content": [{"type": "tool_use", "id": "t1", "name": "web_search", "input": {"query": "x"}}],
        "stop_reason": "tool_use", "usage": {"input_tokens": 5, "output_tokens": 3}
    });
    let round1 = serde_json::json!({
        "id": "m2", "type": "message", "role": "assistant", "model": "qwen3",
        "content": [{"type": "text", "text": "Search failed, here's what I know."}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 5, "output_tokens": 3}
    });
    let (vllm_url, upstream, _v) = spawn_mock_vllm_messages(vec![round0, round1]).await;
    // Search backend that returns 500 → dispatch error.
    let (search_url, _rx, _s) = spawn_failing_search().await;
    let exec_ctx = build_exec_ctx(&vllm_url, &search_url).await;
    let request = web_search_request();
    let tools: Vec<ToolParam> = serde_json::from_value(request["tools"].clone()).unwrap();
    let registry = ToolRegistry::build_with_handlers(
        &registry_tools(Some(&tools), &GatewayToolMap::default()),
        &exec_ctx.gateway_executors,
    )
    .await
    .unwrap();

    let result = run_messages_loop(request, &registry, &exec_ctx, None).await.unwrap();

    // The request did NOT fail — it looped to a final answer.
    assert_eq!(result["stop_reason"], "end_turn");
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);
    // Round 2 fed back a tool_result (carrying the error text), not a hard failure.
    let reqs = upstream.requests.lock().await;
    let round2 = reqs[1]["messages"].as_array().unwrap();
    let has_tool_result = round2
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .any(|b| b["type"] == "tool_result");
    assert!(has_tool_result, "tool failure fed back as an (error) tool_result");
}

// E14: an upstream error body mid-loop is surfaced (not swallowed or looped on).
#[tokio::test]
async fn messages_loop_surfaces_upstream_error_body() {
    let err = serde_json::json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}});
    let (result, calls) = run_against(vec![err], web_search_request()).await;
    assert_eq!(calls, 1, "error surfaced on the first round, no loop");
    assert_eq!(result["type"], "error");
    assert_eq!(result["error"]["type"], "overloaded_error");
}

// E4: the loop caps at MAX rounds. Feed an unbounded run of tool_use rounds and
// assert it terminates with an error rather than looping forever.
#[tokio::test]
async fn messages_loop_caps_at_max_rounds() {
    let tool_round = serde_json::json!({
        "id": "m", "type": "message", "role": "assistant", "model": "qwen3",
        "content": [{"type": "tool_use", "id": "t", "name": "web_search", "input": {"query": "x"}}],
        "stop_reason": "tool_use", "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    // 20 tool rounds available, but the loop must stop at its cap (10).
    let bodies = vec![tool_round; 20];
    let (vllm_url, upstream, _v) = spawn_mock_vllm_messages(bodies).await;
    let (search_url, _rx, _s) = spawn_mock_search().await;
    let exec_ctx = build_exec_ctx(&vllm_url, &search_url).await;
    let request = web_search_request();
    let tools: Vec<ToolParam> = serde_json::from_value(request["tools"].clone()).unwrap();
    let registry = ToolRegistry::build_with_handlers(
        &registry_tools(Some(&tools), &GatewayToolMap::default()),
        &exec_ctx.gateway_executors,
    )
    .await
    .unwrap();

    let result = run_messages_loop(request, &registry, &exec_ctx, None).await.unwrap();

    let calls = upstream.calls.load(Ordering::SeqCst);
    assert!(calls <= 10, "loop must cap at MAX_GATEWAY_TOOL_ROUNDS (got {calls})");
    assert_eq!(result["type"], "error", "round-budget exhaustion surfaces an error");
    assert!(
        result["error"]["message"].as_str().unwrap().contains("rounds"),
        "error mentions the round cap"
    );
}

// E3: a malformed tool_use.input (not an object) must not panic — the arg
// stringification falls back and the call still dispatches.
#[tokio::test]
async fn messages_loop_handles_malformed_tool_input() {
    let round0 = serde_json::json!({
        "id": "m", "type": "message", "role": "assistant", "model": "qwen3",
        // input as a bare string instead of an object.
        "content": [{"type": "tool_use", "id": "t1", "name": "web_search", "input": "not-an-object"}],
        "stop_reason": "tool_use", "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let round1 = serde_json::json!({
        "id": "m2", "type": "message", "role": "assistant", "model": "qwen3",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let (result, calls) = run_against(vec![round0, round1], web_search_request()).await;
    assert_eq!(calls, 2, "loop still ran the tool round + final");
    assert_eq!(result["stop_reason"], "end_turn");
}
