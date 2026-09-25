//! Offline characterization of real pinned-provider captures, not execution authority.

use super::*;
use crate::types::{RequestPayload, reasoning_profile::OpaqueReasoningProfile};
use std::path::PathBuf;

const PROFILE: OpaqueReasoningProfile = OpaqueReasoningProfile::OpenAiGpt54_20260305V1;

#[derive(Deserialize)]
struct Capture {
    turns: Vec<CaptureTurn>,
}

#[derive(Deserialize)]
struct CaptureTurn {
    request: CaptureRequest,
    response: RecordedResponse,
}

#[derive(Deserialize)]
struct CaptureRequest {
    body: Value,
}

fn capture(scenario: &str, transport: &str) -> Capture {
    let root = std::env::var_os("AGENTIC_OPAQUE_CASSETTE_DIR").map_or_else(
        || {
            PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/cassettes/reasoning/opaque/gpt-5.4-2026-03-05"
            ))
        },
        PathBuf::from,
    );
    let bytes = std::fs::read(root.join(format!("{scenario}-{transport}.yaml"))).expect("recorded fixture");
    serde_yaml::from_slice(&bytes).expect("cassette structure")
}

fn terminal(response: &RecordedResponse) -> Value {
    response.body.clone().unwrap_or_else(|| {
        response
            .sse
            .as_ref()
            .unwrap()
            .iter()
            .flat_map(|raw| raw.lines())
            .find_map(|line| {
                let event: Value = serde_json::from_str(line.strip_prefix("data:")?.trim()).ok()?;
                (event["type"] == "response.completed").then(|| event["response"].clone())
            })
            .expect("explicit recorded terminal")
    })
}

fn completed_items(response: &RecordedResponse) -> Vec<Value> {
    if let Some(body) = &response.body {
        return body["output"].as_array().unwrap().clone();
    }
    // The gateway's single ingestion path retains output_item.done, not the
    // distinct opaque value also present in the terminal response envelope.
    response
        .sse
        .as_ref()
        .unwrap()
        .iter()
        .flat_map(|raw| raw.lines())
        .filter_map(|line| {
            let event: Value = serde_json::from_str(line.strip_prefix("data:")?.trim()).ok()?;
            (event["type"] == "response.output_item.done").then(|| event["item"].clone())
        })
        .collect()
}

fn check_items(source: &[Value], projected: &[Value]) {
    assert_eq!(source.len(), projected.len(), "item count");
    for (source, projected) in source.iter().zip(projected) {
        for field in ["type", "id", "call_id", "name", "arguments", "role", "phase"] {
            assert!(
                source[field] == projected[field],
                "item field {field} must survive typed replay"
            );
        }
        if source["type"] == "reasoning" {
            assert!(
                source["encrypted_content"] == projected["encrypted_content"],
                "opaque bytes changed"
            );
            assert!(source["summary"] == projected["summary"], "reasoning summary changed");
        }
    }
}

#[tokio::test]
async fn pinned_reference_matrix_ingests_with_exact_model_and_preserves_output_items() {
    for scenario in ["continuation", "function"] {
        for transport in ["json", "sse", "websocket"] {
            let capture = capture(scenario, transport);
            assert_eq!(capture.turns.len(), 3);
            for turn in capture.turns {
                let terminal = terminal(&turn.response);
                assert_eq!(terminal["model"], PROFILE.model());
                let expected = completed_items(&turn.response);
                let mut context = tests::request_context();
                context.enriched_request = serde_json::from_value(turn.request.body).unwrap();
                let mut agent = agent_pipeline(context, None, None);
                let (payload, observed) = if let Some(body) = turn.response.body {
                    let ingested = agent
                        .run_with_json_body(
                            &body.to_string(),
                            Validation::Strict,
                            TranslationContext::default(),
                            None,
                        )
                        .unwrap();
                    (ingested.payload, ingested.upstream_model)
                } else {
                    let lines = turn.response.sse.unwrap().join("");
                    let ingested = agent
                        .run_with_stream_body(
                            futures::stream::iter(lines.lines().map(|line| Ok(line.to_owned()))),
                            Validation::Strict,
                            TranslationContext::default(),
                            &ToolRegistry::default(),
                            0,
                            None,
                        )
                        .await
                        .unwrap();
                    (ingested.payload, ingested.upstream_model)
                };
                assert!(observed.as_ref().is_some_and(|model| model.as_str() == PROFILE.model()));
                let output: Vec<Value> = payload
                    .output
                    .iter()
                    .map(|item| serde_json::to_value(item).unwrap())
                    .collect();
                check_items(&expected, &output);
                let input = payload.output.iter().filter_map(OutputItem::to_input_item).collect();
                let mut context = tests::request_context();
                context.enriched_request.input = ResponsesInput::Items(input);
                let projected: Value =
                    serde_json::from_str(&request_for_policy(&context, false, OPAQUE).unwrap()).unwrap();
                check_items(&expected, projected["input"].as_array().unwrap());
            }
        }
    }
}

