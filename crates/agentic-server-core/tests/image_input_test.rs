//! Replays the recorded image-input scenarios and checks that the gateway path
//! (client -> Agentic API -> vLLM hosting an open-source vision model) preserves
//! the `OpenAI` Responses contract of the reference path (client -> `OpenAI`):
//! the request an image travels in, the shape of the completed response, the
//! streaming event lifecycle, continuation by `previous_response_id`, and a
//! client-executed tool returning an image. Both providers receive the same
//! image bytes, prompts, and tool definitions; only the model name differs, and
//! model wording and token counts are never compared.

use agentic_core::executor::ExecuteRequest;
use agentic_core::types::io::OutputItem;
use agentic_core::types::request_response::{RequestPayload, ResponsePayload};
use either::Either;
use futures::StreamExt;
use serde_json::{Value, json};

mod support;

const CASSETTE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/images/responses");
const RED_BLUE_PNG: &[u8] = include_bytes!("cassettes/images/inputs/red-blue-64.png");
const GREEN_YELLOW_PNG: &[u8] = include_bytes!("cassettes/images/inputs/green-yellow-64.png");
const SINGLE_IMAGE_TURN: &str = include_str!("cassettes/images/inputs/single-image.json");
const MULTI_IMAGE_TURN: &str = include_str!("cassettes/images/inputs/multi-image.json");
const VIEW_IMAGE_TOOL: &str = include_str!("cassettes/images/inputs/view_image_tool.json");
const OPENAI_MODEL: &str = "gpt-4o";
const OPENAI_MODEL_SLUG: &str = "gpt-4o";
const GATEWAY_MODEL: &str = "Qwen/Qwen2.5-VL-3B-Instruct";
const GATEWAY_MODEL_SLUG: &str = "Qwen-Qwen2.5-VL-3B-Instruct";
const FOLLOW_UP_PROMPT: &str =
    "Without repeating the colors, reply with exactly one word: did my previous message include an image? YES or NO.";
const TOOL_PROMPT: &str = "Call the view_image tool exactly once, with path \"diagram.png\". After you see the image, \
                           reply with exactly two words: the color on its left half, then the color on its right half.";
const MAX_OUTPUT_TOKENS: u64 = 64;
const MESSAGE_LIFECYCLE: &[&str] = &[
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
const FUNCTION_CALL_LIFECYCLE: &[&str] = &[
    "response.created",
    "response.in_progress",
    "response.output_item.added:function_call",
    "response.function_call_arguments.delta",
    "response.function_call_arguments.done",
    "response.output_item.done:function_call",
    "response.completed",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    SingleImage,
    MultiImage,
    Continuation,
    ToolImage,
}

impl Scenario {
    const ALL: [Self; 4] = [Self::SingleImage, Self::MultiImage, Self::Continuation, Self::ToolImage];

    const fn name(self) -> &'static str {
        match self {
            Self::SingleImage => "single-image",
            Self::MultiImage => "multi-image",
            Self::Continuation => "continuation",
            Self::ToolImage => "tool-image",
        }
    }

    const fn turns(self) -> usize {
        match self {
            Self::SingleImage | Self::MultiImage => 1,
            Self::Continuation | Self::ToolImage => 2,
        }
    }

    /// The output item each turn must produce, in order.
    const fn expected_items(self) -> &'static [&'static str] {
        match self {
            Self::SingleImage | Self::MultiImage => &["message"],
            Self::Continuation => &["message", "message"],
            Self::ToolImage => &["function_call", "message"],
        }
    }

    /// The image data URLs the first turn carries, in order.
    fn image_urls(self) -> Vec<String> {
        match self {
            Self::SingleImage | Self::Continuation => vec![data_url(RED_BLUE_PNG)],
            Self::MultiImage => vec![data_url(RED_BLUE_PNG), data_url(GREEN_YELLOW_PNG)],
            Self::ToolImage => Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Provider {
    OpenAi,
    Gateway,
}

impl Provider {
    const fn model(self) -> &'static str {
        match self {
            Self::OpenAi => OPENAI_MODEL,
            Self::Gateway => GATEWAY_MODEL,
        }
    }

    fn cassette_path(self, scenario: Scenario, streaming: bool) -> String {
        let (name, slug) = match self {
            Self::OpenAi => ("openai", OPENAI_MODEL_SLUG),
            Self::Gateway => ("gateway", GATEWAY_MODEL_SLUG),
        };
        let mode = if streaming { "streaming" } else { "nonstreaming" };
        format!("{CASSETTE_DIR}/image-{}-{name}-{slug}-{mode}.yaml", scenario.name())
    }
}

/// Standard base64 without a dependency: the fixtures are a few hundred bytes,
/// and the test only needs to prove the committed JSON turns embed the
/// committed PNGs.
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

fn data_url(png: &[u8]) -> String {
    format!("data:image/png;base64,{}", base64_encode(png))
}

fn fixture_json(text: &str) -> Value {
    serde_json::from_str(text).expect("fixture should be valid JSON")
}

fn first_turn_input(scenario: Scenario) -> Value {
    match scenario {
        Scenario::SingleImage | Scenario::Continuation => fixture_json(SINGLE_IMAGE_TURN),
        Scenario::MultiImage => fixture_json(MULTI_IMAGE_TURN),
        Scenario::ToolImage => Value::String(TOOL_PROMPT.to_owned()),
    }
}

fn view_image_tools() -> Vec<Value> {
    fixture_json(VIEW_IMAGE_TOOL)
        .as_array()
        .expect("tool fixture should be an array")
        .clone()
}

/// The `input_image` URLs inside a message's content parts, in order.
fn image_urls_of(content: &Value) -> Vec<String> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "input_image")
        .filter_map(|part| part["image_url"].as_str().map(str::to_owned))
        .collect()
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

