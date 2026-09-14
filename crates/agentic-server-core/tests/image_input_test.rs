//! Replays the recorded image-input conversations against `OpenAI` and the
//! gateway and checks that the gateway preserves the `OpenAI` Responses contract:
//! the request an `input_image` part travels in, the shape of the completed
//! response, the streaming event lifecycle, and continuation of an image turn
//! by `previous_response_id`. Model text is never compared — only structure.

use agentic_core::executor::ExecuteRequest;
use agentic_core::types::io::OutputItem;
use agentic_core::types::request_response::{RequestPayload, ResponsePayload};
use either::Either;
use futures::StreamExt;
use serde_json::{Value, json};

mod support;

const CASSETTE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/images/responses");
const IMAGE_PNG: &[u8] = include_bytes!("cassettes/images/inputs/red-blue-64.png");
const IMAGE_TURN: &str = include_str!("cassettes/images/inputs/image-turn.json");
const MODEL: &str = "gpt-4o";
const MODEL_SLUG: &str = "gpt-4o";
const FOLLOW_UP_PROMPT: &str =
    "Without repeating the colors, reply with exactly one word: did my previous message include an image? YES or NO.";
const MAX_OUTPUT_TOKENS: u64 = 64;
const EXPECTED_STREAMING_LIFECYCLE: &[&str] = &[
    "response.created",
    "response.in_progress",
    "response.output_item.added:message",
    "response.content_part.added",
    "response.output_text.delta",
    "response.output_text.done",
    "response.content_part.done",
    "response.output_item.done:message",
    "response.completed",
];

