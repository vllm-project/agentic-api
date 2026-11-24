//! Replay live vLLM inference and the captured public thread contract after restart.
#[allow(dead_code)]
mod common;
#[path = "common/messages_database.rs"]
mod messages_database;

use std::sync::Arc;

use agentic_core::executor::ExecutionContext;
use agentic_core::types::messages::thread::MESSAGE_THREADS_BETA;
use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use http::{HeaderMap, StatusCode};
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

#[derive(Clone)]
struct Replay {
    turns: Arc<Vec<Value>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn infer(State(state): State<Replay>, headers: HeaderMap, Json(request): Json<Value>) -> Response {
    assert!(request.get("thread").is_none());
    assert!(
        headers
            .get("anthropic-beta")
            .is_none_or(|v| !v.to_str().unwrap().contains(MESSAGE_THREADS_BETA))
    );
    let index = {
        let mut requests = state.requests.lock().await;
        let index = requests.len();
        requests.push(request);
        index
    };
    let response = &state.turns[index]["response"];
    assert_eq!(response["status_code"], 200);
    if let Some(lines) = response["sse"].as_array() {
        let body: String = lines.iter().map(|line| line.as_str().unwrap()).collect();
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    } else {
        Json(response["body"].clone()).into_response()
    }
}

fn events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

fn normalize_id(message: &mut Value) {
    if message["type"] == "message_start" {
        message["message"]["id"] = json!("generated-thread-message-id");
    } else if message["type"] == "message" {
        message["id"] = json!("generated-thread-message-id");
    }
}

fn assert_public(actual: &str, expected: &Value, stream: bool) -> String {
    let (mut actual, mut expected) = if stream {
        (events(actual), events(expected.as_str().unwrap()))
    } else {
        (vec![serde_json::from_str(actual).unwrap()], vec![expected.clone()])
    };
    let id = if stream {
        &actual[0]["message"]["id"]
    } else {
        &actual[0]["id"]
    };
    let id = id.as_str().unwrap().to_owned();
    for message in actual.iter_mut().chain(expected.iter_mut()) {
        normalize_id(message);
    }
    assert_eq!(
        actual, expected,
        "public content, usage, stop reason and SSE lifecycle must match the live capture"
    );
    id
}

async fn send(url: &str, request: &Value, owner: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/v1/messages?beta=true"))
        .header("anthropic-beta", MESSAGE_THREADS_BETA)
        .header("x-api-key", owner)
        .json(request)
        .send()
        .await
        .unwrap()
}

async fn replay(kind: &str, stream: bool) {
    let suffix = if stream { "streaming" } else { "nonstreaming" };
    let stem = format!("messages-thread-{kind}-Qwen-Qwen3.8-27B-FP8-{suffix}");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agentic-server-core/tests/cassettes/messages/threads");
    let captured: Value =
        serde_yml::from_str(&std::fs::read_to_string(root.join(format!("{stem}.yaml"))).unwrap()).unwrap();
    let public: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(format!("{stem}.json"))).unwrap()).unwrap();
    let turns = Arc::new(captured["turns"].as_array().unwrap().clone());
    let public = public["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(public.len(), 2);
    let upstream = Replay {
        turns,
        requests: Arc::default(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/messages", post(infer))
        .with_state(upstream.clone());
    let provider = Server(tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }));
    let (database_url, _directory) = messages_database::database().await;
    let mut config = common::test_config(&upstream_url);
    config.db_url = Some(database_url);
    let mut previous_id = String::new();
    for (index, turn) in public.iter().enumerate() {
        // Create a fresh execution context/pool and HTTP server from the same database.
        let context = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
        let mut state = common::test_state(&config);
        state.exec_ctx = Arc::clone(&context);
        let (url, handle) = common::spawn_gateway(state).await;
        let gateway = Server(handle);
        let mut request = turn["request"].clone();
        assert_eq!(request["messages"].as_array().unwrap().len(), 1);
        if index == 1 {
            assert!(request.get("system").is_none() && request.get("tools").is_none());
            request["thread"]["previous_message_id"] = json!(previous_id);
            let denied = send(&url, &request, "other-owner").await;
            assert_eq!(denied.status(), StatusCode::NOT_FOUND);
            assert_eq!(upstream.requests.lock().await.len(), 1);
        }
        let response = send(&url, &request, "thread-recording-owner").await;
        assert_eq!(response.status(), StatusCode::OK);
        previous_id = assert_public(&response.text().await.unwrap(), &turn["response"], stream);
        if index == 1 {
            let duplicate = send(&url, &request, "thread-recording-owner").await;
            assert_eq!(duplicate.status(), StatusCode::BAD_REQUEST);
        }
        for table in ["responses", "items", "conversations"] {
            let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(context.storage_pool().unwrap())
                .await
                .unwrap();
            assert_eq!(count, 0, "Messages state remains separate from {table}");
        }
        gateway.stop().await;
        context.messages_threads.shutdown().await;
        context.storage_pool().unwrap().close().await;
    }
    let actual = upstream.requests.lock().await;
    let expected: Vec<_> = upstream
        .turns
        .iter()
        .map(|turn| turn["request"]["body"].clone())
        .collect();
    assert_eq!(
        *actual, expected,
        "rehydrated model input must exactly match the captured vLLM requests"
    );
    assert_eq!(expected[1]["messages"].as_array().unwrap().len(), 3);
    if kind == "client" {
        let call = expected[1]["messages"][1]["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "tool_use")
            .unwrap();
        let result = &expected[1]["messages"][2]["content"][0];
        assert_eq!(call["name"], "client_echo");
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], call["id"]);
        assert_eq!(result["content"], "THREAD_PROOF_91827");
    }
    drop(actual);
    provider.stop().await;
}

#[tokio::test]
async fn recorded_text_json_survives_restart() {
    replay("text", false).await;
}
#[tokio::test]
async fn recorded_text_sse_survives_restart() {
    replay("text", true).await;
}
#[tokio::test]
async fn recorded_client_tool_json_survives_restart() {
    replay("client", false).await;
}
#[tokio::test]
async fn recorded_client_tool_sse_survives_restart() {
    replay("client", true).await;
}
