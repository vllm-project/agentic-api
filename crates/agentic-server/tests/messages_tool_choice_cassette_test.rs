//! Replay real vLLM forced-to-auto exchanges through the HTTP gateway.
#[allow(dead_code)]
mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agentic_core::executor::ExecutionContext;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
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
    searches: Arc<Mutex<Vec<String>>>,
}

fn sse(response: &Value) -> String {
    response["sse"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap())
        .collect()
}

fn events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

async fn infer(State(state): State<Replay>, Json(request): Json<Value>) -> Response {
    let index = {
        let mut requests = state.requests.lock().await;
        let index = requests.len();
        requests.push(request);
        index
    };
    // Replay twice on one gateway. The test compares every actual request to
    // the complete recorded body, independently of the production selector logic.
    assert!(index < 4, "unexpected extra inference request");
    let response = &state.turns[index % 2]["response"];
    if response.get("sse").is_some() {
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(sse(response)))
            .unwrap()
    } else {
        Json(response["body"].clone()).into_response()
    }
}

async fn search(State(state): State<Replay>, Query(query): Query<HashMap<String, String>>) -> Json<Value> {
    state.searches.lock().await.push(query["query"].clone());
    Json(json!({"results":{"web":[{
        "url":"https://example.com/proof", "title":"proof", "description":"SEARCH_PROOF_雪"
    }]}}))
}

fn assert_completion(body: &str, turns: &[Value], stream: bool) {
    // Both two-round recordings cost 326+430 input and 28+6 output tokens; the
    // public message reports that sum rather than the final round's usage.
    let usage = json!({"input_tokens": 756, "output_tokens": 34});
    if !stream {
        let message: Value = serde_json::from_str(body).unwrap();
        let mut expected = turns[1]["response"]["body"].clone();
        expected["usage"] = usage;
        assert_eq!(message, expected);
        assert_eq!(message["stop_reason"], "end_turn");
        assert!(message["content"].to_string().contains("SEARCH_PROOF_雪"));
        return;
    }
    let actual = events(body);
    let first = events(&sse(&turns[0]["response"]));
    let last = events(&sse(&turns[1]["response"]));
    assert_eq!(actual.first(), first.first());
    assert_eq!(actual.last().unwrap()["type"], "message_stop");
    for kind in ["message_start", "message_delta", "message_stop"] {
        assert_eq!(actual.iter().filter(|event| event["type"] == kind).count(), 1, "{body}");
    }
    let mut expected_terminal = last[last.len() - 2].clone();
    expected_terminal["usage"] = usage;
    assert_eq!(actual[actual.len() - 2], expected_terminal);
    assert_eq!(actual[actual.len() - 2]["delta"]["stop_reason"], "end_turn");
    assert!(
        !actual
            .iter()
            .any(|event| event["type"] == "error" || event["content_block"]["type"] == "tool_use")
    );
    let expected_text: String = first
        .iter()
        .chain(&last)
        .filter_map(|event| event["delta"]["text"].as_str())
        .collect();
    let actual_text: String = actual
        .iter()
        .filter_map(|event| event["delta"]["text"].as_str())
        .collect();
    assert!(actual_text.contains("SEARCH_PROOF_雪"), "{body}");
    assert_eq!(actual_text, expected_text);
}

fn client_tool_content(body: &str, recorded: &Value, stream: bool) -> Vec<Value> {
    if !stream {
        let actual: Value = serde_json::from_str(body).unwrap();
        let mut expected = recorded["body"].clone();
        assert_eq!(
            expected["stop_reason"], "end_turn",
            "record the provider's incorrect stop"
        );
        expected["stop_reason"] = json!("tool_use");
        assert_eq!(actual, expected, "only the public stop reason changes");
        return actual["content"].as_array().unwrap().clone();
    }
    let actual = events(body);
    let mut expected = events(&sse(recorded));
    let terminal = expected
        .iter_mut()
        .find(|event| event["type"] == "message_delta")
        .unwrap();
    assert_eq!(terminal["delta"]["stop_reason"], "end_turn");
    terminal["delta"]["stop_reason"] = json!("tool_use");
    assert_eq!(
        actual, expected,
        "preserve the recorded lifecycle, arguments and metadata"
    );
    let starts: Vec<_> = actual
        .iter()
        .filter(|event| event["type"] == "content_block_start")
        .collect();
    assert_eq!(starts.len(), 1);
    let mut call = starts[0]["content_block"].clone();
    let input: String = actual
        .iter()
        .filter_map(|event| event["delta"]["partial_json"].as_str())
        .collect();
    call["input"] = serde_json::from_str(&input).unwrap();
    vec![call]
}

