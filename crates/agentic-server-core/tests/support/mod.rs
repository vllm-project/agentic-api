//! Shared test infrastructure for executor integration tests.
//!
//! - [`MockServer`] — axum-based HTTP mock with RAII shutdown (`Drop`).
//! - [`TestFixture`] — bundles mock server + `ExecutionContext` for one test.
//! - Cassette loading utilities.
//! - Response helpers.

#![allow(dead_code)]

use std::sync::Arc;

use axum::Router;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use either::Either;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use agentic_core::executor::{BoxStream, ConversationHandler, ExecutionContext, ResponseHandler};
use agentic_core::storage::{ConversationStore, DbPool, ResponseStore, create_pool_with_schema};
use agentic_core::types::io::{OutputItem, ResponsesInput};
use agentic_core::types::request_response::{RequestPayload, ResponsePayload};

#[derive(Debug, Deserialize)]
pub struct Cassette {
    pub turns: Vec<Turn>,
}

#[derive(Debug, Deserialize)]
pub struct Turn {
    pub request: TurnRequest,
    pub response: TurnResponse,
}

#[derive(Debug, Deserialize)]
pub struct TurnRequest {
    pub path: String,
    pub body: TurnBody,
}

#[derive(Debug, Deserialize, Default)]
pub struct TurnBody {
    #[serde(default)]
    pub input: String,
    #[serde(default = "default_true")]
    pub store: bool,
    #[serde(default)]
    pub stream: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct TurnResponse {
    /// Non-streaming: full JSON response body.
    pub body: Option<serde_json::Value>,
    /// Streaming: list of raw SSE strings from the recording.
    pub sse: Option<Vec<String>>,
}

/// Load and parse a cassette YAML file (all turns preserved).
pub fn load_cassette(path: &str) -> Cassette {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read cassette {path}: {e}"));
    serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("failed to parse cassette {path}: {e}"))
}

/// Filter to only `/v1/responses` turns — the LLM inference turns that need a
/// mock HTTP response.  Conversation cassettes interleave `/v1/conversations`
/// management turns; the Rust executor handles those internally via
/// [`ConversationHandler`] without any HTTP call.
pub fn responses_turns(cassette: &Cassette) -> Vec<&Turn> {
    cassette
        .turns
        .iter()
        .filter(|t| t.request.path == "/v1/responses")
        .collect()
}

/// Extract the expected output text from a cassette turn.
///
/// - Non-streaming: `body.output[0].content[0].text`
/// - Streaming: concatenate all `response.output_text.delta` values
pub fn expected_text(turn: &Turn) -> String {
    if let Some(body) = &turn.response.body {
        return body["output"][0]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
    }
    if let Some(sse) = &turn.response.sse {
        let mut out = String::new();
        for raw in sse {
            for line in raw.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        if json["type"].as_str() == Some("response.output_text.delta") {
                            if let Some(delta) = json["delta"].as_str() {
                                out.push_str(delta);
                            }
                        }
                    }
                }
            }
        }
        return out;
    }
    String::new()
}

/// A per-test HTTP mock server.  The server task is aborted when this struct
/// is dropped, ensuring clean teardown even if a test panics.
pub struct MockServer {
    url: String,
    handle: JoinHandle<()>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl MockServer {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn request_bodies(&self) -> Vec<Value> {
        self.requests.lock().await.clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn build_response(resp: MockResponse) -> Response {
    match resp {
        MockResponse::Json(body) => Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body))
            .unwrap()
            .into_response(),
        MockResponse::Sse(body) => Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
            .body(axum::body::Body::from(body))
            .unwrap()
            .into_response(),
    }
}

/// A single queued mock response.
pub enum MockResponse {
    Json(String),
    Sse(String),
}

impl MockResponse {
    /// Build a `MockResponse` from a cassette turn.
    pub fn from_turn(turn: &Turn) -> Self {
        if let Some(body) = &turn.response.body {
            return Self::Json(serde_json::to_string(body).expect("cassette body is valid JSON"));
        }
        if let Some(sse) = &turn.response.sse {
            let mut body = sse.join("");
            // Ensure the stream is terminated.
            if !body.contains("data: [DONE]") {
                body.push_str("data: [DONE]\n\n");
            }
            return Self::Sse(body);
        }
        panic!("cassette turn has neither body nor sse");
    }
}

// Use a VecDeque so pop_front is O(1).
impl MockServer {
    pub async fn start_deque(responses: Vec<MockResponse>) -> Self {
        use std::collections::VecDeque;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        let url = format!("http://{addr}");
        // Store as VecDeque for O(1) pop_front.
        let queue: Arc<Mutex<VecDeque<MockResponse>>> = Arc::new(Mutex::new(VecDeque::from(responses)));
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let requests_for_route = Arc::clone(&requests);

        let handle = tokio::spawn(async move {
            let app = Router::new()
                .route(
                    "/v1/responses",
                    post(move |body: axum::body::Bytes| {
                        let queue = Arc::clone(&queue);
                        let requests = Arc::clone(&requests_for_route);
                        async move {
                            let request_body =
                                serde_json::from_slice::<Value>(&body).expect("request body should be valid JSON");
                            requests.lock().await.push(request_body);
                            let mut q = queue.lock().await;
                            let resp = q.pop_front().expect("mock queue exhausted — check test setup");
                            build_response(resp)
                        }
                    }),
                )
                // Conversation management calls don't go through the mock —
                // the executor handles them via ConversationHandler (DB-only).
                // This route is here so the server doesn't return 404 if called.
                .route(
                    "/v1/conversations",
                    post(|| async { (axum::http::StatusCode::OK, "{}") }),
                );
            axum::serve(listener, app).await.ok();
        });

        Self { url, handle, requests }
    }
}

/// Create a fresh `SQLite` pool with schema applied.
///
/// Uses a unique temp-file per call so concurrent tests don't conflict.
pub async fn setup_pool() -> Arc<DbPool> {
    let db_path = std::env::temp_dir().join(format!("test_{}.db", uuid::Uuid::now_v7()));
    let db_url = format!("sqlite://{}", db_path.display());
    create_pool_with_schema(Some(&db_url))
        .await
        .expect("failed to create test pool")
}

/// Bundles everything a test needs.  Dropped at end of test scope.
pub struct TestFixture {
    pub exec_ctx: Arc<ExecutionContext>,
    // Kept for its Drop impl — aborts the mock server when the test ends.
    server: MockServer,
}

impl TestFixture {
    /// Build a fixture from a full cassette turn slice.
    ///
    /// The mock server queues only `/v1/responses` turns (LLM inference).
    /// `/v1/conversations` turns are handled by the executor via
    /// [`ConversationHandler`] (DB-only, no outbound HTTP).
    pub async fn new(turns: &[&Turn]) -> Self {
        let responses = turns
            .iter()
            .filter(|t| t.request.path == "/v1/responses")
            .map(|t| MockResponse::from_turn(t))
            .collect();
        let server = MockServer::start_deque(responses).await;

        let pool = setup_pool().await;
        let conv_handler = ConversationHandler::new(ConversationStore::new(Arc::clone(&pool)));
        let resp_handler = ResponseHandler::new(ResponseStore::new(Arc::clone(&pool)));
        let client = Arc::new(reqwest::Client::new());
        let exec_ctx = Arc::new(ExecutionContext::new(
            conv_handler,
            resp_handler,
            client,
            server.url().to_string(),
        ));

        Self { exec_ctx, server }
    }

