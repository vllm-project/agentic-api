//! A previous-response branch reads its checkpoint without joining its conversation.

mod support;

use std::num::NonZeroUsize;
use std::sync::Arc;

use agentic_core::executor::ExecuteRequest;
use agentic_core::executor::session::ResponseSession;
use agentic_core::types::RequestPayload;
use either::Either;
use futures::StreamExt;
use serde_json::Value;
use support::{MockResponse, TestFixture, load_cassette, make_request, responses_turns, streamed_sse_event};

async fn run_turn(fixture: &TestFixture, session: Option<&ResponseSession>, request: RequestPayload) -> Value {
    let conversation = request.conversation.clone();
    let execution = ExecuteRequest::new(request, Arc::clone(&fixture.exec_ctx));
    let execution = if let Some(session) = session {
        execution.with_session(session).unwrap()
    } else {
        execution
    };
    match execution.run().await.unwrap() {
        Either::Left(payload) => serde_json::to_value(payload).unwrap(),
        Either::Right(mut stream) => {
            let mut completed = None;
            let mut lifecycle = Vec::new();
            while let Some(chunk) = stream.next().await {
                let Some(event) = streamed_sse_event(&chunk) else {
                    continue;
                };
                if let Some(response) = event.get("response") {
                    lifecycle.push(event["type"].as_str().unwrap().to_owned());
                    assert!(response.get("conversation_id").is_none());
                    if let Some(id) = &conversation {
                        assert_eq!(response["conversation"]["id"], *id);
                    } else {
                        assert!(response.get("conversation").is_none(), "{event}");
                    }
                }
                if event["type"] == "response.completed" {
                    completed = Some(event["response"].clone());
                }
            }
            assert_eq!(
                lifecycle,
                ["response.created", "response.in_progress", "response.completed"]
            );
            completed.expect("completed branch response")
        }
    }
}

async fn assert_branch_is_detached(streaming: bool) {
    let scenario = if streaming { "branch-stream" } else { "branch" };
    let cassette = load_cassette(&format!(
        "{}/tests/cassettes/conversations/conversations-{scenario}-openai.yaml",
        env!("CARGO_MANIFEST_DIR")
    ));
    let turns = responses_turns(&cassette);
    // Exercise durable lookup directly and through a transient session.
    for use_session in [false, true] {
        for store in [false, true] {
            // A fresh session only loads a durable parent when storage is requested.
            if use_session && !store {
                continue;
            }
            let fixture = TestFixture::new_with_responses(vec![
                MockResponse::from_turn(turns[0]),
                MockResponse::from_turn(turns[1]),
            ])
            .await;
            let session = ResponseSession::new(NonZeroUsize::new(128).unwrap(), NonZeroUsize::new(65_536).unwrap());
            let conversation_id = fixture
                .exec_ctx
                .conv_handler
                .create_with_metadata_and_items("default_tenant", None, Vec::new())
                .await
                .unwrap()
                .conversation_id;
            let parent = run_turn(
                &fixture,
                None,
                make_request(
                    &turns[0].request.body.input,
                    true,
                    streaming,
                    None,
                    Some(conversation_id.clone()),
                ),
            )
            .await;
            assert_eq!(parent["conversation"]["id"], conversation_id);
            let before = fixture
                .exec_ctx
                .conv_handler
                .list_items("default_tenant", &conversation_id, 100, None, "asc")
                .await
                .unwrap();
            let parent_id = parent["id"].as_str().unwrap();
            let branch = run_turn(
                &fixture,
                use_session.then_some(&session),
                make_request(
                    &turns[1].request.body.input,
                    store,
                    streaming,
                    Some(parent_id.to_owned()),
                    None,
                ),
            )
            .await;
            assert_eq!(branch["status"], "completed");
            assert_eq!(branch["previous_response_id"], parent_id);
            assert!(branch.get("conversation").is_none(), "{branch}");
            assert!(branch.get("conversation_id").is_none());
            let after = fixture
                .exec_ctx
                .conv_handler
                .list_items("default_tenant", &conversation_id, 100, None, "asc")
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_value(before).unwrap(),
                serde_json::to_value(after).unwrap()
            );
            if store {
                let retrieved = fixture
                    .exec_ctx
                    .resp_handler
                    .retrieve(branch["id"].as_str().unwrap())
                    .await
                    .unwrap();
                assert_eq!(serde_json::to_value(retrieved).unwrap(), branch);
            }
            let requests = fixture.request_bodies().await;
            let branch_input = serde_json::to_string(&requests[1]["input"]).unwrap();
            assert!(branch_input.contains("SAPPHIRE"), "parent history was not restored");
        }
    }
}

#[tokio::test]
async fn previous_response_branch_omits_conversation_in_json_and_retrieval() {
    assert_branch_is_detached(false).await;
}

#[tokio::test]
async fn previous_response_branch_omits_conversation_in_every_stream_response() {
    assert_branch_is_detached(true).await;
}
