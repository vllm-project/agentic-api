mod support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const MARKER: &str = "CODE_INTERPRETER_OK=385";
const OPENAI_PREFIX: &str = "code-interpreter-openai-reference-";
const GATEWAY_PREFIX: &str = "code-interpreter-gateway-";

#[derive(Debug, PartialEq, Eq)]
struct SemanticResult {
    response_status: String,
    call_status: String,
    final_text: String,
}

fn cassette_directory() -> PathBuf {
    std::env::var_os("CODE_INTERPRETER_CASSETTE_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes/code_interpreter"),
        PathBuf::from,
    )
}

fn one_cassette(directory: &Path, prefix: &str, suffix: &str) -> PathBuf {
    let matches = fs::read_dir(directory)
        .expect("code-interpreter cassette directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix) && name.ends_with(suffix))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "expected one recorder-generated {prefix}*{suffix} cassette, found {matches:?}"
    );
    matches.into_iter().next().expect("one cassette")
}

fn load_one_turn(path: &Path) -> support::Cassette {
    let cassette = support::load_cassette(path.to_str().expect("cassette path should be UTF-8"));
    assert_eq!(cassette.turns.len(), 1, "code-interpreter scenario has one turn");
    assert_eq!(cassette.turns[0].request.path, "/v1/responses");
    cassette
}

fn terminal_response(turn: &support::Turn) -> Value {
    if let Some(body) = &turn.response.body {
        return body.clone();
    }
    support::recorded_named_sse_events(turn)
        .into_iter()
        .find(|event| event["type"] == "response.completed")
        .and_then(|event| event.get("response").cloned())
        .expect("streaming cassette should contain response.completed")
}

fn code_call(response: &Value) -> &Value {
    let calls = response["output"]
        .as_array()
        .expect("response output should be an array")
        .iter()
        .filter(|item| item["type"] == "code_interpreter_call")
        .collect::<Vec<_>>();
    assert_eq!(
        calls.len(),
        1,
        "response should contain exactly one code-interpreter call"
    );
    calls[0]
}

fn final_text(response: &Value) -> String {
    response["output"]
        .as_array()
        .expect("response output should be an array")
        .iter()
        .filter(|item| item["type"] == "message")
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == "output_text")
        .filter_map(|part| part["text"].as_str())
        .collect::<String>()
        .trim()
        .to_owned()
}

fn normalize_terminal(response: &Value) -> SemanticResult {
    let call = code_call(response);
    let id = call["id"].as_str().expect("code call should have an ID");
    let container_id = call["container_id"]
        .as_str()
        .expect("code call should have a container ID");
    let code = call["code"].as_str().expect("code call should contain code");
    assert!(!id.trim().is_empty());
    assert!(!container_id.trim().is_empty());
    assert!(!code.trim().is_empty());

    let semantic = SemanticResult {
        response_status: response["status"]
            .as_str()
            .expect("response should have a status")
            .to_owned(),
        call_status: call["status"]
            .as_str()
            .expect("code call should have a status")
            .to_owned(),
        final_text: final_text(response),
    };
    assert_eq!(semantic.response_status, "completed");
    assert_eq!(semantic.call_status, "completed");
    assert_eq!(semantic.final_text, MARKER);
    semantic
}

fn assert_gateway_logs(response: &Value) {
    let outputs = code_call(response)["outputs"]
        .as_array()
        .expect("gateway code call should expose execution outputs");
    assert!(
        outputs.iter().any(|output| {
            output["type"] == "logs" && output["logs"].as_str().is_some_and(|logs| logs.contains(MARKER))
        }),
        "gateway execution logs should contain the scenario marker"
    );
}

fn assert_sequence_numbers(events: &[Value]) {
    assert!(!events.is_empty(), "stream should contain events");
    for (expected, event) in events.iter().enumerate() {
        assert_eq!(
            event["sequence_number"].as_u64(),
            Some(u64::try_from(expected).expect("event count fits in u64")),
            "sequence numbers should be contiguous"
        );
        assert!(
            !matches!(event["type"].as_str(), Some("error" | "response.failed")),
            "recorded stream should not contain failure events"
        );
    }
    assert_eq!(
        events.last().and_then(|event| event["type"].as_str()),
        Some("response.completed")
    );
}

fn code_lifecycle(events: &[Value]) -> Vec<&Value> {
    let added_id = events
        .iter()
        .find(|event| event["type"] == "response.output_item.added" && event["item"]["type"] == "code_interpreter_call")
        .and_then(|event| event["item"]["id"].as_str())
        .expect("stream should add a code-interpreter item");

    events
        .iter()
        .filter(|event| {
            event["item_id"].as_str() == Some(added_id)
                || (event["item"]["type"] == "code_interpreter_call" && event["item"]["id"].as_str() == Some(added_id))
        })
        .collect()
}

