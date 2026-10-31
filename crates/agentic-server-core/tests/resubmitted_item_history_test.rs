//! Recorded multi-turn Responses history keeps every submitted occurrence (issue #385).
//!
//! The two-turn `OpenAI` recordings provide the expected behavior: a continuation rehydrates the
//! stored turn and forwards it upstream, and output resubmitted as input keeps each occurrence.

mod support;

use std::collections::HashSet;
use std::sync::Arc;

use agentic_core::executor::ExecuteRequest;
use agentic_core::storage::ResponseStore;
use agentic_core::types::io::InOutItem;
use agentic_core::types::request_response::ResponsePayload;
use serde_json::{Value, json};
use support::{TestFixture, collect_stream, expected_text, load_cassette, make_request, unwrap_blocking};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/text_only/responses");

fn recorded(name: &str) -> support::Cassette {
    load_cassette(&format!("{DIR}/{name}.yaml"))
}

fn ids(items: &Value) -> Vec<String> {
    items
        .as_array()
        .expect("item array")
        .iter()
        .filter_map(|item| item["id"].as_str().map(str::to_owned))
        .collect()
}

fn text(item: &Value) -> String {
    match &item["content"] {
        Value::String(content) => content.clone(),
        Value::Array(parts) => parts.iter().filter_map(|part| part["text"].as_str()).collect(),
        _ => String::new(),
    }
}

