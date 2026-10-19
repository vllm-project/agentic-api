//! Request members the gateway recognizes but cannot honour are rejected at
//! admission, before any upstream call or storage work, on every core entry
//! point: the executor in both stream modes and `rehydrate_conversation`,
//! which split execution and embedders call directly.

use std::sync::Arc;

use agentic_core::executor::{ExecuteRequest, ExecutorError, rehydrate_conversation};
use agentic_core::types::request_response::{JsonKind, RequestPayload, UnsupportedParameterError};
use serde_json::{Value, json};

mod support;
use support::{TestFixture, text_response};

fn request(extra: &Value) -> RequestPayload {
    let mut body = json!({"model": "test-model", "input": "hi"});
    body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    serde_json::from_value(body).unwrap()
}

fn cases() -> [(Value, UnsupportedParameterError); 6] {
    [
        (json!({"background": true}), UnsupportedParameterError::BackgroundMode),
        (
            json!({"background": "false"}),
            UnsupportedParameterError::BackgroundType(JsonKind::String),
        ),
        (
            json!({"background": 0, "stream": true}),
            UnsupportedParameterError::BackgroundType(JsonKind::Integer),
        ),
        (
            json!({"background": true, "stream": true}),
            UnsupportedParameterError::BackgroundMode,
        ),
        (
            json!({"prompt": {"id": "pmpt_1", "version": "2"}}),
            UnsupportedParameterError::PromptTemplate,
        ),
        (
            json!({"prompt": {"id": "pmpt_1"}, "stream": true}),
            UnsupportedParameterError::PromptTemplate,
        ),
    ]
}

#[tokio::test]
async fn execution_rejects_unsupported_members_before_any_upstream_call() {
    for (extra, expected) in cases() {
        let fixture = TestFixture::new_with_responses(vec![text_response("unused")]).await;

        let result = Box::pin(ExecuteRequest::new(request(&extra), Arc::clone(&fixture.exec_ctx)).run()).await;

        let Err(error) = result else {
            panic!("{extra} was executed");
        };
        assert!(
            matches!(error, ExecutorError::UnsupportedParameter(found) if found == expected),
            "{extra}: {error:?}"
        );
        assert!(
            fixture.request_bodies().await.is_empty(),
            "{extra} reached the upstream"
        );
    }
}

#[tokio::test]
async fn rehydration_rejects_unsupported_members() {
    for (extra, expected) in cases() {
        let fixture = TestFixture::new_with_responses(Vec::new()).await;

        let result = rehydrate_conversation(request(&extra), &fixture.exec_ctx).await;

        let Err(error) = result else {
            panic!("{extra} was admitted");
        };
        assert!(
            matches!(error, ExecutorError::UnsupportedParameter(found) if found == expected),
            "{extra}: {error:?}"
        );
    }
}

#[tokio::test]
async fn explicit_defaults_execute_and_are_not_forwarded() {
    let fixture = TestFixture::new_with_responses(vec![text_response("ok")]).await;

    let result = Box::pin(
        ExecuteRequest::new(
            request(&json!({"background": false, "prompt": null})),
            Arc::clone(&fixture.exec_ctx),
        )
        .run(),
    )
    .await;

    assert!(result.is_ok(), "explicit defaults are accepted");
    let upstream = fixture.request_bodies().await;
    assert_eq!(upstream.len(), 1);
    assert!(upstream[0].get("background").is_none(), "{}", upstream[0]);
    assert!(upstream[0].get("prompt").is_none(), "{}", upstream[0]);
}
