//! Replays the recorded inline text-file scenarios through the gateway against a
//! mock upstream serving the recorded responses. Every turn carries `notes.txt`
//! as an inline `input_file`; its release codeword appears only in the file, so
//! a recorded answer containing it shows that the model read the attachment.
//!
//! The replays check what the recordings cannot see: the gateway sends the
//! model a `File: notes.txt` `input_text` part instead of the file, stored
//! history keeps the client's original `input_file`, a `previous_response_id`
//! continuation decodes the stored file again, and compaction sends the decoded
//! text to the summarizer while the compacted window keeps the file.
//!
//! Both the gateway set (Agentic API -> vLLM) and the `OpenAI` reference set are
//! replayed, and the gateway must build the same model-facing file turn for
//! both; see `cassettes/README.md` "Text file input".

use std::sync::Arc;

use agentic_core::executor::{ExecuteRequest, compact_response, execute};
use agentic_core::types::io::OutputItem;
use agentic_core::types::request_response::{RequestPayload, ResponsePayload};
use agentic_core::{CompactRequest, InputItem};
use either::Either;
use futures::StreamExt;
use serde_json::{Value, json};

mod support;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/text_files/responses");
const NOTES: &str = include_str!("cassettes/text_files/inputs/notes.txt");
const SINGLE_FILE_TURN: &str = include_str!("cassettes/text_files/inputs/single-file.json");
const COMPACT_INPUT: &str = include_str!("cassettes/text_files/inputs/compact-text-file.json");
const MARKER: &str = "PAPAYA-7";
const FOLLOW_UP_PROMPT: &str =
    "What was the release codeword in the file I attached earlier? Reply with only the codeword.";
const COMPACT_FOLLOW_UP_PROMPT: &str = "Reply with only the release codeword from the checkpoint.";
const GATEWAY_MODEL_SLUG: &str = "Qwen-Qwen2.5-3B-Instruct";
const OPENAI_MODEL_SLUG: &str = "gpt-4o";

#[derive(Clone, Copy, Debug)]
enum Provider {
    Gateway,
    OpenAi,
}

impl Provider {
    /// The recording label of a conversation scenario and of the compaction pair.
    /// The gateway's compaction calls are recorded directly against vLLM.
    const fn labels(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Gateway => ("gateway", "vllm", GATEWAY_MODEL_SLUG),
            Self::OpenAi => ("openai", "openai", OPENAI_MODEL_SLUG),
        }
    }

    fn path(self, scenario: &str, compaction: bool, mode: &str) -> String {
        let (conversation, compact, slug) = self.labels();
        let label = if compaction { compact } else { conversation };
        format!("{DIR}/text-file-{scenario}-{label}-{slug}-{mode}.yaml")
    }

    fn load(self, scenario: &str, compaction: bool, mode: &str) -> support::Cassette {
        support::load_cassette(&self.path(scenario, compaction, mode))
    }
}

fn mode(streaming: bool) -> &'static str {
    if streaming { "streaming" } else { "nonstreaming" }
}

fn fixture_json(text: &str) -> Value {
    serde_json::from_str(text).expect("fixture JSON")
}

/// The client's original file part.
fn file_part() -> Value {
    fixture_json(SINGLE_FILE_TURN)[0]["content"][1].clone()
}

/// The part the gateway sends the model in place of the file.
fn inlined_part() -> Value {
    json!({"type": "input_text", "text": format!("File: notes.txt\n{NOTES}")})
}

/// The recorded user turn with its file replaced by the inlined text.
fn model_facing_turn() -> Value {
    let mut turn = fixture_json(SINGLE_FILE_TURN);
    turn[0]["content"][1] = inlined_part();
    turn
}

fn assert_no_file_upstream(requests: &[Value]) {
    for request in requests {
        let encoded = request.to_string();
        assert!(
            !encoded.contains("input_file") && !encoded.contains("file_data"),
            "the model must receive text, not the file part: {encoded}"
        );
    }
}

fn assert_answers_with_marker(response: &ResponsePayload) {
    assert_eq!(response.status, "completed");
    assert!(
        matches!(response.output.as_slice(), [OutputItem::Message(_)]),
        "{:?}",
        response.output
    );
    let text = support::output_text(response);
    assert!(text.contains(MARKER), "recorded answer must contain {MARKER}: {text:?}");
}

