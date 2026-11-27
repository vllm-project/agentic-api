//! Recorded `OpenAI` behavior for hosted call items sent back with `store: false`: an inline `code_interpreter_call`,
//! and the `mcp_call`, `web_search_call` and `code_interpreter_call` items of a stored response sent back by `id`.
//!
//! The turns are documented in `record_store_false_cassettes.sh` (`STORE_FALSE_SUITE=hosted-calls`); no gateway
//! recording is committed for them.

use serde_json::Value;

mod support;

const CASSETTE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/cassettes/store_false/store-false-hosted-calls-openai-reference-gpt-5.6-sol-nonstreaming.yaml"
);

fn hosted_calls() -> support::Cassette {
    let recorded = support::load_cassette(CASSETTE);
    assert_eq!(
        recorded.turns.len(),
        11,
        "see STORE_FALSE_SUITE=hosted-calls in the recorder"
    );
    recorded
}

/// Item types in order, without reasoning items, which depend on the model.
fn item_types(items: &Value) -> Vec<&str> {
    match items {
        Value::String(_) => vec!["message"],
        Value::Array(items) => items
            .iter()
            .map(|item| item["type"].as_str().unwrap_or("message"))
            .filter(|kind| *kind != "reasoning")
            .collect(),
        other => panic!("unexpected input {other}"),
    }
}

/// One recorded request: whether it stored the response and declared the type's tool, the item types it sent and
/// those `OpenAI` returned.
struct Row {
    store: bool,
    declares_tool: bool,
    sent: &'static [&'static str],
    returned: &'static [&'static str],
}

const fn row(
    store: bool,
    declares_tool: bool,
    sent: &'static [&'static str],
    returned: &'static [&'static str],
) -> Row {
    Row {
        store,
        declares_tool,
        sent,
        returned,
    }
}

/// Checks the turns from `first` on, numbered from 1 as in the recorder.
fn assert_rows(recorded: &support::Cassette, first: usize, rows: &[Row]) {
    for (offset, row) in rows.iter().enumerate() {
        let label = format!("t{}", first + offset);
        let turn = &recorded.turns[first + offset - 1];
        let body = turn.response.body.as_ref().expect("non-streaming response body");
        assert_eq!(turn.response.status_code, Some(200), "{label}: {body}");
        assert_eq!(body["status"], "completed", "{label}");
        assert_eq!(turn.request.body.store, row.store, "{label}");
        assert_eq!(!turn.request.body.tools.is_empty(), row.declares_tool, "{label}");
        assert_eq!(item_types(&turn.request.body.input), row.sent, "{label}");
        assert_eq!(item_types(&body["output"]), row.returned, "{label}");
    }
}

/// The two turns after the stored turn name its output items: as returned with their ids, then as `item_reference`.
fn assert_replays_name_the_stored_items(recorded: &support::Cassette, stored_turn: usize) {
    let stored: Vec<&Value> = recorded.turns[stored_turn - 1]
        .response
        .body
        .as_ref()
        .expect("stored response")["output"]
        .as_array()
        .expect("output items")
        .iter()
        .map(|item| &item["id"])
        .collect();
    for (offset, turn) in recorded.turns[stored_turn..stored_turn + 2].iter().enumerate() {
        let label = format!("t{}", stored_turn + offset + 1);
        let sent = turn.request.body.input.as_array().expect("replayed items");
        // The first item is the original question and the last the follow-up.
        let replayed: Vec<&Value> = sent[1..sent.len() - 1].iter().map(|item| &item["id"]).collect();
        assert_eq!(replayed, stored, "{label}");
        assert_eq!(
            turn.request.body.tool_choice.as_ref(),
            Some(&Value::from("none")),
            "{label}"
        );
    }
}

#[test]
fn openai_accepts_replayed_mcp_calls_by_id() {
    let recorded = hosted_calls();
    let listed = &["mcp_list_tools", "mcp_call", "message"];
    let question = &["message"];
    let replay = &["message", "mcp_list_tools", "mcp_call", "message", "message"];
    let referenced = &[
        "message",
        "item_reference",
        "item_reference",
        "item_reference",
        "item_reference",
        "message",
    ];
    let answer = &["message"];
    assert_rows(
        &recorded,
        3,
        &[
            row(true, true, question, listed),
            row(false, true, replay, answer),
            row(false, true, referenced, answer),
        ],
    );
    assert_replays_name_the_stored_items(&recorded, 3);
}

#[test]
fn openai_accepts_replayed_web_search_calls_by_id() {
    let recorded = hosted_calls();
    let searched = &["web_search_call", "message"];
    let question = &["message"];
    let replay = &["message", "web_search_call", "message", "message"];
    let referenced = &[
        "message",
        "item_reference",
        "item_reference",
        "item_reference",
        "message",
    ];
    let answer = &["message"];
    assert_rows(
        &recorded,
        6,
        &[
            row(true, true, question, searched),
            row(false, true, replay, answer),
            row(false, true, referenced, answer),
        ],
    );
    assert_replays_name_the_stored_items(&recorded, 6);
}

#[test]
fn openai_accepts_replayed_code_interpreter_calls_inline_and_by_id() {
    let recorded = hosted_calls();
    let ran = &["code_interpreter_call", "message"];
    let question = &["message"];
    let replay = &["message", "code_interpreter_call", "message", "message"];
    let referenced = &["message", "item_reference", "item_reference", "message"];
    let answer = &["message"];
    assert_rows(
        &recorded,
        1,
        &[row(false, true, question, ran), row(false, true, replay, answer)],
    );
    assert_rows(
        &recorded,
        9,
        &[
            row(true, true, question, ran),
            row(false, true, replay, answer),
            row(false, true, referenced, answer),
        ],
    );
    assert_replays_name_the_stored_items(&recorded, 9);
}
