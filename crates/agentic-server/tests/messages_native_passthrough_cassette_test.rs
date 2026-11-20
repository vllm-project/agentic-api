//! Replays REAL Anthropic Messages recordings of native server-tool
//! pass-through (#409) over the gateway's transparent `/v1/messages` path.
//!
//! The cassettes were captured by `record_409_native_passthrough.py` through a
//! live gateway whose upstream executes `web_search_20250305` itself
//! (`MESSAGES_UPSTREAM_SERVER_TOOLS=web_search_20250305`). No response body,
//! SSE frame, or tool block was authored by hand: turn 1 carries the real
//! `server_tool_use` / `web_search_tool_result` blocks from the provider, and
//! turn 2 carries the follow-up in which the client sends those blocks back.
//!
//! The replay serves those recorded responses from a mock upstream and checks
//! the provider contract: the native declaration is forwarded unchanged, the
//! real server-tool blocks reach the client verbatim (including streamed
//! `input_json_delta` frames that reconstruct to the upstream's input), and a
//! later turn that echoes the blocks back reaches the upstream untouched.

#![allow(dead_code)]
mod common;

use std::sync::{Arc, Mutex};

use agentic_core::types::messages::UpstreamServerTools;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const CASSETTE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../agentic-server-core/tests/cassettes/messages"
);

struct Turn {
    request: Value,
    /// Raw recorded response: JSON body or the SSE frame list.
    sse: Option<Vec<String>>,
    body: Value,
    status: u16,
}

fn load(cassette: &str) -> Vec<Turn> {
    let path = format!("{CASSETTE_DIR}/{cassette}");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let doc: Value = serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"));
    doc["turns"]
        .as_array()
        .expect("turns")
        .iter()
        .map(|t| Turn {
            request: t["request"]["body"].clone(),
            sse: t["response"]["sse"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()),
            body: t["response"]["body"].clone(),
            status: u16::try_from(t["response"]["status_code"].as_u64().unwrap_or(200)).expect("status code"),
        })
        .collect()
}

#[derive(Clone)]
struct Backend {
    turns: Arc<Vec<Arc<Turn>>>,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}

async fn infer(State(backend): State<Backend>, Json(body): Json<Value>) -> axum::response::Response {
    let idx = {
        let mut requests = backend.requests.lock().unwrap();
        requests.push(("/v1/messages".to_owned(), body));
        requests.len() - 1
    };
    let Some(turn) = backend.turns.get(idx) else {
        return (axum::http::StatusCode::BAD_GATEWAY, "cassette exhausted").into_response();
    };
    if let Some(frames) = &turn.sse {
        axum::response::Response::builder()
            .status(turn.status)
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from(frames.join("")))
            .unwrap()
    } else {
        (
            axum::http::StatusCode::from_u16(turn.status).unwrap(),
            Json(turn.body.clone()),
        )
            .into_response()
    }
}

struct Harness {
    url: String,
    turns: Vec<Arc<Turn>>,
    backend: Backend,
    _server: tokio::task::JoinHandle<()>,
}

async fn harness(cassette: &str) -> Harness {
    let turns: Vec<Arc<Turn>> = load(cassette).into_iter().map(Arc::new).collect();
    assert_eq!(turns.len(), 2, "{cassette}: native call then replayed follow-up");
    let backend = Backend {
        turns: Arc::new(turns.clone()),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/v1/messages", post(infer))
        .with_state(backend.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let config = common::test_config(&url);
    let mut state = common::test_state(&config);
    // The upstream runs the native tool; the gateway forwards it untouched.
    // test_state builds its ExecutionContext with the default policy, so set
    // the ownership policy directly, as a live gateway does from the env var.
    let exec_ctx = Arc::new(
        state
            .exec_ctx
            .as_ref()
            .clone()
            .with_messages_upstream_server_tools(UpstreamServerTools::parse("web_search_20250305").unwrap()),
    );
    state.exec_ctx = exec_ctx;
    let (gateway_url, gateway_server) = common::spawn_gateway(state).await;
    assert!(!gateway_server.is_finished(), "gateway must stay up");

    Harness {
        url: gateway_url,
        turns,
        backend,
        _server: gateway_server,
    }
}

impl Harness {
    fn upstream_requests(&self) -> Vec<(String, Value)> {
        self.backend.requests.lock().unwrap().clone()
    }
}

/// The real, recorded native declaration must pass through untouched.
fn assert_native_declaration_forwarded(sent: &Value) {
    let tools = sent["tools"].as_array().expect("tools");
    assert_eq!(
        tools[0],
        json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 1}),
        "the native declaration reaches the upstream unchanged: {tools:?}"
    );
}