/// Every `input_file` part in stored response history, as persisted.
async fn stored_files(fixture: &support::TestFixture) -> Vec<Value> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT data FROM items")
        .fetch_all(fixture.pool.as_ref())
        .await
        .expect("stored items");
    rows.iter()
        .map(|row| serde_json::from_str::<Value>(row).expect("stored item JSON"))
        .filter_map(|item| item["content"].as_array().cloned())
        .flatten()
        .filter(|part| part["type"] == "input_file")
        .collect()
}

fn request_payload(turn: &support::Turn, previous_response_id: Option<&str>) -> RequestPayload {
    let body = &turn.request.body;
    serde_json::from_value(json!({
        "model": body.model,
        "input": body.input,
        "store": body.store,
        "stream": body.stream,
        "max_output_tokens": body.max_output_tokens,
        "previous_response_id": previous_response_id,
    }))
    .expect("recorded request should satisfy the gateway request schema")
}

async fn run_turn(payload: RequestPayload, fixture: &support::TestFixture) -> ResponsePayload {
    let streaming = payload.stream;
    let result = ExecuteRequest::new(payload, Arc::clone(&fixture.exec_ctx))
        .run()
        .await
        .expect("recorded response should replay through the gateway");
    match result {
        Either::Left(response) if !streaming => response,
        Either::Right(stream) if streaming => {
            let chunks: Vec<String> = stream.collect().await;
            let events = support::streamed_sse_events(&chunks);
            let completed = events
                .iter()
                .rev()
                .find(|event| event["type"] == "response.completed")
                .expect("streamed response should complete");
            serde_json::from_value(completed["response"].clone()).expect("completed event carries a response")
        }
        _ => panic!("response mode should match stream={streaming}"),
    }
}

/// Replay one recorded conversation and return the upstream request bodies.
async fn replay_conversation(cassette: &support::Cassette, streaming: bool) -> Vec<Value> {
    let turns = cassette.turns.iter().collect::<Vec<_>>();
    let fixture = support::TestFixture::new(&turns).await;
    assert_eq!(turns[0].request.body.input, fixture_json(SINGLE_FILE_TURN));
    let mut previous_response_id: Option<String> = None;
    for turn in &turns {
        assert_eq!(turn.request.body.stream, streaming);
        let response = run_turn(request_payload(turn, previous_response_id.as_deref()), &fixture).await;
        assert_answers_with_marker(&response);
        previous_response_id = Some(response.id);
    }
    let requests = fixture.request_bodies().await;
    assert_eq!(requests.len(), turns.len(), "each turn reaches the upstream once");
    assert_eq!(
        requests[0]["input"],
        model_facing_turn(),
        "the file reaches the model as File: notes.txt text"
    );
    assert_no_file_upstream(&requests);
    assert_eq!(
        stored_files(&fixture).await,
        vec![file_part()],
        "stored history keeps the client's original input_file"
    );
    requests
}

async fn check_single(provider: Provider, streaming: bool) -> Vec<Value> {
    let cassette = provider.load("single", false, mode(streaming));
    assert_eq!(cassette.turns.len(), 1);
    replay_conversation(&cassette, streaming).await
}

async fn check_continuation(provider: Provider, streaming: bool) -> Vec<Value> {
    let cassette = provider.load("continuation", false, mode(streaming));
    assert_eq!(cassette.turns.len(), 2);
    assert_eq!(cassette.turns[1].request.body.input, json!(FOLLOW_UP_PROMPT));
    let requests = replay_conversation(&cassette, streaming).await;
    let second = &requests[1];
    assert!(
        second.get("previous_response_id").is_none_or(Value::is_null),
        "the gateway rehydrates history itself"
    );
    let history = second["input"].as_array().expect("continuation forwards item history");
    assert_eq!(history.len(), 3, "first turn, model answer, follow-up: {history:?}");
    assert_eq!(
        history[0]["content"],
        model_facing_turn()[0]["content"],
        "continuation decodes the stored file again"
    );
    assert_eq!(history[1]["role"], "assistant");
    assert_eq!(
        support::request_input_texts(second).last().map(String::as_str),
        Some(FOLLOW_UP_PROMPT)
    );
    requests
}