fn collapsed_lifecycle_types<'a>(lifecycle: &[&'a Value]) -> Vec<&'a str> {
    lifecycle
        .iter()
        .map(|event| event["type"].as_str().expect("event should have a type"))
        .fold(Vec::new(), |mut types, event_type| {
            if event_type != "response.code_interpreter_call_code.delta" || types.last().copied() != Some(event_type) {
                types.push(event_type);
            }
            types
        })
}

fn assert_code_lifecycle(turn: &support::Turn) -> SemanticResult {
    let events = support::recorded_named_sse_events(turn);
    assert_sequence_numbers(&events);
    let lifecycle = code_lifecycle(&events);
    assert_eq!(
        collapsed_lifecycle_types(&lifecycle),
        [
            "response.output_item.added",
            "response.code_interpreter_call.in_progress",
            "response.code_interpreter_call_code.delta",
            "response.code_interpreter_call_code.done",
            "response.code_interpreter_call.interpreting",
            "response.code_interpreter_call.completed",
            "response.output_item.done",
        ],
        "code-call lifecycle should match the OpenAI reference order; provider chunking may only vary delta count"
    );

    let added = lifecycle[0];
    let done = lifecycle.last().expect("lifecycle should end with output_item.done");
    assert_eq!(added["item"]["status"], "in_progress");
    assert_eq!(added["item"]["code"], "");
    assert!(
        added["item"].get("outputs").is_some_and(Value::is_null),
        "OpenAI-reference added item shape includes outputs: null"
    );
    assert_eq!(done["item"]["status"], "completed");

    let item_id = added["item"]["id"].as_str().expect("added item ID");
    let output_index = added["output_index"].as_u64().expect("added output index");
    assert!(lifecycle.iter().all(|event| {
        event
            .get("item_id")
            .and_then(Value::as_str)
            .or_else(|| event["item"]["id"].as_str())
            == Some(item_id)
    }));
    assert!(
        lifecycle
            .iter()
            .all(|event| event["output_index"].as_u64() == Some(output_index))
    );

    let code_from_deltas = lifecycle
        .iter()
        .filter(|event| event["type"] == "response.code_interpreter_call_code.delta")
        .filter_map(|event| event["delta"].as_str())
        .collect::<String>();
    let code_done = lifecycle
        .iter()
        .find(|event| event["type"] == "response.code_interpreter_call_code.done")
        .and_then(|event| event["code"].as_str())
        .expect("code.done should contain code");
    assert_eq!(code_from_deltas, code_done, "code deltas should fold to code.done");
    assert_eq!(done["item"]["code"], code_done);

    let terminal = terminal_response(turn);
    let terminal_call = code_call(&terminal);
    assert_eq!(terminal_call["id"], item_id);
    assert_eq!(terminal_call["code"], code_done);
    assert_eq!(terminal_call, &done["item"]);
    normalize_terminal(&terminal)
}

fn assert_request_contract(turn: &support::Turn, expected_tool: Value, expected_store: bool, expected_stream: bool) {
    assert_eq!(turn.request.body.tools, vec![expected_tool]);
    assert_eq!(turn.request.body.tool_choice, Some(json!("required")));
    assert_eq!(turn.request.body.parallel_tool_calls, Some(false));
    assert_eq!(turn.request.body.max_output_tokens, Some(2048));
    assert_eq!(turn.request.body.store, expected_store);
    assert_eq!(turn.request.body.stream, expected_stream);
}

fn raw_cassette(path: &Path) -> serde_yaml::Value {
    let raw = fs::read_to_string(path).expect("WebSocket cassette should be readable");
    serde_yaml::from_str(&raw).expect("cassette should be YAML")
}

fn websocket_messages(path: &Path) -> Vec<Value> {
    let cassette = raw_cassette(path);
    cassette["turns"][0]["response"]["websocket"]
        .as_sequence()
        .expect("WebSocket cassette should contain raw messages")
        .iter()
        .map(|message| {
            let text = message.as_str().expect("WebSocket message should be text");
            serde_json::from_str(text).expect("WebSocket message should be JSON")
        })
        .collect()
}