fn assert_real_server_tool_blocks(blocks: &[Value]) {
    let tool = blocks
        .iter()
        .find(|b| b["type"] == "server_tool_use")
        .expect("server_tool_use");
    let id = tool["id"].as_str().unwrap();
    assert!(id.starts_with("srvtoolu_"), "upstream-owned id: {id}");
    assert_eq!(tool["name"], "web_search");
    assert!(tool["input"].get("query").is_some(), "real query input: {tool:?}");
    let result = blocks
        .iter()
        .find(|b| b["type"] == "web_search_tool_result")
        .expect("result block");
    assert_eq!(result["tool_use_id"], id);
    assert!(
        result["content"].as_array().is_some_and(|c| !c.is_empty()),
        "real search results"
    );
}

#[tokio::test]
async fn recorded_nonstreaming_native_passthrough_and_replay() {
    let harness = harness("messages-native-server-tool-passthrough-nonstreaming.yaml").await;
    let recorded = &harness.turns[0].request;

    // Turn 1: send exactly the recorded client request.
    let first = reqwest::Client::new()
        .post(format!("{}/v1/messages", harness.url))
        .json(recorded)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let message: Value = first.json().await.unwrap();
    assert_real_server_tool_blocks(message["content"].as_array().unwrap());

    // Turn 2: send the recorded follow-up, which echoes the real server-tool
    // blocks back to the upstream.
    let follow_up = &harness.turns[1].request;
    let assistant = follow_up["messages"][1]["content"].as_array().unwrap();
    assert_real_server_tool_blocks(assistant);
    let second = reqwest::Client::new()
        .post(format!("{}/v1/messages", harness.url))
        .json(follow_up)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);

    let requests = harness.upstream_requests();
    assert_eq!(requests.len(), 2, "one upstream request per turn");
    assert_eq!(requests[0].1["messages"], recorded["messages"]);
    assert_native_declaration_forwarded(&requests[0].1);
    assert_eq!(
        requests[1].1["messages"], follow_up["messages"],
        "replayed blocks reach the upstream verbatim"
    );
}

#[tokio::test]
async fn recorded_streaming_native_passthrough_reconstructs_real_input() {
    let harness = harness("messages-native-server-tool-passthrough-streaming.yaml").await;
    let recorded = &harness.turns[0].request;

    let first = reqwest::Client::new()
        .post(format!("{}/v1/messages", harness.url))
        .json(recorded)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert!(
        first
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/event-stream")
    );
    let frames = first.text().await.unwrap();

    // The provider's real input_json_delta frames must survive the pass-through
    // and reconstruct to the exact input the upstream recorded.
    let deltas: Vec<String> = frames
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .filter(|event| event["delta"]["type"] == "input_json_delta")
        .filter_map(|event| event["delta"]["partial_json"].as_str().map(str::to_owned))
        .collect();
    assert!(!deltas.is_empty(), "real input_json_delta frames are forwarded");
    let reconstructed: Value = serde_json::from_str(&deltas.join("")).expect("reconstructed input is valid JSON");

    // The expected input comes from the recorded *follow-up* request (the
    // assistant blocks the client sent back on turn 2), not from the same SSE
    // frames above. That keeps this assertion from being circular: a gateway
    // replay error that corrupts the block would diverge from the recorded
    // client request.
    let follow_up = &harness.turns[1].request;
    let replayed_assistant = follow_up["messages"][1]["content"].as_array().unwrap();
    let expected_input = replayed_assistant
        .iter()
        .find(|b| b["type"] == "server_tool_use")
        .map(|b| b["input"].clone())
        .expect("recorded server_tool_use input in the turn-2 follow-up");
    assert_eq!(
        reconstructed, expected_input,
        "streamed delta reconstruction matches the input the client replayed"
    );

    assert!(frames.contains("srvtoolu_"), "upstream id passes through");
    assert!(frames.contains("web_search_tool_result"), "result block passes through");
    assert!(!frames.contains("srvtoolu_gw_"), "no gateway-rewritten id");
    assert!(!frames.contains("event: error"), "no error frame: {frames}");

    // Turn 2: send the recorded streaming follow-up through the gateway and
    // assert the real server-tool blocks reach the upstream unchanged, as the
    // non-streaming test does.
    assert_real_server_tool_blocks(replayed_assistant);
    let second = reqwest::Client::new()
        .post(format!("{}/v1/messages", harness.url))
        .json(follow_up)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    assert!(
        second
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/event-stream")
    );
    let _ = second.text().await.unwrap();

    let requests = harness.upstream_requests();
    assert_eq!(requests.len(), 2, "one upstream request per turn");
    assert_eq!(requests[0].1["messages"], recorded["messages"]);
    assert_native_declaration_forwarded(&requests[0].1);
    assert_eq!(
        requests[1].1["messages"], follow_up["messages"],
        "replayed streaming blocks reach the upstream verbatim"
    );
}