fn terminal_response(turn: &support::Turn) -> Value {
    if let Some(body) = &turn.response.body {
        return body.clone();
    }
    terminal_event_response(&support::recorded_named_sse_events(turn))
}

fn output_types(response: &Value) -> Vec<String> {
    response["output"]
        .as_array()
        .expect("completed response should contain output")
        .iter()
        .filter_map(|item| item["type"].as_str().map(str::to_owned))
        .collect()
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

fn function_call(response: &Value) -> &Value {
    let calls = response["output"]
        .as_array()
        .expect("completed response should contain output")
        .iter()
        .filter(|item| item["type"] == "function_call")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1, "the tool turn should produce exactly one function call");
    calls[0]
}

/// A recorded tool-output turn with the provider-specific `call_id` removed,
/// so both providers' second turns can be compared as one input.
fn without_call_ids(input: &Value) -> Value {
    let mut input = input.clone();
    if let Some(items) = input.as_array_mut() {
        for item in items {
            if let Some(object) = item.as_object_mut() {
                object.remove("call_id");
            }
        }
    }
    input
}

struct Pair {
    scenario: Scenario,
    streaming: bool,
    openai: support::Cassette,
    gateway: support::Cassette,
}

fn load_pair(scenario: Scenario, streaming: bool) -> Pair {
    Pair {
        scenario,
        streaming,
        openai: support::load_cassette(&Provider::OpenAi.cassette_path(scenario, streaming)),
        gateway: support::load_cassette(&Provider::Gateway.cassette_path(scenario, streaming)),
    }
}