/// Compact the original file, then continue from the compacted window.
async fn check_compaction(provider: Provider) -> Vec<Value> {
    let summary = provider.load("compact", true, "nonstreaming");
    let follow_up = provider.load("compact-followup", true, "nonstreaming");
    assert_eq!(summary.turns[0].request.body.input, fixture_json(COMPACT_INPUT));
    let fixture = support::TestFixture::new_with_responses(vec![
        support::MockResponse::from_turn(&summary.turns[0]),
        support::MockResponse::from_turn(&follow_up.turns[0]),
    ])
    .await;
    let model = summary.turns[0].request.body.model.clone().expect("recorded model");
    let mut source = fixture_json(COMPACT_INPUT)[0].clone();
    source["content"][1] = file_part();
    let request: CompactRequest =
        serde_json::from_value(json!({"model": model, "input": [source]})).expect("valid compact request");

    let compacted = compact_response(request, &fixture.exec_ctx, None)
        .await
        .expect("compact recorded history");
    let summary_text = support::expected_text(&summary.turns[0]);
    assert!(
        summary_text.contains(MARKER),
        "recorded summary must keep {MARKER}: {summary_text:?}"
    );
    let window = serde_json::to_value(&compacted.output).expect("serialize compacted window");
    assert_eq!(
        window[0]["content"][1],
        file_part(),
        "the compacted window keeps the original file"
    );
    assert!(
        matches!(compacted.output.last(), Some(InputItem::Compaction(item)) if item.encrypted_content == summary_text)
    );

    let mut input = compacted.output;
    input.push(
        serde_json::from_value(json!({"type": "message", "role": "user", "content": COMPACT_FOLLOW_UP_PROMPT}))
            .expect("valid follow-up item"),
    );
    let payload: RequestPayload = serde_json::from_value(json!({
        "model": model, "input": input, "stream": false, "store": false,
        "max_output_tokens": follow_up.turns[0].request.body.max_output_tokens,
    }))
    .expect("valid follow-up request");
    let response = support::unwrap_blocking(
        execute(payload, Arc::clone(&fixture.exec_ctx))
            .await
            .expect("execute recorded follow-up"),
    );
    assert_answers_with_marker(&response);

    let requests = fixture.request_bodies().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]["input"], summary.turns[0].request.body.input,
        "the summarizer receives exactly the recorded decoded-text input"
    );
    // Compaction assigns the retained message an id and status; the recording,
    // built from the summary before any gateway ran, carries neither.
    let mut follow_up_input = requests[1]["input"].clone();
    for item in follow_up_input.as_array_mut().expect("follow-up items") {
        let item = item.as_object_mut().expect("follow-up item");
        item.remove("id");
        item.remove("status");
    }
    assert_eq!(
        follow_up_input, follow_up.turns[0].request.body.input,
        "the compacted window reaches the model as recorded, with the file decoded"
    );
    assert_no_file_upstream(&requests);
    requests
}

/// The gateway must build the same model-facing file turn for both providers.
/// Later turns carry provider-specific answers.
fn assert_pair(gateway: &[Value], openai: &[Value]) {
    assert_eq!(gateway.len(), openai.len());
    assert_eq!(gateway[0]["input"], openai[0]["input"]);
    if let (Some(gateway), Some(openai)) = (gateway.get(1), openai.get(1)) {
        assert_eq!(gateway["input"][0], openai["input"][0]);
    }
}

#[test]
fn fixtures_embed_notes_txt() {
    assert!(NOTES.contains(MARKER));
    let turn = fixture_json(SINGLE_FILE_TURN);
    assert!(!turn[0]["content"][0]["text"].as_str().unwrap().contains(MARKER));
    assert_eq!(file_part()["filename"], "notes.txt");
    assert_eq!(fixture_json(COMPACT_INPUT)[0]["content"][1], inlined_part());
}

#[tokio::test]
async fn recorded_single_text_file_turns_reach_the_model_as_text() {
    for streaming in [false, true] {
        let gateway = check_single(Provider::Gateway, streaming).await;
        assert_pair(&gateway, &check_single(Provider::OpenAi, streaming).await);
    }
}

#[tokio::test]
async fn recorded_continuation_reinlines_the_stored_text_file() {
    for streaming in [false, true] {
        let gateway = check_continuation(Provider::Gateway, streaming).await;
        assert_pair(&gateway, &check_continuation(Provider::OpenAi, streaming).await);
    }
}

#[tokio::test]
async fn recorded_compaction_summarizes_decoded_text_and_keeps_the_file() {
    let gateway = check_compaction(Provider::Gateway).await;
    let openai = check_compaction(Provider::OpenAi).await;
    // Summaries differ by model, so only the summary request is compared.
    assert_eq!(gateway[0]["input"], openai[0]["input"]);
}