    pub async fn new_with_responses(responses: Vec<MockResponse>) -> Self {
        let server = MockServer::start_deque(responses).await;

        let pool = setup_pool().await;
        let conv_handler = ConversationHandler::new(ConversationStore::new(Arc::clone(&pool)));
        let resp_handler = ResponseHandler::new(ResponseStore::new(Arc::clone(&pool)));
        let client = Arc::new(reqwest::Client::new());
        let exec_ctx = Arc::new(ExecutionContext::new(
            conv_handler,
            resp_handler,
            client,
            server.url().to_string(),
        ));

        Self { exec_ctx, server }
    }

    pub async fn request_bodies(&self) -> Vec<Value> {
        self.server.request_bodies().await
    }
}

pub fn text_response(text: &str) -> MockResponse {
    let id_suffix = text.replace(' ', "_");
    MockResponse::Json(
        serde_json::json!({
            "id": format!("resp_upstream_{id_suffix}"),
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [{
                "id": format!("msg_upstream_{id_suffix}"),
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": []
                }]
            }],
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
        .to_string(),
    )
}

pub fn request_input_texts(body: &Value) -> Vec<String> {
    match &body["input"] {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match &item["content"] {
                Value::String(text) => Some(text.clone()),
                Value::Array(parts) => Some(
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn make_request(
    input: &str,
    store: bool,
    stream: bool,
    previous_response_id: Option<String>,
    conversation_id: Option<String>,
) -> RequestPayload {
    RequestPayload {
        model: "test-model".to_string(),
        input: ResponsesInput::Text(input.to_string()),
        instructions: None,
        previous_response_id,
        conversation_id,
        tools: None,
        tool_choice: None,
        stream,
        store,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        truncation: None,
        metadata: None,
    }
}

pub fn unwrap_blocking(result: Either<ResponsePayload, BoxStream>) -> ResponsePayload {
    match result {
        Either::Left(p) => p,
        Either::Right(_) => panic!("expected non-streaming response, got stream"),
    }
}

/// Collect a streaming response to its final `ResponsePayload`.
pub async fn collect_stream(result: Either<ResponsePayload, BoxStream>) -> ResponsePayload {
    let stream = match result {
        Either::Right(s) => s,
        Either::Left(_) => panic!("expected streaming response, got blocking"),
    };
    let mut stream = Box::pin(stream);
    while let Some(chunk) = stream.next().await {
        if let Some(data) = chunk.trim_end_matches('\n').strip_prefix("data: ") {
            if data != "[DONE]" {
                if let Ok(payload) = serde_json::from_str::<ResponsePayload>(data) {
                    while stream.next().await.is_some() {}
                    return payload;
                }
                if let Ok(mut event) = serde_json::from_str::<serde_json::Value>(data)
                    && event.get("type").and_then(serde_json::Value::as_str) == Some("response.completed")
                    && let Some(response) = event.get_mut("response")
                    && let Ok(payload) = serde_json::from_value::<ResponsePayload>(response.take())
                {
                    while stream.next().await.is_some() {}
                    return payload;
                }
            }
        }
    }
    panic!("stream ended without a ResponsePayload chunk");
}

/// Extract concatenated text content from a `ResponsePayload`.
pub fn output_text(payload: &ResponsePayload) -> String {
    payload
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Message(msg) => Some(msg.content.iter().map(|c| c.text.as_str()).collect::<String>()),
            OutputItem::FunctionCall(_)
            | OutputItem::WebSearchCall(_)
            | OutputItem::Reasoning(_)
            | OutputItem::Unknown => None,
        })
        .collect::<String>()
}
