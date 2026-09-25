use std::{collections::VecDeque, num::NonZeroUsize, sync::Arc, time::Duration};

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use either::Either;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::{sync::Mutex, task::JoinHandle};

use crate::config::ResponsesConfig;
use crate::executor::{
    BoxStream, ConversationHandler, ExecuteRequest, ExecutionContext, ExecutorResult, ResponseHandler, ResponseSession,
    ResponseSessionGroup, inference::transport::ResponsesTransport,
};
use crate::storage::{ConversationStore, DbPool, ResponseStore, create_pool_with_schema};
use crate::types::{
    RequestPayload, ResponsePayload, reasoning_profile::OpaqueReasoningProfile, reasoning_replay::ReasoningReplayPolicy,
};

pub(super) const AUTH: &str = "offline-qualification-credential";
pub(super) const PROFILE: OpaqueReasoningProfile = OpaqueReasoningProfile::OpenAiGpt54_20260305V1;
const MAX_REQUESTS: usize = 8;

#[derive(Deserialize)]
pub(super) struct Capture {
    pub turns: Vec<Turn>,
}

#[derive(Deserialize)]
pub(super) struct Turn {
    pub request: Request,
    pub response: Response,
}

#[derive(Deserialize)]
pub(super) struct Request {
    pub body: Value,
}

#[derive(Clone, Deserialize)]
pub(super) struct Response {
    pub body: Option<Value>,
    pub sse: Option<Vec<String>>,
}

pub(super) fn capture(scenario: &str, streaming: bool) -> Capture {
    let mode = if streaming { "sse" } else { "json" };
    let path = format!(
        "{}/tests/cassettes/reasoning/opaque/gpt-5.4-2026-03-05/{scenario}-{mode}.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read(path).unwrap();
    assert!(raw.len() < 1_000_000);
    let capture: Capture = serde_yaml::from_slice(&raw).unwrap();
    assert_eq!(capture.turns.len(), 3);
    capture
}

pub(super) fn request(turn: &Turn, store: bool) -> RequestPayload {
    let mut request: RequestPayload = serde_json::from_value(turn.request.body.clone()).unwrap();
    request.store = store;
    request
}

/// Submit only the new suffix; the executor must restore all earlier items itself.
pub(super) fn child(turn: &Turn, prefix_len: usize, parent: &str, store: bool) -> RequestPayload {
    let mut request = request(turn, store);
    request.input = serde_json::from_value(Value::Array(
        turn.request.body["input"].as_array().unwrap()[prefix_len..].to_vec(),
    ))
    .unwrap();
    request.previous_response_id = Some(parent.to_owned());
    request
}

#[derive(Default)]
struct WireState {
    replies: VecDeque<Response>,
    requests: Vec<Value>,
}

pub(super) struct Fixture {
    pub context: Arc<ExecutionContext>,
    pub pool: Arc<DbPool>,
    wire: Arc<Mutex<WireState>>,
    task: JoinHandle<()>,
}

impl Fixture {
    pub async fn new(responses: impl IntoIterator<Item = Response>) -> Self {
        let replies: VecDeque<_> = responses.into_iter().take(MAX_REQUESTS + 1).collect();
        assert!(replies.len() <= MAX_REQUESTS);
        for response in &replies {
            let bytes = response.body.as_ref().map_or(0, |body| body.to_string().len())
                + response
                    .sse
                    .as_ref()
                    .map_or(0, |lines| lines.iter().map(String::len).sum::<usize>());
            assert!(bytes <= 1_000_000, "bounded replay response");
        }
        let wire = Arc::new(Mutex::new(WireState {
            replies,
            requests: Vec::with_capacity(MAX_REQUESTS),
        }));
        let router = Router::new()
            .route(
                "/v1/responses",
                post(
                    |State(state): State<Arc<Mutex<WireState>>>, Json(request): Json<Value>| async move {
                        let reply = {
                            let mut state = state.lock().await;
                            if state.requests.len() >= MAX_REQUESTS {
                                return StatusCode::TOO_MANY_REQUESTS.into_response();
                            }
                            state.requests.push(request);
                            state.replies.pop_front()
                        };
                        match reply {
                            Some(Response { body: Some(body), .. }) => Json(body).into_response(),
                            Some(Response { sse: Some(lines), .. }) => {
                                ([("content-type", "text/event-stream")], lines.join("")).into_response()
                            }
                            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                        }
                    },
                ),
            )
            .layer(axum::extract::DefaultBodyLimit::max(1_000_000))
            .with_state(Arc::clone(&wire));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
        let mut context = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
            ResponseHandler::new(ResponseStore::new(Arc::clone(&pool))),
            Arc::new(reqwest::Client::new()),
            "https://api.openai.com".to_owned(),
        )
        .with_responses_config(ResponsesConfig {
            reasoning_replay_policy: ReasoningReplayPolicy::OpaqueResponses,
            reasoning_replay_profile: Some(PROFILE),
            ..ResponsesConfig::default()
        });
        context.opaque_replay_fixture = Some(ResponsesTransport::replay_fixture(address));
        Self {
            context: Arc::new(context),
            pool,
            wire,
            task,
        }
    }

    pub async fn requests(&self) -> Vec<Value> {
        self.wire.lock().await.requests.clone()
    }

    pub async fn row_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM responses")
            .fetch_one(self.pool.as_ref())
            .await
            .unwrap()
    }

    pub async fn run(
        &self,
        request: RequestPayload,
        session: Option<&ResponseSession>,
    ) -> ExecutorResult<Either<ResponsePayload, BoxStream>> {
        let mut execute = ExecuteRequest::new(request, Arc::clone(&self.context)).with_auth(Some(AUTH.to_owned()));
        if let Some(session) = session {
            execute = execute.with_session(session)?;
        }
        tokio::time::timeout(Duration::from_secs(5), execute.run())
            .await
            .expect("bounded execution")
    }

    pub async fn stop(mut self) {
        self.task.abort();
        assert!((&mut self.task).await.unwrap_err().is_cancelled());
        self.pool.close().await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) fn group() -> ResponseSessionGroup {
    ResponseSessionGroup::new(
        NonZeroUsize::new(4).unwrap(),
        NonZeroUsize::new(128).unwrap(),
        NonZeroUsize::new(128 * 1024).unwrap(),
        NonZeroUsize::new(512 * 1024).unwrap(),
    )
}

