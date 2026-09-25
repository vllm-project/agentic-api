use super::support::*;
use crate::executor::{ExecuteRequest, ExecutorError, rehydrate_conversation, rehydrate_in_session};
use crate::storage::{InOutItem, ResponseStore};
use crate::types::{
    io::{InputItem, OutputItem, ResponsesInput},
    reasoning_profile::OpaqueReplayRequestField as Field,
    reasoning_replay::{ReasoningProvenance, ReasoningReplayError, ReasoningReplayPolicy, ReasoningSource},
};
use std::sync::Arc;

fn assert_provenance(input: &ResponsesInput) {
    let ResponsesInput::Items(items) = input else {
        panic!("canonical item history");
    };
    let mut count = 0;
    for item in items {
        if let InputItem::Reasoning(reasoning) = item {
            count += 1;
            assert!(matches!(
                reasoning.replay_provenance,
                Some(ReasoningProvenance::V1 {
                    source: ReasoningSource::Upstream {
                        policy: ReasoningReplayPolicy::OpaqueResponses,
                        ..
                    }
                })
            ));
        }
    }
    assert!(count > 0);
}

#[tokio::test]
async fn pinned_execution_durable_continuations_and_branches_match_recorded_input() {
    for scenario in ["continuation", "function"] {
        for streaming in [false, true] {
            let capture = capture(scenario, streaming);
            let fixture = Fixture::new(capture.turns.iter().map(|turn| turn.response.clone())).await;
            let first = collect(fixture.run(request(&capture.turns[0], true), None).await.unwrap()).await;
            let prefix = capture.turns[0].request.body["input"].as_array().unwrap().len() + first.output.len();
            for turn in &capture.turns[1..] {
                let followup = child(turn, prefix, &first.id, true);
                let restored = rehydrate_conversation(followup.clone(), &fixture.context)
                    .await
                    .unwrap();
                assert_provenance(&restored.enriched_request.input);
                let result = collect(fixture.run(followup, None).await.unwrap()).await;
                assert_eq!(result.previous_response_id.as_deref(), Some(first.id.as_str()));
            }
            for (turn, sent) in capture.turns.iter().zip(fixture.requests().await) {
                assert_request(turn, &sent);
            }
            assert_eq!(fixture.requests().await.len(), 3);
            assert_eq!(fixture.row_count().await, 3);
            fixture.stop().await;
        }
    }
}

#[tokio::test]
async fn pinned_execution_transient_forks_and_promotion_preserve_origin_and_isolation() {
    for scenario in ["continuation", "function"] {
        for streaming in [false, true] {
            let capture = capture(scenario, streaming);
            let fixture = Fixture::new(capture.turns.iter().map(|turn| turn.response.clone())).await;
            let group = group();
            let source = group.new_session().unwrap();
            let target = group.new_session().unwrap();
            let promoted = group.new_session().unwrap();
            let first = collect(
                fixture
                    .run(request(&capture.turns[0], false), Some(&source))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(fixture.row_count().await, 0);
            let prefix = capture.turns[0].request.body["input"].as_array().unwrap().len() + first.output.len();
            let second = collect(
                fixture
                    .run(child(&capture.turns[1], prefix, &first.id, false), Some(&target))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(fixture.row_count().await, 0);
            let third = collect(
                fixture
                    .run(child(&capture.turns[2], prefix, &first.id, true), Some(&promoted))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(fixture.row_count().await, 1);
            let stored = ResponseStore::new(Arc::clone(&fixture.pool))
                .rehydrate(&third.id)
                .await
                .unwrap();
            let history = ResponsesInput::Items(InOutItem::into_input_items(stored));
            assert_provenance(&history);
            let wire = serde_json::to_value(&history).unwrap();
            assert_input(
                &capture.turns[2].request.body["input"],
                &serde_json::Value::Array(
                    wire.as_array().unwrap()[..prefix + if scenario == "function" { 2 } else { 1 }].to_vec(),
                ),
            );
            assert!(!serde_json::to_string(&wire).unwrap().contains(&second.id));
            for (turn, sent) in capture.turns.iter().zip(fixture.requests().await) {
                assert_request(turn, &sent);
            }
            assert_eq!(fixture.requests().await.len(), 3);
            fixture.stop().await;
        }
    }
}

#[tokio::test]
async fn pinned_execution_rejects_rotated_credentials_and_manual_origin_before_network() {
    for streaming in [false, true] {
        let capture = capture("continuation", streaming);
        let fixture = Fixture::new([capture.turns[0].response.clone()]).await;
        let first = collect(fixture.run(request(&capture.turns[0], true), None).await.unwrap()).await;
        let prefix = capture.turns[0].request.body["input"].as_array().unwrap().len() + first.output.len();
        let followup = child(&capture.turns[1], prefix, &first.id, true);
        let error = ExecuteRequest::new(followup, Arc::clone(&fixture.context))
            .with_auth(Some("rotated".into()))
            .run()
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error,
            ExecutorError::ReasoningReplay(ReasoningReplayError::IncompatibleProvenance)
        ));
        let mut manual = request(&capture.turns[1], true);
        manual.previous_response_id = None;
        let error = fixture.run(manual, None).await.err().unwrap();
        assert!(matches!(
            error,
            ExecutorError::ReasoningReplay(ReasoningReplayError::UnknownProvenance)
        ));
        assert_eq!(fixture.requests().await.len(), 1);
        assert_eq!(fixture.row_count().await, 1);
        fixture.stop().await;
    }
}

#[tokio::test]
async fn pinned_execution_drop_releases_fork_without_evicting_source() {
    let capture = capture("continuation", true);
    let fixture = Fixture::new([capture.turns[0].response.clone(), capture.turns[1].response.clone()]).await;
    let group = group();
    let source = group.new_session().unwrap();
    let cancelled = group.new_session().unwrap();
    let surviving = group.new_session().unwrap();
    let first = collect(
        fixture
            .run(request(&capture.turns[0], false), Some(&source))
            .await
            .unwrap(),
    )
    .await;
    let prefix = capture.turns[0].request.body["input"].as_array().unwrap().len() + first.output.len();
    let followup = child(&capture.turns[1], prefix, &first.id, false);
    let stream = fixture
        .run(followup.clone(), Some(&cancelled))
        .await
        .unwrap()
        .right()
        .unwrap();
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(1), cancelled.wait_until_idle())
        .await
        .unwrap()
        .unwrap();
    let restored = rehydrate_in_session(followup.clone(), &fixture.context, &surviving)
        .await
        .unwrap();
    assert_provenance(&restored.enriched_request.input);
    drop(restored);
    let result = collect(fixture.run(followup, Some(&surviving)).await.unwrap()).await;
    assert!(result.output.iter().any(|item| matches!(item, OutputItem::Message(_))));
    assert_eq!(fixture.requests().await.len(), 2);
    assert_eq!(fixture.row_count().await, 0);
    fixture.stop().await;
}