fn roles(items: &Value) -> Vec<String> {
    items
        .as_array()
        .expect("item array")
        .iter()
        .map(|item| {
            item["role"]
                .as_str()
                .or(item["type"].as_str())
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

fn texts(items: &Value) -> Vec<String> {
    items.as_array().expect("item array").iter().map(text).collect()
}

fn input_text(turn: &support::Turn) -> String {
    match &turn.request.body.input {
        Value::String(content) => content.clone(),
        Value::Array(items) => items.iter().map(text).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

fn stored(items: &[InOutItem]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|item| match item {
                InOutItem::Input(input) => serde_json::to_value(input).expect("serialize input item"),
                InOutItem::Output(output) => serde_json::to_value(output).expect("serialize output item"),
            })
            .collect(),
    )
}

async fn run(
    fixture: &TestFixture,
    input: Value,
    store: bool,
    stream: bool,
    previous_response_id: Option<String>,
) -> ResponsePayload {
    let result = ExecuteRequest::new(
        make_request(input, store, stream, previous_response_id, None),
        Arc::clone(&fixture.exec_ctx),
    )
    .run()
    .await
    .expect("execute recorded turn");
    if stream {
        collect_stream(result).await
    } else {
        unwrap_blocking(result)
    }
}

async fn recorded_continuation_keeps_history(cassette_name: &str) {
    let cassette = recorded(cassette_name);
    let turns = cassette.turns.iter().collect::<Vec<_>>();
    let fixture = TestFixture::new(&turns).await;
    let (first_turn, second_turn) = (&cassette.turns[0], &cassette.turns[1]);
    let store = ResponseStore::new(Arc::clone(&fixture.pool));

    let first = run(
        &fixture,
        first_turn.request.body.input.clone(),
        first_turn.request.body.store,
        first_turn.request.body.stream,
        None,
    )
    .await;
    let second = run(
        &fixture,
        second_turn.request.body.input.clone(),
        second_turn.request.body.store,
        second_turn.request.body.stream,
        Some(first.id.clone()),
    )
    .await;
    assert_eq!(second.status, "completed");

    let first_output = serde_json::to_value(&first.output).expect("serialize first output");
    let second_output = serde_json::to_value(&second.output).expect("serialize second output");
    assert_eq!(texts(&second_output), [expected_text(second_turn)]);

    // The continuation forwards the stored turn and the new input upstream, in order.
    let bodies = fixture.request_bodies().await;
    assert_eq!(bodies.len(), 2, "one upstream request per turn");
    assert_eq!(ids(&bodies[1]["input"]), ids(&first_output));
    assert_eq!(
        texts(&bodies[1]["input"]),
        [
            input_text(first_turn),
            expected_text(first_turn),
            input_text(second_turn)
        ]
    );

    // Stored history keeps the recorded public IDs, roles, and content in order.
    let history = stored(&store.rehydrate(&second.id).await.expect("rehydrate continuation"));
    assert_eq!(
        ids(&history),
        ids(&first_output)
            .into_iter()
            .chain(ids(&second_output))
            .collect::<Vec<_>>()
    );
    assert_eq!(roles(&history), ["user", "assistant", "user", "assistant"]);
    assert_eq!(
        texts(&history),
        [
            input_text(first_turn),
            expected_text(first_turn),
            input_text(second_turn),
            expected_text(second_turn)
        ]
    );

    // Each stored occurrence has a private row ID; public IDs stay stable for clients.
    let row_ids = store
        .get_items(&second.id)
        .await
        .expect("read history rows")
        .into_iter()
        .map(|row| row.id)
        .collect::<Vec<_>>();
    assert_eq!(row_ids.len(), 4);
    assert_eq!(
        row_ids.iter().collect::<HashSet<_>>().len(),
        4,
        "history rows must not share a primary key"
    );
    assert!(
        row_ids.iter().all(|id| id.starts_with("item_")),
        "history rows are addressed by row ID: {row_ids:?}"
    );
}

#[tokio::test]
async fn recorded_nonstreaming_continuation_keeps_history() {
    recorded_continuation_keeps_history("resp-two-turn-gpt-4o-nonstreaming").await;
}

#[tokio::test]
async fn recorded_streaming_continuation_keeps_history() {
    recorded_continuation_keeps_history("resp-two-turn-gpt-4o-streaming").await;
}

async fn resubmitted_output_items_keep_every_occurrence(cassette_name: &str) {
    let cassette = recorded(cassette_name);
    let turns = cassette.turns.iter().collect::<Vec<_>>();
    let fixture = TestFixture::new(&turns).await;
    let (first_turn, second_turn) = (&cassette.turns[0], &cassette.turns[1]);
    let store = ResponseStore::new(Arc::clone(&fixture.pool));

    let first = run(
        &fixture,
        first_turn.request.body.input.clone(),
        true,
        first_turn.request.body.stream,
        None,
    )
    .await;
    let first_output = serde_json::to_value(&first.output).expect("serialize first output");

    // Regression case derived from the recording: resubmit the stored output as new input.
    let mut input = vec![json!({"role": "user", "content": first_turn.request.body.input})];
    input.extend(first_output.as_array().expect("output array").iter().cloned());
    input.push(json!({"role": "user", "content": second_turn.request.body.input}));
    let second = run(
        &fixture,
        Value::Array(input),
        true,
        second_turn.request.body.stream,
        None,
    )
    .await;
    assert_eq!(second.status, "completed", "resubmitted history must persist");

    let second_output = serde_json::to_value(&second.output).expect("serialize second output");
    let history = stored(&store.rehydrate(&second.id).await.expect("rehydrate resubmission"));
    assert_eq!(
        ids(&history),
        ids(&first_output)
            .into_iter()
            .chain(ids(&second_output))
            .collect::<Vec<_>>(),
        "every submitted occurrence stays in input order"
    );
    assert_eq!(roles(&history), ["user", "assistant", "user", "assistant"]);
    assert_eq!(
        texts(&history),
        [
            input_text(first_turn),
            expected_text(first_turn),
            input_text(second_turn),
            expected_text(second_turn)
        ]
    );

    let row_ids = store
        .get_items(&second.id)
        .await
        .expect("read history rows")
        .into_iter()
        .map(|row| row.id)
        .collect::<Vec<_>>();
    assert_eq!(row_ids.len(), 4);
    assert_eq!(
        row_ids.iter().collect::<HashSet<_>>().len(),
        4,
        "repeated public IDs must not reuse a row ID"
    );
    // The resubmitted message keeps the upstream public ID but not as the row's primary key.
    let resubmitted = &ids(&first_output)[0];
    assert_ne!(&row_ids[1], resubmitted);
    assert_eq!(history[1]["id"], json!(resubmitted));
}

#[tokio::test]
async fn resubmitted_nonstreaming_output_items_keep_every_occurrence() {
    resubmitted_output_items_keep_every_occurrence("resp-two-turn-gpt-4o-nonstreaming").await;
}

#[tokio::test]
async fn resubmitted_streaming_output_items_keep_every_occurrence() {
    resubmitted_output_items_keep_every_occurrence("resp-two-turn-gpt-4o-streaming").await;
}
