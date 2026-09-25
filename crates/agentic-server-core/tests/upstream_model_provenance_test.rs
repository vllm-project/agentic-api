//! Replay captured exchanges, with explicit in-memory fault injection (no YAML edits).

mod support;

use std::fmt::Write;
use std::sync::Arc;

use agentic_core::executor::{ExecuteRequest, ExecutorError, UpstreamBody, decode_upstream, rehydrate_conversation};
use agentic_core::storage::InOutItem;
use agentic_core::types::InputItem;
use agentic_core::types::reasoning_replay::ReasoningProvenance;
use agentic_core::types::upstream_identity::UpstreamModelError;
use serde_json::{Value, json};

fn cassette(streaming: bool) -> support::Cassette {
    let mode = if streaming { "streaming" } else { "nonstreaming" };
    support::load_cassette(&format!(
        "{}/tests/cassettes/reasoning/responses/reasoning-single-Qwen-Qwen3-30B-A3B-FP8-{mode}.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))
}

fn fault_injected_response(turn: &support::Turn, model: Option<&Value>, terminal_only: bool) -> support::MockResponse {
    let mutate = |response: &mut Value| {
        if let Some(model) = model {
            response["model"] = model.clone();
        } else {
            response.as_object_mut().unwrap().remove("model");
        }
    };
    match support::MockResponse::from_turn(turn) {
        support::MockResponse::Json(body) => {
            let mut body: Value = serde_json::from_str(&body).unwrap();
            mutate(&mut body);
            support::MockResponse::Json(body.to_string())
        }
        support::MockResponse::Sse(body) => {
            let mut replay = String::with_capacity(body.len());
            for line in body.lines() {
                if let Some(data) = line.strip_prefix("data: ")
                    && let Ok(mut event) = serde_json::from_str::<Value>(data)
                    && event["response"].is_object()
                    && (!terminal_only || event["type"] == "response.completed")
                {
                    mutate(&mut event["response"]);
                    writeln!(replay, "data: {event}").unwrap();
                } else {
                    writeln!(replay, "{line}").unwrap();
                }
            }
            support::MockResponse::Sse(replay)
        }
        // A status-only reply carries no model metadata to inject.
        status @ support::MockResponse::Status(..) => status,
    }
}

/// Stored rows keep upstream item IDs, so each replayed copy of a recording needs its own.
fn with_unique_item_ids(responses: Vec<support::MockResponse>) -> Vec<support::MockResponse> {
    fn rename(value: &mut Value, suffix: &str) {
        match value {
            Value::Object(object) => {
                for (key, value) in object.iter_mut() {
                    match value {
                        Value::String(id) if matches!(key.as_str(), "id" | "item_id") && !id.starts_with("resp") => {
                            id.push_str(suffix);
                        }
                        _ => rename(value, suffix),
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| rename(item, suffix)),
            _ => {}
        }
    }
    responses
        .into_iter()
        .enumerate()
        .map(|(index, response)| {
            let suffix = format!("_replay{index}");
            match response {
                support::MockResponse::Json(body) => {
                    let mut body: Value = serde_json::from_str(&body).unwrap();
                    rename(&mut body, &suffix);
                    support::MockResponse::Json(body.to_string())
                }
                support::MockResponse::Sse(body) => {
                    let mut replay = String::with_capacity(body.len());
                    for line in body.lines() {
                        match line
                            .strip_prefix("data: ")
                            .and_then(|data| serde_json::from_str::<Value>(data).ok())
                        {
                            Some(mut event) => {
                                rename(&mut event, &suffix);
                                writeln!(replay, "data: {event}").unwrap();
                            }
                            None => writeln!(replay, "{line}").unwrap(),
                        }
                    }
                    support::MockResponse::Sse(replay)
                }
                status @ support::MockResponse::Status(..) => status,
            }
        })
        .collect()
}

async fn execute_and_read_provenance(fixture: &support::TestFixture, streaming: bool) -> ReasoningProvenance {
    let mut request = support::make_request("HELLO", true, streaming, None, None);
    request.model = "unchanged-client-alias".into();
    let result = ExecuteRequest::new(request, Arc::clone(&fixture.exec_ctx))
        .run()
        .await
        .unwrap();
    let response = if streaming {
        support::collect_stream(result).await
    } else {
        support::unwrap_blocking(result)
    };
    assert_eq!(response.model, "unchanged-client-alias");
    let ctx = rehydrate_conversation(
        support::make_request("continue", true, false, Some(response.id), None),
        &fixture.exec_ctx,
    )
    .await
    .unwrap();
    let history = fixture.exec_ctx.resp_handler.rehydrate(&ctx).await.unwrap();
    let provenance = InOutItem::into_input_items(history)
        .iter()
        .find_map(|item| match item {
            InputItem::Reasoning(reasoning) => reasoning.replay_provenance,
            _ => None,
        })
        .expect("reasoning has durable provenance");
    assert_ne!(provenance, ReasoningProvenance::client_submitted());
    provenance
}

#[tokio::test]
async fn reported_model_changes_persisted_identity_even_when_request_alias_is_unchanged() {
    let json = cassette(false);
    let sse = cassette(true);
    let turns = [&json.turns[0], &sse.turns[0]];
    let mut responses = turns
        .iter()
        .map(|turn| support::MockResponse::from_turn(turn))
        .collect::<Vec<_>>();
    for model in [Some(json!("different-snapshot")), None, Some(Value::Null)] {
        for turn in turns {
            responses.push(fault_injected_response(turn, model.as_ref(), false));
        }
    }
    responses.push(fault_injected_response(
        turns[1],
        Some(&json!("conflicting-snapshot")),
        true,
    ));
    let fixture = support::TestFixture::new_with_responses(with_unique_item_ids(responses)).await;
    let mut identities = Vec::new();
    for index in 0..9 {
        let streaming = index % 2 == 1 || index == 8;
        identities.push(execute_and_read_provenance(&fixture, streaming).await);
    }
    for pair in identities.chunks_exact(2) {
        assert_eq!(pair[0], pair[1], "JSON and SSE must bind the same reported model");
    }
    assert_ne!(identities[0], identities[2], "reported model must affect provenance");
    assert_ne!(
        identities[0], identities[4],
        "unknown is not the requested or reported model"
    );
    assert_ne!(identities[2], identities[4]);
    assert_eq!(identities[4], identities[6], "missing and null both mean unknown");
    assert_eq!(
        identities[4], identities[8],
        "lenient conflicting names must remain unknown"
    );
    assert!(
        fixture
            .request_bodies()
            .await
            .iter()
            .all(|request| request["model"] == "unchanged-client-alias")
    );
}

#[tokio::test]
async fn recorded_gateway_alias_rewrite_is_unknown_in_lenient_and_rejected_in_strict_ingestion() {
    let cassette = support::load_cassette(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/cassettes/reasoning/responses/reasoning-gateway-gpt-5.6-streaming.yaml",
    ));
    let turn = &cassette.turns[0];
    let fixture = support::TestFixture::new_with_responses(with_unique_item_ids(vec![
        support::MockResponse::from_turn(turn),
        fault_injected_response(turn, None, false),
    ]))
    .await;
    let ctx = rehydrate_conversation(
        support::make_request("HELLO", false, true, None, None),
        &fixture.exec_ctx,
    )
    .await
    .unwrap();
    let body = turn.response.sse.as_ref().unwrap().join("");
    let error = decode_upstream(ctx, UpstreamBody::Sse(&body)).await.unwrap_err();
    assert!(matches!(
        error,
        ExecutorError::UpstreamModel(UpstreamModelError::Changed)
    ));
    assert!(fixture.request_bodies().await.is_empty());
    let conflicted = execute_and_read_provenance(&fixture, true).await;
    let unknown = execute_and_read_provenance(&fixture, true).await;
    assert_eq!(conflicted, unknown);
}

#[tokio::test]
async fn malformed_model_metadata_persists_unknown_evidence_in_both_body_formats() {
    let json = cassette(false);
    let sse = cassette(true);
    let fixture = support::TestFixture::new_with_responses(with_unique_item_ids(vec![
        fault_injected_response(&json.turns[0], Some(&json!({"sensitive":"invalid"})), false),
        fault_injected_response(&json.turns[0], None, false),
        fault_injected_response(&sse.turns[0], Some(&json!({"sensitive":"invalid"})), true),
        fault_injected_response(&sse.turns[0], None, false),
    ]))
    .await;
    let invalid_json = execute_and_read_provenance(&fixture, false).await;
    let missing_json = execute_and_read_provenance(&fixture, false).await;
    let invalid_sse = execute_and_read_provenance(&fixture, true).await;
    let missing_sse = execute_and_read_provenance(&fixture, true).await;
    assert_eq!(invalid_json, missing_json);
    assert_eq!(invalid_sse, missing_sse);
    assert_eq!(invalid_json, invalid_sse);
    assert_eq!(fixture.request_bodies().await.len(), 4);
}