pub(super) async fn collect(result: Either<ResponsePayload, BoxStream>) -> ResponsePayload {
    let Either::Right(mut stream) = result else {
        return result.left().unwrap();
    };
    let mut terminal = None;
    let mut previous = None;
    let mut count = 0;
    while let Some(frame) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
    {
        count += 1;
        assert!(count <= 512 && frame.len() <= 1_000_000);
        for data in frame.lines().filter_map(|line| line.strip_prefix("data:")) {
            if data.trim() == "[DONE]" {
                continue;
            }
            let event: Value = serde_json::from_str(data).unwrap();
            assert!(
                event["type"] != "error" && event["type"] != "response.failed",
                "unexpected execution error"
            );
            if let Some(sequence) = event["sequence_number"].as_u64() {
                if let Some(previous) = previous {
                    assert_eq!(sequence, previous + 1);
                }
                previous = Some(sequence);
            }
            if event["type"] == "response.completed" {
                assert!(terminal.is_none(), "one public terminal across all rounds");
                terminal = Some(serde_json::from_value(event["response"].clone()).unwrap());
            }
        }
    }
    terminal.expect("completed response")
}

/// Exact replay-critical fields, with public-only text annotations excluded.
pub(super) fn assert_input(expected: &Value, actual: &Value) {
    let expected = expected.as_array().unwrap();
    let actual = actual.as_array().unwrap();
    assert_eq!(actual.len(), expected.len(), "history length");
    for (expected, actual) in expected.iter().zip(actual) {
        for field in [
            "type",
            "id",
            "role",
            "status",
            "phase",
            "summary",
            "encrypted_content",
            "call_id",
            "name",
            "arguments",
            "output",
        ] {
            assert!(expected[field] == actual[field], "replay field {field} changed");
        }
        if expected["type"] == "message" {
            let mut content = expected["content"].clone();
            if let Some(parts) = content.as_array_mut() {
                for part in parts {
                    part.as_object_mut().unwrap().remove("annotations");
                    part.as_object_mut().unwrap().remove("logprobs");
                }
            }
            assert!(content == actual["content"], "message content changed");
        }
    }
}

pub(super) fn assert_request(turn: &Turn, actual: &Value) {
    assert_eq!(actual["store"], false);
    assert!(actual.get("previous_response_id").is_none() && actual.get("conversation").is_none());
    for field in ["model", "stream", "reasoning", "max_output_tokens", "tools"] {
        assert!(
            turn.request.body[field] == actual[field],
            "request field {field} changed"
        );
    }
    assert_input(&turn.request.body["input"], &actual["input"]);
    for item in actual["input"].as_array().unwrap() {
        if item["type"] == "reasoning" {
            assert!(
                item.get("content").is_none(),
                "opaque replay must omit plaintext content"
            );
        }
    }
}
