//! `GET /v1/responses/{id}/input_items` lists the stored context a response was generated from.
#[allow(dead_code)]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agentic_core::executor::ExecutionContext;
use agentic_core::storage::{ResponseMetadata, ResponseStore};
use axum::{Json, Router, routing::post};
use http::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Mock inference upstream answering every turn with one uniquely identified message.
fn answering_upstream() -> Router {
    answering_upstream_with(json!([]))
}

/// Like [`answering_upstream`], but each turn's output starts with `extra_output`.
fn answering_upstream_with(extra_output: Value) -> Router {
    let next_message_id = Arc::new(AtomicUsize::new(0));
    Router::new().route(
        "/v1/responses",
        post(move |Json(_): Json<Value>| {
            let extra_output = extra_output.clone();
            let message_id = next_message_id.fetch_add(1, Ordering::Relaxed);
            let mut output = extra_output.as_array().cloned().unwrap_or_default();
            output.push(json!({
                "type":"message", "id":format!("msg_answer_{message_id}"), "role":"assistant",
                "status":"completed", "content":[{"type":"output_text", "text":format!("answer {message_id}")}]}));
            async move {
                Json(json!({"id":"resp_upstream", "object":"response", "created_at":123,
                "model":"test-model", "status":"completed", "output":output,
                "usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}}))
            }
        }),
    )
}

struct Harness {
    url: String,
    client: reqwest::Client,
    exec: Arc<ExecutionContext>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with(answering_upstream()).await
    }

    async fn start_with(upstream: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = common::test_config(&format!("http://{}", listener.local_addr().unwrap()));
        let upstream = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        config.db_url = Some("sqlite://?mode=memory".into());
        let exec = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
        let mut state = common::test_state(&config);
        state.exec_ctx = Arc::clone(&exec);
        let (url, gateway) = common::spawn_gateway(state).await;
        Self {
            url,
            client: reqwest::Client::new(),
            exec,
            tasks: vec![upstream, gateway],
        }
    }

    async fn create(&self, body: Value) -> Value {
        let response = self
            .client
            .post(format!("{}/v1/responses", self.url))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn input_items(&self, id: &str, query: &str) -> reqwest::Response {
        self.client
            .get(format!("{}/v1/responses/{id}/input_items{query}", self.url))
            .bearer_auth("test-key")
            .send()
            .await
            .unwrap()
    }

    async fn page(&self, id: &str, query: &str) -> Value {
        let response = self.input_items(id, query).await;
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

/// The text of each item on a page: input text, or the first content part's text.
fn texts(page: &Value) -> Vec<String> {
    page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            item["content"]
                .as_str()
                .or_else(|| item["content"][0]["text"].as_str())
                .expect("text item")
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn input_items_list_the_inherited_context_without_the_response_own_output() {
    let harness = Harness::start().await;
    let first = harness
        .create(json!({"model":"test-model", "input":"first", "store":true}))
        .await;
    let first_id = first["id"].as_str().unwrap();
    let second = harness
        .create(json!({"model":"test-model", "input":"second", "store":true, "previous_response_id":first_id}))
        .await;
    let second_id = second["id"].as_str().unwrap();

    // The first turn's own answer is input to the second, but neither lists its own output.
    assert_eq!(texts(&harness.page(first_id, "?order=asc").await), ["first"]);
    let ascending = harness.page(second_id, "?order=asc").await;
    assert_eq!(texts(&ascending), ["first", "answer 0", "second"]);
    assert_eq!(ascending["object"], "list");
    assert_eq!(ascending["has_more"], false);
    assert_eq!(ascending["data"][1]["id"], "msg_answer_0");
    assert!(
        ascending["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != "msg_answer_1"),
        "a response's own output is not one of its inputs"
    );
    // Like conversation item listing, the default order is newest first.
    assert_eq!(
        texts(&harness.page(second_id, "").await),
        ["second", "answer 0", "first"]
    );

    // Pagination walks the same sequence one item at a time.
    let mut cursor = String::new();
    let mut walked = Vec::new();
    loop {
        let page = harness.page(second_id, &format!("?order=asc&limit=1{cursor}")).await;
        walked.extend(texts(&page));
        assert_eq!(page["first_id"], page["last_id"]);
        if page["has_more"] == false {
            break;
        }
        cursor = format!("&after={}", page["last_id"].as_str().unwrap());
    }
    assert_eq!(walked, ["first", "answer 0", "second"]);

    // OpenAI removes the deleted parent's turn from the surviving child's input window.
    let deleted = harness
        .client
        .delete(format!("{}/v1/responses/{first_id}", harness.url))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(texts(&harness.page(second_id, "?order=asc").await), ["second"]);

    harness.stop().await;
}

#[tokio::test]
async fn input_items_of_a_conversation_turn_reflect_the_conversation_at_that_turn() {
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
    let first = harness
        .create(json!({"model":"test-model", "input":"first", "conversation":conversation_id}))
        .await;
    let second = harness
        .create(json!({"model":"test-model", "input":"second", "conversation":conversation_id}))
        .await;
    // A later turn does not leak into an earlier response's inputs.
    harness
        .create(json!({"model":"test-model", "input":"third", "conversation":conversation_id}))
        .await;

    assert_eq!(
        texts(&harness.page(first["id"].as_str().unwrap(), "?order=asc").await),
        ["first"]
    );
    assert_eq!(
        texts(&harness.page(second["id"].as_str().unwrap(), "?order=asc").await),
        ["first", "answer 0", "second"]
    );
    harness.stop().await;
}

#[tokio::test]
async fn input_items_reject_what_retrieval_rejects() {
    let harness = Harness::start().await;
    let stored = harness
        .create(json!({"model":"test-model", "input":"first", "store":true}))
        .await;
    let stored_id = stored["id"].as_str().unwrap();
    let transient = harness
        .create(json!({"model":"test-model", "input":"transient", "store":false}))
        .await;

    let unauthenticated = harness
        .client
        .get(format!("{}/v1/responses/{stored_id}/input_items", harness.url))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    for missing in [transient["id"].as_str().unwrap(), "resp_missing"] {
        assert_eq!(harness.input_items(missing, "").await.status(), StatusCode::NOT_FOUND);
    }
    // Recorded OpenAI edge cases accept an empty page and known include fields.
    let empty = harness.page(stored_id, "?limit=0").await;
    assert_eq!(empty["data"], json!([]));
    assert_eq!(empty["has_more"], true);
    assert_eq!(
        harness
            .input_items(stored_id, "?include[]=web_search_call.results")
            .await
            .status(),
        StatusCode::OK
    );
    for (query, code, param) in [
        ("?limit=101", "integer_above_max_value", "limit"),
        ("?limit=-1", "integer_below_min_value", "limit"),
        ("?order=sideways", "invalid_value", "order"),
        ("?include=no.such.field", "invalid_value", "include[0]"),
        ("?limit=ten", "invalid_type", "limit"),
        ("?limit=1&limit=2", "duplicate_parameter", "limit"),
    ] {
        let response = harness.input_items(stored_id, query).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        let body: Value = response.json().await.expect("a structured error body");
        assert_eq!(body["error"]["type"], "invalid_request_error", "{query}");
        assert_eq!(body["error"]["code"], code, "{query}");
        assert_eq!(body["error"]["param"], param, "{query}");
    }
    // Same contract as conversation item listing: an unknown cursor is a not-found naming `after`.
    let unknown_cursor = harness.input_items(stored_id, "?after=msg_unknown").await;
    assert_eq!(unknown_cursor.status(), StatusCode::NOT_FOUND);
    assert_eq!(unknown_cursor.json::<Value>().await.unwrap()["error"]["param"], "after");

    // A record without a response snapshot cannot tell its output from its input.
    let store = ResponseStore::new(Arc::new(harness.exec.storage_pool().unwrap().clone()));
    store
        .persist("resp_legacy", None, Vec::new(), &ResponseMetadata::default())
        .await
        .unwrap();
    assert_eq!(
        harness.input_items("resp_legacy", "").await.status(),
        StatusCode::CONFLICT
    );

    // Deleting a response removes its input listing with it.
    let deleted = harness
        .client
        .delete(format!("{}/v1/responses/{stored_id}", harness.url))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(harness.input_items(stored_id, "").await.status(), StatusCode::NOT_FOUND);
    harness.stop().await;
}

#[tokio::test]
async fn input_items_exclude_own_output_items_that_carry_no_wire_id() {
    // A compaction item's wire `id` is optional; storage generates one for the row.
    let harness = Harness::start_with(answering_upstream_with(
        json!([{"type":"compaction", "encrypted_content":"opaque"}]),
    ))
    .await;
    let first = harness
        .create(json!({"model":"test-model", "input":"first", "store":true}))
        .await;
    let first_id = first["id"].as_str().unwrap();
    assert!(
        first["output"][0].get("id").is_none(),
        "the upstream compaction item has no id"
    );
    let second = harness
        .create(json!({"model":"test-model", "input":"second", "store":true, "previous_response_id":first_id}))
        .await;

    let own = harness.page(first_id, "?order=asc").await;
    assert_eq!(own["data"].as_array().unwrap().len(), 1, "{own}");
    assert_eq!(texts(&own), ["first"]);
    // The same item, under its generated ID, is input to the next turn.
    let next = harness.page(second["id"].as_str().unwrap(), "?order=asc").await;
    let types: Vec<&str> = next["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["message", "compaction", "message", "message"], "{next}");
    assert!(next["data"][1]["id"].as_str().unwrap().starts_with("cmp_"));

    // Conversation turns exclude appended outputs without wire IDs too.
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
    let in_conversation = harness
        .create(json!({"model":"test-model", "input":"conversation input", "store":true,
            "conversation":conversation["id"]}))
        .await;
    let conversation_inputs = harness
        .page(in_conversation["id"].as_str().unwrap(), "?order=asc")
        .await;
    assert_eq!(texts(&conversation_inputs), ["conversation input"]);
    harness.stop().await;
}

#[tokio::test]
async fn input_items_keep_an_earlier_turn_output_that_shares_this_turn_output_id() {
    // An upstream may reuse a message ID across turns. Each occurrence is its own history
    // row, so only this turn's row is excluded, not the earlier turn's answer.
    let upstream = Router::new().route(
        "/v1/responses",
        post(|Json(_): Json<Value>| async {
            Json(json!({"id":"resp_upstream", "object":"response", "created_at":123,
            "model":"test-model", "status":"completed", "output":[{
                "type":"message", "id":"msg_reused", "role":"assistant", "status":"completed",
                "content":[{"type":"output_text", "text":"same id"}]}],
            "usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}}))
        }),
    );
    let harness = Harness::start_with(upstream).await;
    let first = harness
        .create(json!({"model":"test-model", "input":"first", "store":true}))
        .await;
    let second = harness
        .create(json!({"model":"test-model", "input":"second", "store":true,
            "previous_response_id":first["id"]}))
        .await;
    assert_eq!(
        texts(&harness.page(second["id"].as_str().unwrap(), "?order=asc").await),
        ["first", "same id", "second"]
    );
    harness.stop().await;
}

#[tokio::test]
async fn input_items_reuse_conversation_query_parameters() {
    let harness = Harness::start().await;
    let stored = harness
        .create(
            json!({"model":"test-model", "store":true, "input":[{"role":"user", "content":[
            {"type":"input_text", "text":"look"},
            {"type":"input_image", "image_url":"data:image/png;base64,AAAA"}]}]}),
        )
        .await;
    let page = harness
        .page(
            stored["id"].as_str().unwrap(),
            "?include[]=message.output_text.logprobs",
        )
        .await;
    assert_eq!(
        page["data"][0]["content"][1]["image_url"], "data:image/png;base64,AAAA",
        "{page}"
    );
    harness.stop().await;
}

#[tokio::test]
async fn input_item_pages_make_progress_past_a_repeated_public_id() {
    // The upstream reuses one message ID every turn, so the third turn's input holds
    // that public ID twice. A cursor naming it resumes after its furthest occurrence,
    // as conversation item listing does, instead of returning the same page forever.
    let upstream = Router::new().route(
        "/v1/responses",
        post(|Json(_): Json<Value>| async {
            Json(json!({"id":"resp_upstream", "object":"response", "created_at":123,
            "model":"test-model", "status":"completed", "output":[{
                "type":"message", "id":"msg_reused", "role":"assistant", "status":"completed",
                "content":[{"type":"output_text", "text":"same id"}]}],
            "usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}}))
        }),
    );
    let harness = Harness::start_with(upstream).await;
    let mut previous = Value::Null;
    for input in ["first", "second", "third"] {
        previous = harness
            .create(json!({"model":"test-model", "input":input, "store":true, "previous_response_id":previous["id"]}))
            .await;
    }
    let third = previous["id"].as_str().unwrap();
    assert_eq!(
        texts(&harness.page(third, "?order=asc").await),
        ["first", "same id", "second", "same id", "third"]
    );

    for (order, expected) in [
        ("asc", ["first", "same id", "third"]),
        ("desc", ["third", "same id", "first"]),
    ] {
        let mut cursor = String::new();
        let mut walked = Vec::new();
        for _ in 0..10 {
            let page = harness.page(third, &format!("?order={order}&limit=1{cursor}")).await;
            walked.extend(texts(&page));
            if page["has_more"] == false {
                break;
            }
            cursor = format!("&after={}", page["last_id"].as_str().unwrap());
        }
        assert_eq!(walked, expected, "{order}");
    }
    harness.stop().await;
}

#[tokio::test]
async fn an_empty_input_turn_keeps_the_previous_answer_in_its_input_list() {
    let harness = Harness::start().await;
    let first = harness
        .create(json!({"model":"test-model", "input":"first", "store":true}))
        .await;
    let second = harness
        .create(json!({"model":"test-model", "input":[], "store":true,
        "previous_response_id":first["id"]}))
        .await;
    assert_eq!(
        texts(&harness.page(second["id"].as_str().unwrap(), "?order=asc").await),
        ["first", "answer 0"]
    );
    harness.stop().await;
}
