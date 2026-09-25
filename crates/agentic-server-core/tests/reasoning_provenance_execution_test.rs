//! Recorder-generated exchanges exercise provenance, not opaque replay qualification.

mod support;

use std::num::NonZeroUsize;
use std::sync::Arc;

use agentic_core::config::ResponsesConfig;
use agentic_core::executor::{
    ExecuteRequest, ExecutorError, ResponseSessionGroup, commit, rehydrate_conversation, rehydrate_in_session,
};
use agentic_core::storage::InOutItem;
use agentic_core::types::io::{InputItem, OutputItem, ReasoningOutput, ResponsesInput};
use agentic_core::types::reasoning_replay::{
    ReasoningProvenance, ReasoningReplayError, ReasoningReplayPolicy, ReasoningSource,
};
use agentic_core::types::request_response::RequestPayload;

fn cassette(model: &str, streaming: bool) -> support::Cassette {
    let mode = if streaming { "streaming" } else { "nonstreaming" };
    support::load_cassette(&format!(
        "{}/tests/cassettes/reasoning/responses/reasoning-{model}-{mode}.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))
}

fn request(turn: &support::Turn, store: bool) -> RequestPayload {
    serde_json::from_value(serde_json::json!({
        "model": turn.request.body.model,
        "input": turn.request.body.input,
        "store": store,
        "stream": turn.request.body.stream,
        "max_output_tokens": turn.request.body.max_output_tokens,
        "reasoning": turn.request.body.reasoning,
    }))
    .unwrap()
}

fn reasoning(items: &[InputItem]) -> Vec<&ReasoningOutput> {
    items
        .iter()
        .filter_map(|item| match item {
            InputItem::Reasoning(reasoning) => Some(reasoning),
            _ => None,
        })
        .collect()
}

fn assert_upstream(provenance: Option<ReasoningProvenance>) {
    assert!(matches!(
        provenance,
        Some(ReasoningProvenance::V1 {
            source: ReasoningSource::Upstream {
                policy: ReasoningReplayPolicy::VllmPlaintext,
                ..
            }
        })
    ));
}

#[tokio::test]
async fn recorded_json_and_sse_stamp_only_internal_history() {
    for model in ["single-Qwen-Qwen3-30B-A3B-FP8", "openai-reference-gpt-5.6"] {
        for streaming in [false, true] {
            let cassette = cassette(model, streaming);
            let turn = &cassette.turns[0];
            let fixture = support::TestFixture::new(&[turn]).await;
            let result = ExecuteRequest::new(request(turn, true), Arc::clone(&fixture.exec_ctx))
                .with_auth(Some("test-only-credential".to_owned()))
                .run()
                .await
                .unwrap();
            let response = if streaming {
                support::collect_stream(result).await
            } else {
                support::unwrap_blocking(result)
            };
            let ctx = rehydrate_conversation(
                support::make_request("continue", true, false, Some(response.id.clone()), None),
                &fixture.exec_ctx,
            )
            .await
            .unwrap();
            let history = fixture.exec_ctx.resp_handler.rehydrate(&ctx).await.unwrap();
            let items = InOutItem::into_input_items(history);
            let stored = reasoning(&items);
            assert!(!stored.is_empty());
            for item in stored {
                assert_upstream(item.replay_provenance);
                let public = response
                    .output
                    .iter()
                    .find_map(|output| match output {
                        OutputItem::Reasoning(reasoning) if reasoning.id == item.id => Some(reasoning),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(item.encrypted_content, public.encrypted_content);
                assert_eq!(item.content, public.content);
                assert_eq!(item.summary, public.summary);
                if !streaming {
                    assert_eq!(item.replay_provenance, public.replay_provenance);
                }
            }
            let wire = serde_json::to_string(&response).unwrap();
            assert!(!wire.contains("replay_provenance"));
            assert!(!wire.contains("test-only-credential"));
            assert_eq!(fixture.request_bodies().await.len(), 1);
        }
    }
}

#[tokio::test]
async fn session_forks_promotion_and_external_commit_preserve_each_items_origin() {
    for streaming in [false, true] {
        let cassette = cassette("single-Qwen-Qwen3-30B-A3B-FP8", streaming);
        let turn = &cassette.turns[0];
        let fixture = support::TestFixture::new(&[turn]).await;
        let group = ResponseSessionGroup::new(
            NonZeroUsize::new(4).unwrap(),
            NonZeroUsize::new(100).unwrap(),
            NonZeroUsize::new(1_000_000).unwrap(),
            NonZeroUsize::new(4_000_000).unwrap(),
        );
        let source = group.new_session().unwrap();
        let target = group.new_session().unwrap();
        let cancelled = group.new_session().unwrap();
        let result = ExecuteRequest::new(request(turn, false), Arc::clone(&fixture.exec_ctx))
            .with_session(&source)
            .unwrap()
            .run()
            .await
            .unwrap();
        let response = if streaming {
            support::collect_stream(result).await
        } else {
            support::unwrap_blocking(result)
        };
        let followup = || support::make_request("continue", true, false, Some(response.id.clone()), None);
        let cancelled_ctx = rehydrate_in_session(followup(), &fixture.exec_ctx, &cancelled)
            .await
            .unwrap();
        drop(cancelled_ctx); // Cancelling one fork must not erase another member's parent.
        let ctx = rehydrate_in_session(followup(), &fixture.exec_ctx, &target)
            .await
            .unwrap();
        let ResponsesInput::Items(parent_items) = &ctx.enriched_request.input else {
            panic!("item history")
        };
        let parent = reasoning(parent_items);
        assert_eq!(parent.len(), 1);
        let provenance = parent[0].replay_provenance;
        assert_upstream(provenance);
        let mut externally_completed = response.clone();
        externally_completed.id.clone_from(&ctx.response_id);
        // An external completion has its own upstream item IDs; stored rows keep them.
        for item in &mut externally_completed.output {
            match item {
                OutputItem::Reasoning(item) => {
                    item.replay_provenance = provenance;
                    item.id.push_str("_external");
                }
                OutputItem::Message(message) => message.id.push_str("_external"),
                _ => {}
            }
        }
        let committed = commit(ctx, externally_completed, &fixture.exec_ctx).await.unwrap();
        let stored_ctx = rehydrate_conversation(
            support::make_request("continue", true, false, Some(committed.id.clone()), None),
            &fixture.exec_ctx,
        )
        .await
        .unwrap();
        let items = InOutItem::into_input_items(fixture.exec_ctx.resp_handler.rehydrate(&stored_ctx).await.unwrap());
        let stored = reasoning(&items);
        assert_eq!(stored.len(), 2);
        assert_eq!(
            stored[0].replay_provenance, provenance,
            "promotion preserves provider origin"
        );
        assert_eq!(
            stored[1].replay_provenance,
            Some(ReasoningProvenance::client_submitted())
        );
        // Re-submitting a provider item manually must not launder its origin.
        let mut manual = followup();
        manual.previous_response_id = None;
        manual.input = ResponsesInput::Items(vec![InputItem::Reasoning(stored[0].clone())]);
        let manual_ctx = rehydrate_conversation(manual, &fixture.exec_ctx).await.unwrap();
        assert_eq!(
            reasoning(&manual_ctx.new_input_items)[0].replay_provenance,
            Some(ReasoningProvenance::client_submitted())
        );
        assert_eq!(fixture.request_bodies().await.len(), 1, "split commit does not infer");
    }
}

#[tokio::test]
async fn opaque_policy_without_profile_fails_before_history_lookup_or_inference() {
    let fixture = support::TestFixture::new(&[]).await;
    let context = Arc::new(
        fixture
            .exec_ctx
            .as_ref()
            .clone()
            .with_responses_config(ResponsesConfig {
                reasoning_replay_policy: ReasoningReplayPolicy::OpaqueResponses,
                ..ResponsesConfig::default()
            }),
    );
    for streaming in [false, true] {
        let result = ExecuteRequest::new(
            support::make_request("continue", true, streaming, Some("resp_missing".to_owned()), None),
            Arc::clone(&context),
        )
        .run()
        .await;
        assert!(matches!(
            result,
            Err(ExecutorError::ReasoningReplay(ReasoningReplayError::MissingProfile))
        ));
    }
    assert!(fixture.request_bodies().await.is_empty());

    let request = support::make_request("hello", true, false, None, None);
    let ctx = rehydrate_conversation(request, &fixture.exec_ctx).await.unwrap();
    let response = serde_json::from_value(serde_json::json!({
        "id": ctx.response_id, "object": "response", "status": "completed", "output": [], "model": "test-model", "created_at": 0
    }))
    .unwrap();
    assert!(matches!(
        commit(ctx, response, &context).await,
        Err(ExecutorError::ReasoningReplay(ReasoningReplayError::MissingProfile))
    ));
    let request = serde_json::from_value(serde_json::json!({"model":"test-model", "input":"compact me"})).unwrap();
    assert!(matches!(
        agentic_core::executor::compact_response(request, &context, None).await,
        Err(ExecutorError::ReasoningReplay(ReasoningReplayError::MissingProfile))
    ));
    assert!(fixture.request_bodies().await.is_empty());
}

#[tokio::test]
async fn split_commit_reclassifies_inputs_after_their_wire_round_trip() {
    let cassette = cassette("single-Qwen-Qwen3-30B-A3B-FP8", false);
    let turn = &cassette.turns[0];
    let fixture = support::TestFixture::new(&[]).await;
    let mut submitted = request(turn, true);
    submitted.input = ResponsesInput::Items(vec![InputItem::Reasoning(ReasoningOutput::new("rs_manual"))]);
    let mut ctx = rehydrate_conversation(submitted, &fixture.exec_ctx).await.unwrap();
    // Split contexts cross a public wire boundary, which deliberately strips provenance.
    ctx.new_input_items = serde_json::from_str(&serde_json::to_string(&ctx.new_input_items).unwrap()).unwrap();
    assert!(reasoning(&ctx.new_input_items)[0].replay_provenance.is_none());
    let mut output: agentic_core::types::request_response::ResponsePayload =
        serde_json::from_value(turn.response.body.clone().unwrap()).unwrap();
    output.id.clone_from(&ctx.response_id);
    let response = commit(ctx, output, &fixture.exec_ctx).await.unwrap();
    let ctx = rehydrate_conversation(
        support::make_request("continue", true, false, Some(response.id), None),
        &fixture.exec_ctx,
    )
    .await
    .unwrap();
    let stored = InOutItem::into_input_items(fixture.exec_ctx.resp_handler.rehydrate(&ctx).await.unwrap());
    let stored = reasoning(&stored);
    assert_eq!(stored.len(), 2);
    for item in stored {
        assert_eq!(item.replay_provenance, Some(ReasoningProvenance::client_submitted()));
    }
    assert!(fixture.request_bodies().await.is_empty());
}
