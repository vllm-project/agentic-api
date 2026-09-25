//! Candidate profiles remain fail-closed; these tests never contact a provider.

mod support;

use agentic_core::config::ResponsesConfig;
use agentic_core::executor::{ExecuteRequest, ExecutorError};
use agentic_core::types::reasoning_profile::{OpaqueReasoningProfile, OpaqueReplayRequestField};
use agentic_core::types::reasoning_replay::{ReasoningReplayError, ReasoningReplayPolicy};
use std::sync::Arc;

const PROFILE: OpaqueReasoningProfile = OpaqueReasoningProfile::OpenAiGpt54_20260305V1;

#[test]
fn profile_selection_is_closed_explicit_and_not_execution_authority() {
    let encoded = "\"openai_gpt_5_4_2026_03_05_v1\"";
    assert_eq!(serde_json::to_string(&PROFILE).unwrap(), encoded);
    assert_eq!(
        serde_json::from_str::<OpaqueReasoningProfile>(encoded).unwrap(),
        PROFILE
    );
    for invalid in [
        "\"auto\"",
        "\"gpt-5.4\"",
        "\"openai\"",
        "\"openai_gpt_5_4_2026_03_05_v2\"",
        "null",
    ] {
        assert!(serde_json::from_str::<OpaqueReasoningProfile>(invalid).is_err());
    }
    for (policy, profile, expected) in [
        (ReasoningReplayPolicy::VllmPlaintext, None, Ok(())),
        (
            ReasoningReplayPolicy::VllmPlaintext,
            Some(PROFILE),
            Err(ReasoningReplayError::UnexpectedProfile),
        ),
        (
            ReasoningReplayPolicy::OpaqueResponses,
            None,
            Err(ReasoningReplayError::MissingProfile),
        ),
        (
            ReasoningReplayPolicy::OpaqueResponses,
            Some(PROFILE),
            Err(ReasoningReplayError::OpaqueNotEnabled),
        ),
    ] {
        let config = ResponsesConfig {
            reasoning_replay_policy: policy,
            reasoning_replay_profile: profile,
            ..ResponsesConfig::default()
        };
        assert_eq!(config.validate_reasoning_replay(), expected);
        assert_eq!(config.validate().is_ok(), expected.is_ok());
    }
}

#[tokio::test]
async fn target_rejection_precedes_history_tool_discovery_and_json_or_sse_inference() {
    let fixture = support::TestFixture::new(&[]).await;
    for (base, model, expected) in [
        (
            fixture.exec_ctx.llm_base_url.as_str(),
            PROFILE.model(),
            ReasoningReplayError::EndpointMismatch,
        ),
        ("https://api.openai.com", "gpt-5.4", ReasoningReplayError::ModelMismatch),
        (
            "https://api.openai.com",
            PROFILE.model(),
            ReasoningReplayError::OpaqueNotEnabled,
        ),
    ] {
        for stream in [false, true] {
            let mut context = fixture.exec_ctx.as_ref().clone();
            context.llm_base_url = base.to_owned();
            context.responses_config.reasoning_replay_policy = ReasoningReplayPolicy::OpaqueResponses;
            context.responses_config.reasoning_replay_profile = Some(PROFILE);
            let mut request = support::make_request("continue", true, stream, Some("resp_missing".to_owned()), None);
            request.model = model.to_owned();
            // A missing parent must not be read and this unreachable MCP server
            // must not be discovered when the replay profile is rejected.
            request.tools = Some(
                serde_json::from_value(serde_json::json!([
                    {"type":"mcp", "server_label":"unreachable", "server_url":"http://127.0.0.1:1/mcp", "require_approval":"never"}
                ]))
                .unwrap(),
            );
            let error = ExecuteRequest::new(request, Arc::new(context))
                .run()
                .await
                .err()
                .expect("rejected");
            assert!(matches!(error, ExecutorError::ReasoningReplay(actual) if actual == expected));
        }
    }
    assert!(fixture.request_bodies().await.is_empty());
}

#[test]
fn replay_failures_have_safe_typed_http_and_stream_error_contracts() {
    use http::StatusCode;
    for (failure, status, kind, param) in [
        (
            ReasoningReplayError::ModelMismatch,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some("model"),
        ),
        (
            ReasoningReplayError::UnknownProvenance,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some("input"),
        ),
        (
            ReasoningReplayError::IncompatibleProvenance,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some("input"),
        ),
        (
            ReasoningReplayError::MissingOpaqueState,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some("input"),
        ),
        (
            ReasoningReplayError::UnfinishedReasoning,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some("input"),
        ),
        (
            ReasoningReplayError::MissingCredential,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            None,
        ),
        (
            ReasoningReplayError::UnsupportedCompaction,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            None,
        ),
        (
            ReasoningReplayError::UnsupportedParameter(OpaqueReplayRequestField::ReasoningMode),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some("reasoning.mode"),
        ),
        (
            ReasoningReplayError::ReportedModelMismatch,
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            None,
        ),
        (
            ReasoningReplayError::EndpointMismatch,
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            None,
        ),
        (
            ReasoningReplayError::MissingProfile,
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            None,
        ),
        (
            ReasoningReplayError::UnexpectedProfile,
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            None,
        ),
        (
            ReasoningReplayError::OpaqueNotEnabled,
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            None,
        ),
    ] {
        let error = ExecutorError::from(failure);
        assert_eq!(error.http_status(), status);
        assert_eq!(error.error_type(), kind);
        assert_eq!(error.error_code(), "reasoning_replay_incompatible");
        assert_eq!(error.error_param(), param);
        let wire: serde_json::Value = serde_json::from_slice(&error.into_response_body()).unwrap();
        assert_eq!(wire["error"]["message"], failure.to_string());
        assert_eq!(wire["error"]["type"], kind);
        assert_eq!(wire["error"]["code"], "reasoning_replay_incompatible");
        assert_eq!(wire["error"].get("param").and_then(serde_json::Value::as_str), param);
    }
}