/// Both providers must have received equivalent requests: the same image
/// bytes, prompts, tool declarations, and Responses settings. Only the model
/// name and the provider-assigned identifiers may differ.
fn assert_request_contract(pair: &Pair) {
    let Pair {
        scenario,
        streaming,
        openai,
        gateway,
    } = pair;
    assert_eq!(
        openai.turns.len(),
        scenario.turns(),
        "OpenAI turn count for {}",
        scenario.name()
    );
    assert_eq!(
        gateway.turns.len(),
        scenario.turns(),
        "gateway turn count for {}",
        scenario.name()
    );

    for (index, (openai_turn, gateway_turn)) in openai.turns.iter().zip(&gateway.turns).enumerate() {
        let openai_body = &openai_turn.request.body;
        let gateway_body = &gateway_turn.request.body;
        assert_eq!(openai_turn.request.path, "/v1/responses");
        assert_eq!(gateway_turn.request.path, openai_turn.request.path);
        assert_eq!(openai_body.model.as_deref(), Some(Provider::OpenAi.model()));
        assert_eq!(gateway_body.model.as_deref(), Some(Provider::Gateway.model()));
        assert!(openai_body.store, "stored turns are needed for continuation");
        assert_eq!(gateway_body.store, openai_body.store);
        assert_eq!(openai_body.stream, *streaming);
        assert_eq!(gateway_body.stream, openai_body.stream);
        assert_eq!(openai_body.max_output_tokens, Some(MAX_OUTPUT_TOKENS));
        assert_eq!(gateway_body.max_output_tokens, openai_body.max_output_tokens);
        assert_eq!(
            without_call_ids(&gateway_body.input),
            without_call_ids(&openai_body.input),
            "{} turn {} must carry the same input to both providers",
            scenario.name(),
            index + 1
        );
        assert_eq!(gateway_body.tools, openai_body.tools);
        assert_eq!(gateway_body.tool_choice, openai_body.tool_choice);
        assert_eq!(gateway_body.extra, openai_body.extra);
    }

    for cassette in [openai, gateway] {
        let first = &cassette.turns[0];
        assert_eq!(first.request.body.input, first_turn_input(*scenario));
        assert_eq!(first.request.body.previous_response_id, None);
        assert_eq!(
            image_urls_of(&first.request.body.input[0]["content"]),
            scenario.image_urls()
        );
        if *scenario == Scenario::ToolImage {
            assert_eq!(first.request.body.tools, view_image_tools());
            assert_eq!(first.request.body.tool_choice, Some(json!("auto")));
        } else {
            assert!(first.request.body.tools.is_empty());
        }

        if scenario.turns() == 2 {
            let first_response = terminal_response(first);
            let second = &cassette.turns[1];
            assert_eq!(
                second.request.body.previous_response_id.as_deref(),
                first_response["id"].as_str(),
                "the second turn must continue the first by previous_response_id"
            );
            match scenario {
                Scenario::Continuation => {
                    assert_eq!(second.request.body.input, Value::String(FOLLOW_UP_PROMPT.to_owned()));
                }
                Scenario::ToolImage => {
                    let call = function_call(&first_response);
                    let items = second.request.body.input.as_array().expect("tool output items");
                    assert_eq!(items.len(), 1, "the client submits only the tool output");
                    assert_eq!(items[0]["type"], "function_call_output");
                    assert_eq!(items[0]["call_id"], call["call_id"]);
                    assert_tool_output_carries_image(&items[0]["output"]);
                    assert_eq!(second.request.body.tools, view_image_tools());
                }
                Scenario::SingleImage | Scenario::MultiImage => unreachable!("single-turn scenario"),
            }
        }
    }
}

fn assert_tool_output_carries_image(output: &Value) {
    let parts = output
        .as_array()
        .expect("a tool returning an image submits a content array, not a string");
    assert_eq!(
        parts.iter().map(|part| &part["type"]).collect::<Vec<_>>(),
        vec!["input_text", "input_image"]
    );
    assert_eq!(parts[1]["image_url"], data_url(RED_BLUE_PNG));
    assert_eq!(parts[1]["detail"], "low");
}