#[tokio::test]
async fn pinned_execution_without_fixture_still_fails_closed() {
    let capture = capture("continuation", false);
    for transport in [
        None,
        Some(crate::executor::inference::transport::ResponsesTransport::opaque().unwrap()),
    ] {
        let mut fixture = Fixture::new([]).await;
        Arc::get_mut(&mut fixture.context).unwrap().opaque_replay_fixture = transport;
        let error = fixture.run(request(&capture.turns[0], true), None).await.err().unwrap();
        assert!(matches!(
            error,
            ExecutorError::ReasoningReplay(ReasoningReplayError::OpaqueNotEnabled)
        ));
        assert!(fixture.requests().await.is_empty());
        fixture.stop().await;
    }
}

#[tokio::test]
async fn pinned_execution_rejects_unsupported_parameters_before_history_or_network() {
    use serde_json::{Value, json};

    let capture = capture("continuation", false);
    let cases = [
        (Field::ReasoningContext, json!({"reasoning":{"context":"all_turns"}})),
        (Field::ReasoningEffort, json!({"reasoning":{"effort":"high"}})),
        (
            Field::ReasoningGenerateSummary,
            json!({"reasoning":{"generate_summary":"concise"}}),
        ),
        (Field::ReasoningMode, json!({"reasoning":{"mode":"pro"}})),
        (Field::ReasoningSummary, json!({"reasoning":{"summary":"detailed"}})),
        (Field::Include, json!({"include":["file_search_call.results"]})),
        (Field::Text, json!({"text":{"format":{"type":"text"}}})),
        (Field::Temperature, json!({"temperature":0.5})),
        (Field::TopP, json!({"top_p":0.5})),
        (Field::MaxOutputTokens, json!({"max_output_tokens":128_001})),
        (Field::IgnoreEos, json!({"ignore_eos":false})),
        (Field::Truncation, json!({"truncation":"auto"})),
        (Field::Metadata, json!({"metadata":{"private":"do-not-forward"}})),
        (Field::ParallelToolCalls, json!({"parallel_tool_calls":true})),
        (Field::CacheSalt, json!({"cache_salt":"vllm-only"})),
        (Field::Tools, json!({"tools":[{"type":"file_search"}]})),
        (
            Field::Tools,
            json!({"tools":[{"type":"function","name":"lookup","defer_loading":true}]}),
        ),
        (
            Field::Tools,
            json!({"tools":[{"type":"function","name":"lookup","future_field":true}]}),
        ),
        (Field::Tools, json!({"tools":[{"type":"mcp","server_label":"fixture"}]})),
        (
            Field::ToolChoice,
            json!({"tool_choice":{"type":"custom","name":"lookup"}}),
        ),
    ];
    let fixture = Fixture::new([]).await;
    for streaming in [false, true] {
        for (field, override_fields) in &cases {
            let mut request = serde_json::to_value(request(&capture.turns[0], true)).unwrap();
            let Value::Object(override_fields) = override_fields else {
                panic!("object override");
            };
            for (key, value) in override_fields {
                request[key.as_str()] = value.clone();
            }
            let mut request: crate::types::RequestPayload = serde_json::from_value(request).unwrap();
            request.stream = streaming;
            request.previous_response_id = Some("resp_missing".to_owned());
            let error = fixture.run(request, None).await.err().unwrap();
            assert!(matches!(
                &error,
                ExecutorError::ReasoningReplay(ReasoningReplayError::UnsupportedParameter(actual)) if actual == field
            ));
            assert_eq!(error.http_status(), http::StatusCode::BAD_REQUEST);
            assert_eq!(error.error_param(), Some(field.as_str()));
        }
    }
    assert!(fixture.requests().await.is_empty());
    assert_eq!(fixture.row_count().await, 0);
    fixture.stop().await;
}