/// Standard base64 without a dependency: the fixture is 136 bytes, and the
/// test only needs to prove the committed JSON turn embeds the committed PNG.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut word = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            word |= u32::from(*byte) << (16 - 8 * index);
        }
        for index in 0..4 {
            if index <= chunk.len() {
                let sextet = usize::try_from((word >> (18 - 6 * index)) & 0x3f).expect("sextet fits usize");
                out.push(char::from(ALPHABET[sextet]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn expected_image_data_url() -> String {
    format!("data:image/png;base64,{}", base64_encode(IMAGE_PNG))
}

fn image_turn_input() -> Value {
    serde_json::from_str(IMAGE_TURN).expect("image-turn.json should be valid JSON")
}

fn image_turn_parts() -> Vec<Value> {
    image_turn_input()[0]["content"]
        .as_array()
        .expect("image turn should carry content parts")
        .clone()
}

fn load_recorded_pair(streaming: bool) -> (support::Cassette, support::Cassette) {
    let mode = if streaming { "streaming" } else { "nonstreaming" };
    let openai = support::load_cassette(&format!(
        "{CASSETTE_DIR}/image-input-openai-reference-{MODEL_SLUG}-{mode}.yaml"
    ));
    let gateway = support::load_cassette(&format!("{CASSETTE_DIR}/image-input-gateway-{MODEL_SLUG}-{mode}.yaml"));
    (openai, gateway)
}

fn terminal_response(turn: &support::Turn) -> Value {
    if let Some(body) = &turn.response.body {
        return body.clone();
    }
    terminal_event_response(&support::recorded_named_sse_events(turn))
}

fn terminal_event_response(events: &[Value]) -> Value {
    events
        .iter()
        .rev()
        .find_map(|event| {
            (event["type"] == "response.completed")
                .then(|| event.get("response").cloned())
                .flatten()
        })
        .expect("stream should contain response.completed")
}

fn message_text(response: &Value) -> String {
    response["output"]
        .as_array()
        .expect("completed response should contain output")
        .iter()
        .filter(|item| item["type"] == "message")
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == "output_text")
        .filter_map(|part| part["text"].as_str())
        .collect()
}

fn assert_request_contract(openai: &support::Cassette, gateway: &support::Cassette, streaming: bool) {
    assert_eq!(
        openai.turns.len(),
        2,
        "the OpenAI recording should hold the image turn and its follow-up"
    );
    assert_eq!(
        gateway.turns.len(),
        2,
        "the gateway recording should hold the image turn and its follow-up"
    );

    for (index, (openai_turn, gateway_turn)) in openai.turns.iter().zip(&gateway.turns).enumerate() {
        let openai_request = &openai_turn.request;
        let gateway_request = &gateway_turn.request;
        assert_eq!(openai_request.path, "/v1/responses");
        assert_eq!(gateway_request.path, openai_request.path);
        assert_eq!(openai_request.body.model.as_deref(), Some(MODEL));
        assert_eq!(gateway_request.body.model, openai_request.body.model);
        assert!(
            openai_request.body.store,
            "continuation by previous_response_id needs stored turns"
        );
        assert_eq!(gateway_request.body.store, openai_request.body.store);
        assert_eq!(openai_request.body.stream, streaming);
        assert_eq!(gateway_request.body.stream, openai_request.body.stream);
        assert_eq!(openai_request.body.max_output_tokens, Some(MAX_OUTPUT_TOKENS));
        assert_eq!(
            gateway_request.body.max_output_tokens,
            openai_request.body.max_output_tokens
        );
        assert_eq!(
            gateway_request.body.input,
            openai_request.body.input,
            "turn {} must carry the same input to OpenAI and to the gateway",
            index + 1
        );
        assert_eq!(gateway_request.body.tools, openai_request.body.tools);
        assert_eq!(gateway_request.body.extra, openai_request.body.extra);
    }

    let image_turn = &openai.turns[0].request.body;
    assert_eq!(
        image_turn.input,
        image_turn_input(),
        "the recorded image turn must be exactly the committed fixture"
    );
    assert_eq!(image_turn.previous_response_id, None);

    let follow_up = &openai.turns[1].request.body;
    assert_eq!(follow_up.input, Value::String(FOLLOW_UP_PROMPT.to_owned()));
    for cassette in [openai, gateway] {
        let first_id = terminal_response(&cassette.turns[0])["id"]
            .as_str()
            .expect("first response should carry an id")
            .to_owned();
        assert_eq!(
            cassette.turns[1].request.body.previous_response_id.as_deref(),
            Some(first_id.as_str()),
            "the follow-up must continue the image turn by previous_response_id"
        );
    }
}

fn assert_terminal_contract(turn: &support::Turn) -> Value {
    let response = terminal_response(turn);
    assert_eq!(response["object"], "response");
    assert_eq!(response["status"], "completed");
    assert!(response["id"].as_str().is_some_and(|id| !id.is_empty()));
    let output = response["output"]
        .as_array()
        .expect("completed response should contain output");
    let messages = output
        .iter()
        .filter(|item| item["type"] == "message")
        .collect::<Vec<_>>();
    assert_eq!(
        messages.len(),
        1,
        "an image question should produce exactly one message"
    );
    let message = messages[0];
    assert_eq!(message["role"], "assistant");
    assert_eq!(message["status"], "completed");
    assert!(
        message["content"]
            .as_array()
            .is_some_and(|parts| parts.iter().any(|part| part["type"] == "output_text")),
        "message should carry an output_text part"
    );
    assert!(!message_text(&response).trim().is_empty(), "message should carry text");
    assert!(
        response["usage"]["input_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0)
    );
    assert!(
        response["usage"]["output_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0)
    );
    response
}

fn lifecycle_event_name(event: &Value) -> String {
    let event_type = event["type"].as_str().expect("stream event should contain a type");
    if matches!(event_type, "response.output_item.added" | "response.output_item.done") {
        let item_type = event["item"]["type"]
            .as_str()
            .expect("output-item lifecycle event should contain an item type");
        format!("{event_type}:{item_type}")
    } else {
        event_type.to_owned()
    }
}

/// Collapse runs of text deltas so the lifecycle does not depend on how the
/// model happened to chunk its answer.
fn normalized_streaming_lifecycle(events: &[Value]) -> Vec<String> {
    let mut lifecycle: Vec<String> = Vec::new();
    for event in events {
        let event_name = lifecycle_event_name(event);
        if event_name == "response.output_text.delta" && lifecycle.last() == Some(&event_name) {
            continue;
        }
        lifecycle.push(event_name);
    }
    lifecycle
}

fn assert_stream_events_contract(events: &[Value]) -> Vec<String> {
    let sequence_numbers = events
        .iter()
        .map(|event| {
            event["sequence_number"]
                .as_u64()
                .expect("every stream event should contain a sequence number")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        sequence_numbers,
        (0..u64::try_from(events.len()).expect("stream length should fit in u64")).collect::<Vec<_>>(),
        "stream sequence numbers should be unique and contiguous"
    );

    let lifecycle = normalized_streaming_lifecycle(events);
    assert_eq!(lifecycle, EXPECTED_STREAMING_LIFECYCLE);

    let added = events
        .iter()
        .find(|event| event["type"] == "response.output_item.added")
        .expect("stream should add the message item");
    let item_id = added["item"]["id"].as_str().expect("message item should carry an id");
    for event in events {
        let event_item_id = event["item_id"].as_str().or_else(|| event["item"]["id"].as_str());
        if event_item_id == Some(item_id) {
            assert_eq!(
                event["output_index"].as_u64(),
                Some(0),
                "every message lifecycle event should keep output index 0"
            );
        }
    }

    let delta_text = events
        .iter()
        .filter(|event| event["type"] == "response.output_text.delta")
        .map(|event| event["delta"].as_str().expect("text delta should contain delta text"))
        .collect::<String>();
    assert!(!delta_text.is_empty(), "output_text deltas should contain text");
    let done_events = events
        .iter()
        .filter(|event| event["type"] == "response.output_text.done")
        .collect::<Vec<_>>();
    assert_eq!(done_events.len(), 1);
    assert_eq!(done_events[0]["text"].as_str(), Some(delta_text.as_str()));

    let terminal = terminal_event_response(events);
    assert_eq!(
        message_text(&terminal),
        delta_text,
        "the completed response should carry the streamed text"
    );
    lifecycle
}

fn assert_streaming_contract(turn: &support::Turn) -> Vec<String> {
    assert_stream_events_contract(&support::recorded_named_sse_events(turn))
}

fn request_payload(turn: &support::Turn, previous_response_id: Option<&str>) -> RequestPayload {
    serde_json::from_value(json!({
        "model": turn.request.body.model,
        "input": turn.request.body.input,
        "store": turn.request.body.store,
        "stream": turn.request.body.stream,
        "max_output_tokens": turn.request.body.max_output_tokens,
        "previous_response_id": previous_response_id,
    }))
    .expect("recorded request should satisfy the gateway request schema")
}

struct ReplayedTurn {
    response: ResponsePayload,
    /// Every SSE event the gateway emitted; empty for a non-streaming replay.
    events: Vec<Value>,
}

async fn execute(payload: RequestPayload, fixture: &support::TestFixture) -> ReplayedTurn {
    let streaming = payload.stream;
    let result = ExecuteRequest::new(payload, fixture.exec_ctx.clone())
        .run()
        .await
        .expect("recorded response should replay through the gateway");
    if !streaming {
        return ReplayedTurn {
            response: support::unwrap_blocking(result),
            events: Vec::new(),
        };
    }
    let Either::Right(stream) = result else {
        panic!("streaming request should return a stream");
    };
    let chunks: Vec<String> = stream.collect().await;
    let events = support::streamed_sse_events(&chunks);
    let response =
        serde_json::from_value(terminal_event_response(&events)).expect("completed event should carry a response");
    ReplayedTurn { response, events }
}

/// Replay both recorded turns through the gateway against a mock upstream that
/// serves the recorded responses, continuing the second turn from the id the
/// gateway itself assigned to the first.
async fn replay_conversation(cassette: &support::Cassette) -> (ReplayedTurn, ReplayedTurn, Vec<Value>) {
    let fixture = support::TestFixture::new(&[&cassette.turns[0], &cassette.turns[1]]).await;
    let first = execute(request_payload(&cassette.turns[0], None), &fixture).await;
    let second = execute(request_payload(&cassette.turns[1], Some(&first.response.id)), &fixture).await;
    let requests = fixture.request_bodies().await;
    assert_eq!(requests.len(), 2, "each turn should reach the upstream exactly once");
    (first, second, requests)
}

fn assert_replayed_response(turn: &ReplayedTurn, streaming: bool) {
    assert_eq!(turn.response.status, "completed");
    let messages = turn
        .response
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::Message(_)))
        .count();
    assert_eq!(messages, 1, "gateway replay should surface exactly one message");
    assert!(!support::output_text(&turn.response).trim().is_empty());
    if streaming {
        assert_stream_events_contract(&turn.events);
    } else {
        assert!(turn.events.is_empty());
    }
}

fn assert_upstream_requests(requests: &[Value], first: &ReplayedTurn, streaming: bool) {
    let image_turn = &requests[0];
    assert_eq!(image_turn["stream"], streaming);
    assert_eq!(
        image_turn["input"],
        image_turn_input(),
        "the image turn must reach the upstream exactly as the client sent it"
    );

    let follow_up = &requests[1];
    assert_eq!(follow_up["stream"], streaming);
    assert!(
        follow_up.get("previous_response_id").is_none_or(Value::is_null),
        "the gateway rehydrates history itself instead of forwarding its own response id"
    );
    let history = follow_up["input"]
        .as_array()
        .expect("continuation should forward rehydrated item history");
    assert_eq!(
        history.len(),
        3,
        "history should be: image turn, assistant reply, follow-up"
    );

    assert_eq!(history[0]["role"], "user");
    assert_eq!(
        history[0]["content"],
        image_turn_input()[0]["content"],
        "the stored image turn must rehydrate with the image part intact and in order"
    );

    assert_eq!(history[1]["type"], "message");
    assert_eq!(history[1]["role"], "assistant");
    let assistant_text = history[1]["content"]
        .as_array()
        .expect("assistant history should carry content parts")
        .iter()
        .filter(|part| part["type"] == "output_text")
        .filter_map(|part| part["text"].as_str())
        .collect::<String>();
    assert_eq!(assistant_text, support::output_text(&first.response));

    assert_eq!(history[2]["role"], "user");
    assert_eq!(
        support::request_input_texts(follow_up).last().map(String::as_str),
        Some(FOLLOW_UP_PROMPT)
    );
}

#[test]
fn image_fixture_embeds_the_committed_png() {
    let parts = image_turn_parts();
    assert_eq!(
        parts.iter().map(|part| &part["type"]).collect::<Vec<_>>(),
        vec!["input_text", "input_image"]
    );
    assert_eq!(parts[1]["image_url"], expected_image_data_url());
    assert_eq!(parts[1]["detail"], "low");
}

#[tokio::test]
async fn recorded_nonstreaming_image_input_matches_openai_contract() {
    let (openai, gateway) = load_recorded_pair(false);
    assert_request_contract(&openai, &gateway, false);
    for cassette in [&openai, &gateway] {
        for turn in &cassette.turns {
            assert_terminal_contract(turn);
        }
    }

    for cassette in [&openai, &gateway] {
        let (first, second, requests) = replay_conversation(cassette).await;
        assert_replayed_response(&first, false);
        assert_replayed_response(&second, false);
        assert_upstream_requests(&requests, &first, false);
    }
}

#[tokio::test]
async fn recorded_streaming_image_input_matches_openai_contract() {
    let (openai, gateway) = load_recorded_pair(true);
    assert_request_contract(&openai, &gateway, true);
    for cassette in [&openai, &gateway] {
        for turn in &cassette.turns {
            assert_terminal_contract(turn);
            let lifecycle = assert_streaming_contract(turn);
            assert_eq!(lifecycle, EXPECTED_STREAMING_LIFECYCLE);
        }
    }

    for cassette in [&openai, &gateway] {
        let (first, second, requests) = replay_conversation(cassette).await;
        assert_replayed_response(&first, true);
        assert_replayed_response(&second, true);
        assert_upstream_requests(&requests, &first, true);
    }
}