fn assert_terminal_contract(
    turn: &support::Turn,
    expected_item: &str,
    provider: Provider,
    previous_response_id: Option<&str>,
) -> Value {
    let response = terminal_response(turn);
    assert_eq!(response["object"], "response");
    assert_eq!(response["status"], "completed");
    assert!(response["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(response["created_at"].as_u64().is_some_and(|created| created > 0));
    assert!(response["error"].is_null(), "a completed response carries no error");
    assert!(response["incomplete_details"].is_null());
    let model = response["model"].as_str().expect("response should name its model");
    match provider {
        // OpenAI answers with the dated snapshot of the requested alias.
        Provider::OpenAi => assert!(
            model.starts_with(provider.model()),
            "{model} should be a {} snapshot",
            provider.model()
        ),
        Provider::Gateway => assert_eq!(model, provider.model()),
    }
    assert_eq!(
        response["previous_response_id"].as_str(),
        previous_response_id,
        "the response must echo the continuation it was asked for"
    );
    assert_eq!(output_types(&response), vec![expected_item]);
    match expected_item {
        "message" => {
            let message = &response["output"][0];
            assert_eq!(message["role"], "assistant");
            assert_eq!(message["status"], "completed");
            assert!(
                message["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|part| part["type"] == "output_text")),
                "message should carry an output_text part"
            );
            assert!(!message_text(&response).trim().is_empty(), "message should carry text");
        }
        "function_call" => {
            let call = function_call(&response);
            assert_eq!(call["name"], "view_image");
            assert_eq!(call["status"], "completed");
            assert!(call["call_id"].as_str().is_some_and(|id| !id.is_empty()));
            let arguments: Value = serde_json::from_str(call["arguments"].as_str().expect("arguments string"))
                .expect("function call arguments should be a JSON object");
            assert_eq!(arguments["path"], "diagram.png");
        }
        other => panic!("unexpected output item {other}"),
    }
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

/// Collapse runs of deltas so the lifecycle does not depend on how a model
/// happened to chunk its output.
fn normalized_streaming_lifecycle(events: &[Value]) -> Vec<String> {
    let mut lifecycle: Vec<String> = Vec::new();
    for event in events {
        let event_name = lifecycle_event_name(event);
        let is_delta = matches!(
            event_name.as_str(),
            "response.output_text.delta" | "response.function_call_arguments.delta"
        );
        if is_delta && lifecycle.last() == Some(&event_name) {
            continue;
        }
        lifecycle.push(event_name);
    }
    lifecycle
}

fn expected_lifecycle(expected_item: &str) -> &'static [&'static str] {
    match expected_item {
        "message" => MESSAGE_LIFECYCLE,
        "function_call" => FUNCTION_CALL_LIFECYCLE,
        other => panic!("unexpected output item {other}"),
    }
}

fn assert_stream_events_contract(events: &[Value], expected_item: &str) {
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
    assert_eq!(
        normalized_streaming_lifecycle(events),
        expected_lifecycle(expected_item)
    );

    // One response, one output item, one content part: every event that names
    // an identifier or index must agree with the first one that introduced it.
    let response_id = events[0]["response"]["id"]
        .as_str()
        .expect("response.created should carry the response id");
    let added = events
        .iter()
        .find(|event| event["type"] == "response.output_item.added")
        .expect("stream should add the output item");
    let item_id = added["item"]["id"].as_str().expect("output item should carry an id");
    assert!(!item_id.is_empty());
    for event in events {
        if let Some(response) = event.get("response") {
            assert_eq!(
                response["id"], response_id,
                "{}: response id must not change",
                event["type"]
            );
        }
        if let Some(item) = event.get("item") {
            assert_eq!(item["id"], item_id, "{}: output item id must not change", event["type"]);
            assert_eq!(
                item["type"], expected_item,
                "{}: output item type must not change",
                event["type"]
            );
        }
        if let Some(event_item_id) = event.get("item_id") {
            assert_eq!(
                event_item_id, item_id,
                "{}: item_id must name the added item",
                event["type"]
            );
        }
        if let Some(output_index) = event.get("output_index") {
            assert_eq!(output_index, 0, "{}: the only item keeps output index 0", event["type"]);
        }
        if let Some(content_index) = event.get("content_index") {
            assert_eq!(
                content_index, 0,
                "{}: the only part keeps content index 0",
                event["type"]
            );
        }
    }
    let terminal = terminal_event_response(events);
    assert_eq!(
        terminal["output"][0]["id"], item_id,
        "the completed output must be the streamed item"
    );

    let (delta_type, done_type, done_field) = match expected_item {
        "message" => ("response.output_text.delta", "response.output_text.done", "text"),
        "function_call" => (
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "arguments",
        ),
        other => panic!("unexpected output item {other}"),
    };
    let delta_text = events
        .iter()
        .filter(|event| event["type"] == delta_type)
        .map(|event| event["delta"].as_str().expect("delta event should contain delta text"))
        .collect::<String>();
    assert!(!delta_text.is_empty(), "{delta_type} events should contain text");
    let done_events = events
        .iter()
        .filter(|event| event["type"] == done_type)
        .collect::<Vec<_>>();
    assert_eq!(done_events.len(), 1);
    assert_eq!(done_events[0][done_field].as_str(), Some(delta_text.as_str()));

    let terminal_text = match expected_item {
        "message" => message_text(&terminal),
        _ => function_call(&terminal)["arguments"]
            .as_str()
            .expect("arguments string")
            .to_owned(),
    };
    assert_eq!(
        terminal_text, delta_text,
        "the completed response should carry the streamed text"
    );
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
        "tools": (!body.tools.is_empty()).then_some(&body.tools),
        "tool_choice": body.tool_choice,
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

/// Replay every recorded turn through the gateway against a mock upstream that
/// serves the recorded responses, continuing the second turn from the id the
/// gateway itself assigned to the first.
async fn replay(cassette: &support::Cassette) -> (Vec<ReplayedTurn>, Vec<Value>) {
    let turns = cassette.turns.iter().collect::<Vec<_>>();
    let fixture = support::TestFixture::new(&turns).await;
    let mut replayed = Vec::with_capacity(turns.len());
    let mut previous_response_id: Option<String> = None;
    for turn in &turns {
        let replayed_turn = execute(request_payload(turn, previous_response_id.as_deref()), &fixture).await;
        previous_response_id = Some(replayed_turn.response.id.clone());
        replayed.push(replayed_turn);
    }
    let requests = fixture.request_bodies().await;
    assert_eq!(
        requests.len(),
        turns.len(),
        "each turn should reach the upstream exactly once"
    );
    (replayed, requests)
}

fn replayed_output_types(response: &ResponsePayload) -> Vec<&'static str> {
    response
        .output
        .iter()
        .map(|item| match item {
            OutputItem::Message(_) => "message",
            OutputItem::FunctionCall(_) => "function_call",
            _ => "other",
        })
        .collect()
}

fn assert_replayed_turns(scenario: Scenario, replayed: &[ReplayedTurn], streaming: bool) {
    for (turn, expected_item) in replayed.iter().zip(scenario.expected_items()) {
        assert_eq!(turn.response.status, "completed");
        assert_eq!(replayed_output_types(&turn.response), vec![*expected_item]);
        if streaming {
            assert_stream_events_contract(&turn.events, expected_item);
        } else {
            assert!(turn.events.is_empty());
        }
    }
}

/// What the gateway forwarded upstream: the first turn exactly as the client
/// sent it, and the second turn as rehydrated history with every image part
/// intact and in order.
fn assert_upstream_requests(scenario: Scenario, requests: &[Value], replayed: &[ReplayedTurn], streaming: bool) {
    let first = &requests[0];
    assert_eq!(first["stream"], streaming);
    if scenario == Scenario::ToolImage {
        // A tool turn runs through the typed executor, which lifts a string
        // input into the equivalent user message item before forwarding.
        assert_eq!(
            first["input"],
            json!([{"type": "message", "role": "user", "content": TOOL_PROMPT}])
        );
        assert_eq!(first["tools"], Value::Array(view_image_tools()));
    } else {
        assert_eq!(
            first["input"],
            first_turn_input(scenario),
            "the image turn reaches the upstream exactly as the client sent it"
        );
    }
    if scenario.turns() == 1 {
        return;
    }

    let second = &requests[1];
    assert_eq!(second["stream"], streaming);
    assert!(
        second.get("previous_response_id").is_none_or(Value::is_null),
        "the gateway rehydrates history itself instead of forwarding its own response id"
    );
    let history = second["input"]
        .as_array()
        .expect("continuation should forward rehydrated item history");
    assert_eq!(
        history.len(),
        3,
        "history should be: first turn, model output, client turn"
    );
    match scenario {
        Scenario::Continuation => {
            assert_eq!(history[0]["role"], "user");
            assert_eq!(
                history[0]["content"],
                first_turn_input(scenario)[0]["content"],
                "the stored image turn must rehydrate with the image part intact"
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
            assert_eq!(assistant_text, support::output_text(&replayed[0].response));
            assert_eq!(history[2]["role"], "user");
            assert_eq!(
                support::request_input_texts(second).last().map(String::as_str),
                Some(FOLLOW_UP_PROMPT)
            );
        }
        Scenario::ToolImage => {
            assert_eq!(history[0]["role"], "user");
            assert_eq!(history[0]["content"], TOOL_PROMPT);
            assert_eq!(history[1]["type"], "function_call");
            assert_eq!(history[1]["name"], "view_image");
            let call_id = history[1]["call_id"]
                .as_str()
                .expect("rehydrated call keeps its call_id");
            assert_eq!(history[2]["type"], "function_call_output");
            assert_eq!(
                history[2]["call_id"], call_id,
                "the tool output must answer the rehydrated call"
            );
            assert_tool_output_carries_image(&history[2]["output"]);
            assert_eq!(second["tools"], Value::Array(view_image_tools()));
        }
        Scenario::SingleImage | Scenario::MultiImage => unreachable!("single-turn scenario"),
    }
}

fn check_scenario_recordings(pair: &Pair) {
    assert_request_contract(pair);
    for (provider, cassette) in [(Provider::OpenAi, &pair.openai), (Provider::Gateway, &pair.gateway)] {
        let mut previous_response_id: Option<String> = None;
        for (turn, expected_item) in cassette.turns.iter().zip(pair.scenario.expected_items()) {
            let response = assert_terminal_contract(turn, expected_item, provider, previous_response_id.as_deref());
            if pair.streaming {
                assert_stream_events_contract(&support::recorded_named_sse_events(turn), expected_item);
            }
            previous_response_id = response["id"].as_str().map(str::to_owned);
        }
    }
}

async fn check_scenario_replay(pair: &Pair) {
    for cassette in [&pair.openai, &pair.gateway] {
        let (replayed, requests) = replay(cassette).await;
        assert_replayed_turns(pair.scenario, &replayed, pair.streaming);
        assert_upstream_requests(pair.scenario, &requests, &replayed, pair.streaming);
    }
}

#[test]
fn image_fixtures_embed_the_committed_pngs() {
    let single = fixture_json(SINGLE_IMAGE_TURN);
    assert_eq!(image_urls_of(&single[0]["content"]), vec![data_url(RED_BLUE_PNG)]);

    let multi = fixture_json(MULTI_IMAGE_TURN);
    let parts = multi[0]["content"].as_array().expect("content parts");
    assert_eq!(
        parts.iter().map(|part| &part["type"]).collect::<Vec<_>>(),
        vec!["input_text", "input_image", "input_text", "input_image", "input_text"],
        "text and images must interleave so ordering is observable"
    );
    assert_eq!(
        image_urls_of(&multi[0]["content"]),
        vec![data_url(RED_BLUE_PNG), data_url(GREEN_YELLOW_PNG)]
    );
    assert_ne!(RED_BLUE_PNG, GREEN_YELLOW_PNG, "the two images must be distinguishable");

    let tools = view_image_tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["name"], "view_image");
}

#[tokio::test]
async fn recorded_nonstreaming_image_scenarios_match_openai_contract() {
    for scenario in Scenario::ALL {
        let pair = load_pair(scenario, false);
        check_scenario_recordings(&pair);
        check_scenario_replay(&pair).await;
    }
}

#[tokio::test]
async fn recorded_streaming_image_scenarios_match_openai_contract() {
    for scenario in Scenario::ALL {
        let pair = load_pair(scenario, true);
        check_scenario_recordings(&pair);
        check_scenario_replay(&pair).await;
    }
}
