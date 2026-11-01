//! `configuration_update` input items keep their place in stored history, continuations and the upstream input.
#[allow(dead_code)]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agentic_core::executor::ExecutionContext;
use axum::{Json, Router, routing::post};
use http::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

fn update(effort: &str) -> Value {
    json!({"type": "configuration_update", "reasoning": {"effort": effort}})
}

fn user(text: &str) -> Value {
    json!({"type": "message", "role": "user", "content": text})
}

fn kinds(items: &Value) -> Vec<&str> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["type"].as_str().unwrap_or("message"))
        .collect()
}

struct Harness {
    url: String,
    client: reqwest::Client,
    exec: Arc<ExecutionContext>,
    upstream: Arc<Mutex<Vec<Value>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Harness {
    /// A gateway with in-memory storage, in front of an upstream that records each request body and answers with one
    /// assistant message.
    async fn start() -> Self {
        let upstream = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&upstream);
        let next_id = Arc::new(AtomicUsize::new(0));
        let router = Router::new().route(
            "/v1/responses",
            post(move |Json(body): Json<Value>| {
                let recorded = Arc::clone(&recorded);
                let id = next_id.fetch_add(1, Ordering::Relaxed);
                async move {
                    recorded.lock().await.push(body);
                    Json(json!({"id": "resp_upstream", "object": "response", "created_at": 1, "model": "test-model",
                        "status": "completed", "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4},
                        "output": [{"type": "message", "id": format!("msg_answer_{id}"), "role": "assistant",
                            "status": "completed", "content": [{"type": "output_text", "text": format!("answer {id}")}]}]}))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = common::test_config(&format!("http://{}", listener.local_addr().unwrap()));
        let upstream_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        config.db_url = Some("sqlite://?mode=memory".into());
        let exec = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
        let mut state = common::test_state(&config);
        state.exec_ctx = Arc::clone(&exec);
        let (url, gateway) = common::spawn_gateway(state).await;
        Self {
            url,
            client: reqwest::Client::new(),
            exec,
            upstream,
            tasks: vec![upstream_task, gateway],
        }
    }

    async fn post(&self, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}/v1/responses", self.url))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn create(&self, body: Value) -> Value {
        let response = self.post(body).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn last_upstream_body(&self) -> Value {
        self.upstream.lock().await.last().cloned().expect("an upstream request")
    }

    async fn input_items(&self, id: &str) -> Value {
        let response = self
            .client
            .get(format!("{}/v1/responses/{id}/input_items?order=asc", self.url))
            .bearer_auth("test-key")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn stop(self) {
        for task in self.tasks {
            task.abort();
            let _ = task.await;
        }
        self.exec.storage_pool().unwrap().close().await;
    }
}

#[tokio::test]
async fn stored_update_keeps_its_place_through_continuation() {
    let harness = Harness::start().await;
    let input = json!([user("first"), update("high"), user("second")]);
    let first = harness
        .create(json!({"model": "test-model", "store": true, "reasoning": {"effort": "low"}, "input": input}))
        .await;
    let first_id = first["id"].as_str().unwrap();
    assert_eq!(
        first["reasoning"]["effort"], "low",
        "the response reports the request-level effort"
    );

    let first_upstream = harness.last_upstream_body().await;
    assert_eq!(first_upstream["reasoning"], json!({"effort": "low"}));
    assert_eq!(
        first_upstream["input"], input,
        "the update reaches the upstream as sent, in place"
    );

    let stored = harness.input_items(first_id).await;
    assert_eq!(kinds(&stored["data"]), ["message", "configuration_update", "message"]);
    assert_eq!(stored["data"][1]["reasoning"], json!({"effort": "high"}));
    assert!(stored["data"][1]["id"].as_str().is_some_and(|id| !id.is_empty()));

    harness
        .create(
            json!({"model": "test-model", "store": true, "reasoning": {"effort": "low"},
            "previous_response_id": first_id, "input": "third"}),
        )
        .await;
    let upstream = harness.last_upstream_body().await;
    assert_eq!(upstream["reasoning"], json!({"effort": "low"}));
    assert_eq!(
        kinds(&upstream["input"]),
        ["message", "configuration_update", "message", "message", "message"]
    );
    assert_eq!(
        upstream["input"].as_array().unwrap()[..3],
        first_upstream["input"].as_array().unwrap()[..],
        "the continuation replays the first turn's input unchanged, update included"
    );

    harness.stop().await;
}

#[tokio::test]
async fn a_branch_keeps_only_its_own_updates() {
    let harness = Harness::start().await;
    let root = harness
        .create(json!({"model": "test-model", "store": true, "input": "root"}))
        .await;
    let root_id = root["id"].as_str().unwrap();
    harness
        .create(
            json!({"model": "test-model", "store": true, "previous_response_id": root_id,
            "input": [update("high"), {"role": "user", "content": "hard branch"}]}),
        )
        .await;
    harness
        .create(
            json!({"model": "test-model", "store": true, "previous_response_id": root_id,
            "input": "plain branch"}),
        )
        .await;

    let upstream = harness.last_upstream_body().await;
    assert_eq!(kinds(&upstream["input"]), ["message", "message", "message"]);
    assert_eq!(upstream["input"][2]["content"], "plain branch");

    harness.stop().await;
}

#[tokio::test]
async fn conversation_history_keeps_the_update() {
    let harness = Harness::start().await;
    let conversation: Value = harness
        .client
        .post(format!("{}/v1/conversations", harness.url))
        .bearer_auth("test-key")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let conversation_id = conversation["id"].as_str().unwrap();
    harness
        .create(
            json!({"model": "test-model", "conversation": conversation_id, "input": [update("high"), user("first")]}),
        )
        .await;
    harness
        .create(json!({"model": "test-model", "conversation": conversation_id, "input": "second"}))
        .await;

    let upstream = harness.last_upstream_body().await;
    assert_eq!(
        kinds(&upstream["input"]),
        ["configuration_update", "message", "message", "message"]
    );
    assert_eq!(upstream["input"][0]["reasoning"], json!({"effort": "high"}));

    harness.stop().await;
}

#[tokio::test]
async fn replayed_update_reaches_the_upstream_without_storage() {
    let harness = Harness::start().await;
    let input = json!([
        user("first"),
        {"type": "message", "id": "msg_answer_0", "role": "assistant", "status": "completed",
         "content": [{"type": "output_text", "text": "answer 0"}]},
        update("medium"),
        user("second")
    ]);
    harness
        .create(json!({"model": "test-model", "store": false, "reasoning": {"effort": "low"}, "input": input}))
        .await;

    let upstream = harness.last_upstream_body().await;
    assert_eq!(upstream["reasoning"], json!({"effort": "low"}));
    assert_eq!(
        kinds(&upstream["input"]),
        ["message", "message", "configuration_update", "message"]
    );
    assert_eq!(upstream["input"][2], update("medium"));
    assert_eq!(upstream["input"][3], user("second"));

    harness.stop().await;
}

#[tokio::test]
async fn malformed_update_is_rejected_before_upstream() {
    let harness = Harness::start().await;
    for update in [
        json!({"type": "configuration_update", "reasoning": {}}),
        json!({"type": "configuration_update", "reasoning": {"effort": " "}}),
        json!({"type": "configuration_update", "reasoning": {"effort": "high", "summary": "auto"}}),
        json!({"type": "configuration_update", "reasoning": {"effort": "high"}, "text": {"format": {"type": "text"}}}),
    ] {
        let response = harness
            .post(json!({"model": "test-model", "store": true, "input": [update, user("hi")]}))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }
    assert!(harness.upstream.lock().await.is_empty());

    harness.stop().await;
}