#[test]
fn openai_reference_and_gateway_blocking_responses_have_semantic_parity() {
    let directory = cassette_directory();
    let openai = load_one_turn(&one_cassette(&directory, OPENAI_PREFIX, "-nonstreaming.yaml"));
    let gateway = load_one_turn(&one_cassette(&directory, GATEWAY_PREFIX, "-nonstreaming.yaml"));
    let openai_turn = &openai.turns[0];
    let gateway_turn = &gateway.turns[0];

    assert_request_contract(
        openai_turn,
        json!({"type": "code_interpreter", "container": {"type": "auto"}}),
        false,
        false,
    );
    assert_request_contract(
        gateway_turn,
        json!({"type": "code_interpreter", "execution": "gateway"}),
        false,
        false,
    );

    let openai_response = terminal_response(openai_turn);
    let gateway_response = terminal_response(gateway_turn);
    assert_eq!(
        normalize_terminal(&openai_response),
        normalize_terminal(&gateway_response)
    );
    assert!(
        code_call(&openai_response)["outputs"].is_null(),
        "OpenAI reference does not expose container logs in the terminal call"
    );
    assert_gateway_logs(&gateway_response);
}

#[test]
fn openai_and_gateway_streams_follow_the_reference_code_call_lifecycle() {
    let directory = cassette_directory();
    let openai = load_one_turn(&one_cassette(&directory, OPENAI_PREFIX, "-streaming.yaml"));
    let gateway = load_one_turn(&one_cassette(&directory, GATEWAY_PREFIX, "-streaming.yaml"));

    assert_request_contract(
        &openai.turns[0],
        json!({"type": "code_interpreter", "container": {"type": "auto"}}),
        false,
        true,
    );
    assert_request_contract(
        &gateway.turns[0],
        json!({"type": "code_interpreter", "execution": "gateway"}),
        false,
        true,
    );

    let openai_semantic = assert_code_lifecycle(&openai.turns[0]);
    let gateway_semantic = assert_code_lifecycle(&gateway.turns[0]);
    assert_eq!(openai_semantic, gateway_semantic);
    assert!(code_call(&terminal_response(&openai.turns[0]))["outputs"].is_null());
    assert_gateway_logs(&terminal_response(&gateway.turns[0]));
}

#[test]
fn gateway_http_sse_and_websocket_have_transport_parity() {
    let directory = cassette_directory();
    let blocking = load_one_turn(&one_cassette(&directory, GATEWAY_PREFIX, "-nonstreaming.yaml"));
    let sse = load_one_turn(&one_cassette(&directory, GATEWAY_PREFIX, "-streaming.yaml"));
    let websocket_path = one_cassette(&directory, GATEWAY_PREFIX, "-websocket.yaml");
    let websocket = load_one_turn(&websocket_path);

    assert_request_contract(
        &websocket.turns[0],
        json!({"type": "code_interpreter", "execution": "gateway"}),
        true,
        false,
    );
    let raw_websocket = raw_cassette(&websocket_path);
    assert_eq!(raw_websocket["turns"][0]["request"]["method"], "WEBSOCKET");
    assert_eq!(raw_websocket["turns"][0]["request"]["transport"], "websocket");
    assert!(raw_websocket["turns"][0]["request"]["body"]["stream"].is_null());

    let blocking_semantic = normalize_terminal(&terminal_response(&blocking.turns[0]));
    let sse_semantic = assert_code_lifecycle(&sse.turns[0]);
    let websocket_semantic = assert_code_lifecycle(&websocket.turns[0]);
    assert_eq!(blocking_semantic, sse_semantic);
    assert_eq!(sse_semantic, websocket_semantic);
    assert_gateway_logs(&terminal_response(&sse.turns[0]));
    assert_gateway_logs(&terminal_response(&websocket.turns[0]));

    let recorded_sse = support::recorded_named_sse_events(&websocket.turns[0]);
    assert_eq!(
        websocket_messages(&websocket_path),
        recorded_sse,
        "recorder-synthesized SSE should exactly preserve every WebSocket event"
    );
}

#[test]
fn recorder_cassettes_do_not_contain_unmasked_authorization() {
    let directory = cassette_directory();
    for prefix in [OPENAI_PREFIX, GATEWAY_PREFIX] {
        for suffix in ["-nonstreaming.yaml", "-streaming.yaml"] {
            let cassette = raw_cassette(&one_cassette(&directory, prefix, suffix));
            let authorization = &cassette["turns"][0]["request"]["headers"]["authorization"];
            assert!(
                authorization.is_null() || authorization.as_str() == Some("Bearer ***"),
                "cassette authorization must be absent or masked"
            );
        }
    }
    let websocket = raw_cassette(&one_cassette(&directory, GATEWAY_PREFIX, "-websocket.yaml"));
    assert!(websocket["turns"][0]["request"]["headers"]["authorization"].is_null());
}