async fn continue_client_conversation(client: &reqwest::Client, url: &str, body: &str, turns: &[Value], stream: bool) {
    let client_blocks = client_tool_content(body, &turns[0]["response"], stream);
    assert_eq!(client_blocks.len(), 1);
    let call = &client_blocks[0];
    assert_eq!(call["type"], "tool_use");
    assert_eq!(call["name"], "client_echo");
    assert!(call["input"]["query"].is_string());
    // A client runner uses the public call's ID to submit the output.
    let result = json!({"type":"tool_result", "tool_use_id":call["id"], "content":"CLIENT_PROOF_雪"});
    let mut continuation = turns[0]["request"]["body"].clone();
    continuation["tool_choice"] = json!({"type":"auto"});
    continuation["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant", "content":client_blocks}),
        json!({"role":"user", "content":[result]}),
    ]);
    let response = client
        .post(format!("{url}/v1/messages"))
        .json(&continuation)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let body = response.text().await.unwrap();
    if stream {
        let actual = events(&body);
        assert_eq!(actual, events(&sse(&turns[1]["response"])));
        let text: String = actual
            .iter()
            .filter_map(|event| event["delta"]["text"].as_str())
            .collect();
        assert!(text.contains("CLIENT_PROOF_雪"), "{body}");
    } else {
        assert!(body.contains("CLIENT_PROOF_雪"), "{body}");
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            turns[1]["response"]["body"]
        );
    }
}

fn cassette_model(kind: &str) -> &'static str {
    if kind == "client" {
        "Qwen-Qwen3-4B"
    } else {
        "RedHatAI-Qwen3-Coder-Next-NVFP4"
    }
}

async fn replay(kind: &str, stream: bool) {
    let suffix = if stream { "streaming" } else { "nonstreaming" };
    let model = cassette_model(kind);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../agentic-server-core/tests/cassettes/messages/tool-choice/messages-{kind}-{model}-{suffix}.yaml"
    ));
    let cassette: Value = serde_yml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let turns = Arc::new(cassette["turns"].as_array().unwrap().clone());
    assert_eq!(turns.len(), 2);
    assert!(turns.iter().all(|turn| turn["response"]["status_code"] == 200));
    let client_tool = kind == "client";
    assert_eq!(
        turns[0]["request"]["body"]["tool_choice"]["type"],
        if client_tool { "tool" } else { kind }
    );
    assert_eq!(turns[1]["request"]["body"]["tool_choice"]["type"], "auto");
    let upstream = Replay {
        turns: Arc::clone(&turns),
        requests: Arc::default(),
        searches: Arc::default(),
    };
    let app = Router::new()
        .route("/v1/messages", post(infer))
        .route("/v1/search", get(search))
        .with_state(upstream.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let provider = Server(tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }));
    let directory = tempfile::tempdir().unwrap();
    let mut config = common::test_config(&url);
    config.db_url = Some(format!("sqlite://{}", directory.path().join("state.db").display()));
    config.tools.web_search.api_key = Some("local-key".to_owned());
    config.tools.web_search.base_url = Some(url);
    let context = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&context);
    let (url, gateway) = common::spawn_gateway(state).await;
    let gateway = Server(gateway);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let expected_requests: Vec<_> = turns.iter().map(|turn| turn["request"]["body"].clone()).collect();
    for _ in 0..2 {
        let response = client
            .post(format!("{url}/v1/messages"))
            .json(&expected_requests[0])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        let body = response.text().await.unwrap();
        if client_tool {
            continue_client_conversation(&client, &url, &body, &turns, stream).await;
        } else {
            assert_completion(&body, &turns, stream);
        }
    }
    {
        let actual = upstream.requests.lock().await;
        assert_eq!(actual.len(), 4);
        for rounds in actual.chunks_exact(2) {
            assert_eq!(
                rounds, expected_requests,
                "complete provider requests must match the recording"
            );
        }
    }
    let history = expected_requests[1]["messages"].as_array().unwrap();
    let call = history[1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_use")
        .unwrap();
    let result = &history[2]["content"][0];
    assert_eq!(result["tool_use_id"], call["id"]);
    if !client_tool {
        assert_eq!(result["is_error"], false);
    }
    assert_eq!(
        *upstream.searches.lock().await,
        if client_tool {
            vec![]
        } else {
            vec![call["input"]["query"].as_str().unwrap(); 2]
        }
    );
    for table in ["responses", "items", "conversations"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(context.storage_pool().unwrap())
            .await
            .unwrap();
        assert_eq!(count, 0, "Messages must not persist {table}");
    }
    gateway.stop().await;
    provider.stop().await;
    context.storage_pool().unwrap().close().await;
}

#[tokio::test]
async fn recorded_any_choice_finishes_http_search() {
    replay("any", false).await;
}

#[tokio::test]
async fn recorded_named_choice_finishes_http_search() {
    replay("tool", false).await;
}

#[tokio::test]
async fn recorded_any_choice_finishes_streaming_search() {
    replay("any", true).await;
}

#[tokio::test]
async fn recorded_named_choice_finishes_streaming_search() {
    replay("tool", true).await;
}

#[tokio::test]
async fn recorded_named_client_call_resumes_http_conversation() {
    replay("client", false).await;
}

#[tokio::test]
async fn recorded_named_client_call_resumes_streaming_conversation() {
    replay("client", true).await;
}
