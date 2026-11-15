//! Recorded `OpenAI` behavior for replayed gateway call items (issue #492): `mcp_call`, `web_search_call` and
//! `code_interpreter_call` sent back with `store: false`, inline and by `id`.
//!
//! Legs are documented in `record_replayed_gateway_calls_cassettes.sh`. These cassettes pin the reference contract
//! the gateway's model-input lowering follows; the gateway recordings for the same legs are not committed yet.

use serde_json::Value;

mod support;

const CASSETTE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/replayed_calls");
const OPENAI: &str = "openai-reference-gpt-5.6-sol";

fn cassette(kind: &str, leg: &str) -> support::Cassette {
    support::load_cassette(&format!(
        "{CASSETTE_DIR}/replayed-calls-{kind}-{leg}-{OPENAI}-nonstreaming.yaml"
    ))
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

fn assert_rows(kind: &str, leg: &str, rows: &[Row]) {
    let recorded = cassette(kind, leg);
    assert_eq!(
        recorded.turns.len(),
        rows.len(),
        "{kind} {leg}: see the recorder header"
    );
    for (index, (turn, row)) in recorded.turns.iter().zip(rows).enumerate() {
        let label = format!("{kind} {leg} t{}", index + 1);
        let body = turn.response.body.as_ref().expect("non-streaming response body");
        assert_eq!(turn.response.status_code, Some(200), "{label}: {body}");
        assert_eq!(body["status"], "completed", "{label}");
        assert_eq!(turn.request.body.store, row.store, "{label}");
        assert_eq!(!turn.request.body.tools.is_empty(), row.declares_tool, "{label}");
        assert_eq!(item_types(&turn.request.body.input), row.sent, "{label}");
        assert_eq!(item_types(&body["output"]), row.returned, "{label}");
    }
}

/// Every by-`id` replay names the first turn's output items: as returned with their ids, then as `item_reference`.
fn assert_replays_name_the_stored_items(kind: &str) {
    let recorded = cassette(kind, "by-id");
    let stored: Vec<&Value> = recorded.turns[0].response.body.as_ref().expect("stored response")["output"]
        .as_array()
        .expect("output items")
        .iter()
        .map(|item| &item["id"])
        .collect();
    for turn in &recorded.turns[1..] {
        let sent = turn.request.body.input.as_array().expect("replayed items");
        // The first item is the original question and the last the follow-up.
        let replayed: Vec<&Value> = sent[1..sent.len() - 1].iter().map(|item| &item["id"]).collect();
        assert_eq!(replayed, stored, "{kind}");
        assert_eq!(
            turn.request.body.tool_choice.as_ref(),
            Some(&Value::from("none")),
            "{kind}"
        );
    }
}

#[test]
fn openai_accepts_replayed_mcp_calls_inline_and_by_id() {
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
        "mcp",
        "inline",
        &[row(false, true, question, listed), row(false, true, replay, answer)],
    );
    assert_rows(
        "mcp",
        "by-id",
        &[
            row(true, true, question, listed),
            row(false, true, replay, answer),
            row(false, true, referenced, answer),
        ],
    );
    assert_replays_name_the_stored_items("mcp");
}

#[test]
fn openai_accepts_a_replayed_mcp_call_without_its_listing_or_any_tools() {
    // Without the listing, the server's tools are listed again; the call itself is accepted.
    assert_rows(
        "mcp",
        "no-listing",
        &[row(
            false,
            true,
            &["message", "mcp_call", "message", "message"],
            &["mcp_list_tools", "message"],
        )],
    );
    // A request that declares no tools, as a compaction summary does.
    assert_rows(
        "mcp",
        "no-tools",
        &[row(
            false,
            false,
            &["message", "mcp_list_tools", "mcp_call", "message", "message"],
            &["message"],
        )],
    );
}

#[test]
fn openai_accepts_replayed_web_search_calls_inline_and_by_id() {
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
        "web-search",
        "inline",
        &[row(false, true, question, searched), row(false, true, replay, answer)],
    );
    assert_rows(
        "web-search",
        "by-id",
        &[
            row(true, true, question, searched),
            row(false, true, replay, answer),
            row(false, true, referenced, answer),
        ],
    );
    assert_replays_name_the_stored_items("web-search");
}

#[test]
fn openai_accepts_replayed_code_interpreter_calls_inline_by_id_and_without_tools() {
    let ran = &["code_interpreter_call", "message"];
    let question = &["message"];
    let replay = &["message", "code_interpreter_call", "message", "message"];
    let referenced = &["message", "item_reference", "item_reference", "message"];
    let answer = &["message"];
    assert_rows(
        "code-interpreter",
        "inline",
        &[row(false, true, question, ran), row(false, true, replay, answer)],
    );
    assert_rows(
        "code-interpreter",
        "by-id",
        &[
            row(true, true, question, ran),
            row(false, true, replay, answer),
            row(false, true, referenced, answer),
        ],
    );
    assert_replays_name_the_stored_items("code-interpreter");
    assert_rows("code-interpreter", "no-tools", &[row(false, false, replay, answer)]);
}