#[test]
fn pinned_reference_requests_preserve_stateless_reasoning_phase_and_tool_contracts() {
    for scenario in ["continuation", "function"] {
        for transport in ["json", "sse", "websocket"] {
            for turn in capture(scenario, transport).turns {
                let recorded = turn.request.body;
                let request: RequestPayload = serde_json::from_value(recorded.clone()).unwrap();
                let mut context = tests::request_context();
                context.enriched_request = request;
                let wire: Value =
                    serde_json::from_str(&request_for_policy(&context, transport != "json", OPAQUE).unwrap()).unwrap();
                assert_eq!(wire["store"], false);
                assert!(wire.get("previous_response_id").is_none());
                for field in ["model", "reasoning", "max_output_tokens", "tools"] {
                    assert!(
                        wire[field] == recorded[field],
                        "request field {field} differs from capture"
                    );
                }
                check_items(recorded["input"].as_array().unwrap(), wire["input"].as_array().unwrap());
            }
        }
    }
}

#[test]
fn pinned_reference_children_replay_item_completion_bytes_from_their_parent() {
    for scenario in ["continuation", "function"] {
        for transport in ["json", "sse", "websocket"] {
            let capture = capture(scenario, transport);
            let parent = &capture.turns[0];
            let mut prefix = parent.request.body["input"].as_array().unwrap().clone();
            let output = completed_items(&parent.response);
            prefix.extend(output.iter().cloned());
            for child in &capture.turns[1..] {
                let input = child.request.body["input"].as_array().unwrap();
                assert!(input.len() >= prefix.len());
                check_items(&prefix, &input[..prefix.len()]);
            }
            if transport != "json" {
                let terminal = terminal(&parent.response);
                assert!(
                    output
                        .iter()
                        .zip(terminal["output"].as_array().unwrap())
                        .any(|(done, final_item)| {
                            done["type"] == "reasoning" && done["encrypted_content"] != final_item["encrypted_content"]
                        }),
                    "pinned capture characterizes distinct terminal and item-completion ciphertext"
                );
            }
        }
    }
}

#[tokio::test]
async fn pinned_reference_rejects_missing_terminal_duplicate_completion_and_wrong_index() {
    let response = capture("function", "sse").turns.remove(0).response;
    let wire = response.sse.unwrap().join("");
    let lines: Vec<_> = wire.lines().map(str::to_owned).collect();
    let find_event = |kind: &str| {
        lines
            .iter()
            .position(|line| {
                line.strip_prefix("data:")
                    .and_then(|data| serde_json::from_str::<Value>(data).ok())
                    .is_some_and(|event| event["type"] == kind)
            })
            .unwrap()
    };
    let terminal = find_event("response.completed");
    assert!(
        ingest_stream(lines[..terminal].iter().cloned().map(Ok).collect())
            .await
            .is_err()
    );
    let done = find_event("response.output_item.done");
    let mut duplicate = lines.clone();
    duplicate.insert(done, lines[done].clone());
    assert!(ingest_stream(duplicate.into_iter().map(Ok).collect()).await.is_err());
    let mut wrong_index = lines.clone();
    let mut event: Value = serde_json::from_str(lines[done].strip_prefix("data:").unwrap()).unwrap();
    event["output_index"] = json!(999);
    wrong_index[done] = format!("data: {event}");
    assert!(ingest_stream(wrong_index.into_iter().map(Ok).collect()).await.is_err());
}
