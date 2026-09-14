use crate::support::{
    TestFixture, Turn, collect_stream, expected_text, load_cassette, make_request, output_text, request_input_texts,
    unwrap_blocking,
};
use agentic_core::events::normalize_sse_line;
use agentic_core::executor::execute;
use agentic_core::types::io::OutputItem;
use agentic_core::types::request_response::ResponsePayload;
use agentic_core::types::tools::ResponsesTool;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

const TURN1_PROMPT: &str = "Remember the word APPLE. Just say: OK";
const TURN2_PROMPT: &str = "What word did I ask you to remember? Reply with just the word.";
const TOOL_PROMPT: &str = "What is the current NVIDIA stock price? Use the tool.";

pub struct Provider {
    pub directory: &'static str,
    pub prefix: &'static str,
    pub model_slug: &'static str,
    pub version: Option<&'static str>,
}

#[derive(Deserialize)]
struct RecordingMetadata {
    provider: Provenance,
}

#[derive(Deserialize)]
struct Provenance {
    name: String,
    version: String,
    model: String,
    transport: String,
}

impl Provider {
    fn cassette_path(&self, scenario: &str, streaming: bool) -> String {
        let suffix = if streaming { "streaming" } else { "nonstreaming" };
        let root = std::env::var("PROVIDER_CASSETTE_ROOT")
            .unwrap_or_else(|_| format!("{}/tests/cassettes", env!("CARGO_MANIFEST_DIR")));
        format!(
            "{}/{}/{}-{}-{}-{}.yaml",
            root, self.directory, self.prefix, scenario, self.model_slug, suffix
        )
    }

    fn load(&self, scenario: &str, streaming: bool) -> crate::support::Cassette {
        let path = self.cassette_path(scenario, streaming);
        eprintln!("replaying {}@{} {path}", self.prefix, self.version.unwrap_or("legacy"));
        let cassette = load_cassette(&path);
        if let Some(version) = self.version {
            let metadata: RecordingMetadata = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap())
                .expect("versioned recording must carry provenance");
            assert_eq!(metadata.provider.name, self.prefix);
            assert_eq!(metadata.provider.version, version);
            assert_eq!(metadata.provider.model.replace(['/', ':', ' '], "-"), self.model_slug);
            assert_eq!(
                metadata.provider.transport,
                if streaming { "http-sse" } else { "http-json" }
            );
        }
        for (turn_index, turn) in cassette.turns.iter().enumerate() {
            if let Some(raw_frames) = &turn.response.sse {
                for (frame_index, line) in raw_frames.iter().flat_map(|raw| raw.lines()).enumerate() {
                    if let Some(data) = line.strip_prefix("data:") {
                        if data.trim() != "[DONE]" {
                            assert!(
                                normalize_sse_line(line).is_some(),
                                "{path}, turn {}, frame {frame_index}: discarded {line}",
                                turn_index + 1
                            );
                        }
                    }
                }
            }
        }
        cassette
    }
}

fn turn2_prompt_from(turn: &Turn) -> String {
    request_input_texts(&serde_json::json!({ "input": turn.request.body.input }))
        .pop()
        .expect("turn 2 recording ends with the user prompt")
}

fn function_calls(payload: &ResponsePayload) -> Vec<(String, String)> {
    payload
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::FunctionCall(fc) => Some((fc.name.clone(), fc.arguments.clone())),
            _ => None,
        })
        .collect()
}

fn first_message_id(payload: &ResponsePayload) -> &str {
    payload
        .output
        .iter()
        .find_map(|item| match item {
            OutputItem::Message(msg) => Some(msg.id.as_str()),
            _ => None,
        })
        .expect("turn 1 output contains an assistant message")
}

/// The upstream request for the second turn must carry the rehydrated item
/// history instead of forwarding gateway-managed response IDs upstream.
/// The history is compared structurally with the recorded turn-2 request; only
/// the assistant message id differs per run, so it is taken from turn 1's payload.
fn assert_upstream_requests_are_stateless(requests: &[Value], t2: &Turn, p1: &ResponsePayload) {
    assert_eq!(requests.len(), 2, "one upstream call per turn");
    for request in requests {
        assert!(
            request.get("previous_response_id").is_none(),
            "gateway response IDs must not reach upstream; request was {request}"
        );
    }
    assert_eq!(request_input_texts(&requests[0]), vec![TURN1_PROMPT]);

    let mut expected_history = t2.request.body.input.clone();
    let recorded_assistant = expected_history
        .as_array_mut()
        .expect("recorded history is an item array")
        .iter_mut()
        .find(|item| item["type"] == "message" && item["role"] == "assistant")
        .expect("recorded turn 2 replays the assistant item");
    recorded_assistant["id"] = Value::String(first_message_id(p1).to_owned());
    assert_eq!(
        requests[1]["input"], expected_history,
        "turn 2 must replay the full item history to the stateless upstream"
    );
}

pub async fn run_stateful_two_turn(provider: &Provider, streaming: bool) {
    let cassette = provider.load("stateful", streaming);
    let (t1, t2) = (&cassette.turns[0], &cassette.turns[1]);
    assert_eq!(turn2_prompt_from(t2), TURN2_PROMPT);
    let fixture = TestFixture::new(&[t1, t2]).await;

    let first = execute(
        make_request(TURN1_PROMPT, true, streaming, None, None),
        Arc::clone(&fixture.exec_ctx),
    )
    .await
    .expect("t1");
    let p1 = if streaming {
        collect_stream(first).await
    } else {
        unwrap_blocking(first)
    };
    let second = execute(
        make_request(TURN2_PROMPT, true, streaming, Some(p1.id.clone()), None),
        Arc::clone(&fixture.exec_ctx),
    )
    .await
    .expect("t2");
    let p2 = if streaming {
        collect_stream(second).await
    } else {
        unwrap_blocking(second)
    };

    assert_eq!(p1.status, "completed");
    assert_eq!(output_text(&p1), expected_text(t1));
    assert!(!output_text(&p1).trim().is_empty());
    assert_ne!(p2.id, p1.id);
    assert_eq!(p2.status, "completed");
    assert_eq!(p2.previous_response_id.as_deref(), Some(p1.id.as_str()));
    assert_eq!(output_text(&p2), expected_text(t2));
    assert!(
        output_text(&p2).contains("APPLE"),
        "continuation must retain the remembered word"
    );

    assert_upstream_requests_are_stateless(&fixture.request_bodies().await, t2, &p1);
}

pub async fn run_function_tool_call(provider: &Provider, streaming: bool) {
    let cassette = provider.load("tool-call-auto", streaming);
    let t1 = &cassette.turns[0];
    let tools: Vec<ResponsesTool> =
        serde_json::from_value(Value::Array(t1.request.body.tools.clone())).expect("recorded tools parse");
    let fixture = TestFixture::new(&[t1]).await;

    let mut request = make_request(TOOL_PROMPT, true, streaming, None, None);
    request.tools = Some(tools);
    let result = execute(request, Arc::clone(&fixture.exec_ctx)).await.expect("t1");
    let payload = if streaming {
        collect_stream(result).await
    } else {
        unwrap_blocking(result)
    };

    assert_eq!(payload.status, "completed");
    let calls = function_calls(&payload);
    assert_eq!(calls.len(), 1, "recording must yield one function_call: {calls:?}");
    let (name, arguments) = &calls[0];
    assert_eq!(name, "get_stock_price");
    let arguments: Value = serde_json::from_str(arguments).expect("arguments are JSON");
    assert_eq!(arguments["ticker"], "NVDA");

    let requests = fixture.request_bodies().await;
    assert_eq!(requests.len(), 1, "client-executed function tools take one model call");
    let upstream_tools = requests[0]["tools"].as_array().expect("tools forwarded upstream");
    assert!(
        upstream_tools.iter().any(|tool| tool["name"] == "get_stock_price"),
        "function declarations must reach the upstream unchanged"
    );
}
