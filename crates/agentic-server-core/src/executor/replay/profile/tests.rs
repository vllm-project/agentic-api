use super::*;
use crate::config::ResponsesConfig;
use crate::executor::error::ExecutorError;
use crate::executor::modes::{ConversationHandler, ResponseHandler};
use crate::executor::replay::{preflight_inference, record_round_provenance, validate_rehydration_request};
use crate::executor::request::ExecutionContext;
use crate::storage::{ConversationStore, ResponseStore};
use crate::types::{OpaqueReasoning, OutputItem, ReasoningOutput, RequestPayload};
use std::sync::Arc;

const PROFILE: OpaqueReasoningProfile = OpaqueReasoningProfile::OpenAiGpt54_20260305V1;
const SECRET: &str = "credential-must-not-appear";

fn target(auth: &str) -> OpaqueReplayTarget {
    OpaqueReplayTarget::new(PROFILE, PROFILE.endpoint(), PROFILE.model(), Some(auth)).unwrap()
}

fn canonical_input(target: &OpaqueReplayTarget) -> ResponsesInput {
    let mut reasoning = ReasoningOutput::new("rs_secret_id");
    reasoning.encrypted_content = Some(OpaqueReasoning::try_from(" \nopaque-secret-原文\\==\t".to_owned()).unwrap());
    reasoning.replay_provenance = Some(ReasoningProvenance::upstream(
        ReasoningReplayPolicy::OpaqueResponses,
        target
            .observed_identity(Some(&UpstreamModelId::new(PROFILE.model().to_owned()).unwrap()))
            .unwrap(),
    ));
    ResponsesInput::Items(vec![InputItem::Reasoning(reasoning)])
}

fn reasoning_mut(input: &mut ResponsesInput) -> &mut ReasoningOutput {
    let ResponsesInput::Items(items) = input else {
        panic!("items")
    };
    let InputItem::Reasoning(reasoning) = &mut items[0] else {
        panic!("reasoning")
    };
    reasoning
}

fn context() -> ExecutionContext {
    ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        "https://api.openai.com".to_owned(),
    )
    .with_responses_config(ResponsesConfig {
        reasoning_replay_policy: ReasoningReplayPolicy::OpaqueResponses,
        reasoning_replay_profile: Some(PROFILE),
        ..ResponsesConfig::default()
    })
}

fn request() -> RequestPayload {
    serde_json::from_value(serde_json::json!({"model": PROFILE.model(), "input": "hello"})).unwrap()
}

#[test]
fn pinned_request_surface_accepts_recorded_settings_without_opening_the_gate() {
    let request: RequestPayload = serde_json::from_value(serde_json::json!({
        "model": PROFILE.model(),
        "input": "hello",
        "reasoning": {"effort": "low", "summary": "concise"},
        "include": ["reasoning.encrypted_content"],
        "max_output_tokens": 128_000,
        "truncation": "disabled",
        "parallel_tool_calls": false,
        "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}],
        "tool_choice": {"type": "function", "name": "lookup"}
    }))
    .unwrap();
    assert!(matches!(
        validate_rehydration_request(&context(), &request),
        Err(ExecutorError::ReasoningReplay(ReasoningReplayError::OpaqueNotEnabled))
    ));
}

#[test]
fn prompt_cache_key_is_rejected_by_typed_opaque_preflight() {
    let mut request = request();
    request.prompt_cache_key = Some("workspace-a".to_owned());
    assert!(matches!(
        validate_rehydration_request(&context(), &request),
        Err(ExecutorError::ReasoningReplay(
            ReasoningReplayError::UnsupportedParameter(
                crate::types::reasoning_profile::OpaqueReplayRequestField::PromptCacheKey
            )
        ))
    ));
}

#[test]
fn unknown_input_item_is_rejected_before_the_closed_profile_gate() {
    let request: RequestPayload = serde_json::from_value(serde_json::json!({
        "model": PROFILE.model(),
        "input": [{"type": "unqualified_future_item", "secret": "not forwarded"}]
    }))
    .unwrap();
    assert!(matches!(
        validate_rehydration_request(&context(), &request),
        Err(ExecutorError::ReasoningReplay(
            ReasoningReplayError::UnsupportedParameter(
                crate::types::reasoning_profile::OpaqueReplayRequestField::Input
            )
        ))
    ));
}

#[test]
fn unqualified_nested_input_is_rejected_before_the_closed_profile_gate() {
    let inputs = [
        serde_json::json!([{"type":"message","role":"system","content":"hi"}]),
        serde_json::json!([{"type":"message","role":"user","content":[{"type":"output_text","text":"hi"}]}]),
        serde_json::json!([{"type":"message","role":"assistant","content":[{"type":"input_text","text":"hi"}]}]),
        serde_json::json!([{"type":"message","role":"user","content":[{"type":"input_text","text":"hi","extra":1}]}]),
        serde_json::json!([{"type":"message","role":"user","content":[{"type":"input_image","image_url":"https://example.test/image"}]}]),
        serde_json::json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi","extra":1}]}]),
        serde_json::json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi","logprobs":[{"token":"hi"}]}]}]),
        serde_json::json!([{"type":"reasoning","content":[{"type":"reasoning_text","text":"private"}],"encrypted_content":"opaque"}]),
        serde_json::json!([{"type":"function_call","call_id":"c","name":"f","namespace":"foreign","arguments":"{}"}]),
        serde_json::json!([{"type":"function_call_output","call_id":"c","output":[{"type":"input_text","text":"hi"}]}]),
        serde_json::json!([{"type":"custom_tool_call_output","call_id":"c","output":"hi"}]),
    ];
    for input in inputs {
        let request: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": PROFILE.model(),
            "input": input
        }))
        .unwrap();
        assert!(matches!(
            validate_rehydration_request(&context(), &request),
            Err(ExecutorError::ReasoningReplay(
                ReasoningReplayError::UnsupportedParameter(
                    crate::types::reasoning_profile::OpaqueReplayRequestField::Input
                )
            ))
        ));
    }
}

#[test]
fn direct_core_calls_obey_the_same_profile_item_caps_as_wire_requests() {
    use crate::types::io::{InputContent, InputMessage, InputMessageContent, InputTextContent};
    use crate::types::reasoning_profile::{
        MAX_OPAQUE_CONTENT_PARTS, MAX_OPAQUE_INPUT_ITEMS, MAX_OPAQUE_REASONING_SUMMARIES, MAX_OPAQUE_TOOLS,
    };

    let assert_field = |request: &RequestPayload, field| {
        assert!(matches!(
            validate_rehydration_request(&context(), request),
            Err(ExecutorError::ReasoningReplay(ReasoningReplayError::UnsupportedParameter(actual))) if actual == field
        ));
    };

    let mut oversized = request();
    let message = InputItem::Message(InputMessage::new("user", InputMessageContent::Text("x".to_owned())));
    oversized.input = ResponsesInput::Items(vec![message; MAX_OPAQUE_INPUT_ITEMS + 1]);
    assert_field(
        &oversized,
        crate::types::reasoning_profile::OpaqueReplayRequestField::Input,
    );

    let mut oversized = request();
    let part = InputContent::InputText(InputTextContent::new("x"));
    oversized.input = ResponsesInput::Items(vec![InputItem::Message(InputMessage::new(
        "user",
        InputMessageContent::Parts(vec![part; MAX_OPAQUE_CONTENT_PARTS + 1]),
    ))]);
    assert_field(
        &oversized,
        crate::types::reasoning_profile::OpaqueReplayRequestField::Input,
    );

    let mut oversized = request();
    let mut reasoning = ReasoningOutput::new("rs_1");
    reasoning.summary =
        vec![crate::types::io::ReasoningSummaryContent::new("brief"); MAX_OPAQUE_REASONING_SUMMARIES + 1];
    oversized.input = ResponsesInput::Items(vec![InputItem::Reasoning(reasoning)]);
    assert_field(
        &oversized,
        crate::types::reasoning_profile::OpaqueReplayRequestField::Input,
    );

    let mut oversized = request();
    let tool = serde_json::from_value(serde_json::json!({"type":"function","name":"lookup"})).unwrap();
    oversized.tools = Some(vec![tool; MAX_OPAQUE_TOOLS + 1]);
    assert_field(
        &oversized,
        crate::types::reasoning_profile::OpaqueReplayRequestField::Tools,
    );
}

#[test]
fn target_requires_exact_endpoint_model_and_effective_credential() {
    for endpoint in [
        "http://api.openai.com/v1/responses",
        "https://api.openai.com/v1/responses/",
        "https://api.openai.com:443/v1/responses",
        "https://api.openai.com/v1/responses?key=secret",
        "https://secret@api.openai.com/v1/responses",
        "https://api.openai.com.attacker.invalid/v1/responses",
        "https://api.openai.com/other/v1/responses",
    ] {
        assert!(matches!(
            OpaqueReplayTarget::new(PROFILE, endpoint, PROFILE.model(), Some(SECRET)),
            Err(ReasoningReplayError::EndpointMismatch)
        ));
    }
    for model in ["gpt-5.4", "gpt-5.6", "gpt-5.4-2026-03-05 ", "secret-model"] {
        assert!(matches!(
            OpaqueReplayTarget::new(PROFILE, PROFILE.endpoint(), model, Some(SECRET)),
            Err(ReasoningReplayError::ModelMismatch)
        ));
    }
    for auth in [None, Some(""), Some(" \t\n")] {
        assert!(matches!(
            OpaqueReplayTarget::new(PROFILE, PROFILE.endpoint(), PROFILE.model(), auth),
            Err(ReasoningReplayError::MissingCredential)
        ));
    }
}

#[test]
fn exact_terminal_evidence_is_required_to_stamp_profile_provenance() {
    let target = target(SECRET);
    assert_eq!(
        target.observed_identity(None),
        Err(ReasoningReplayError::ReportedModelMismatch)
    );
    for model in ["gpt-5.4", "gpt-5.6", "gpt-5.4-2026-03-05 "] {
        let model = UpstreamModelId::new(model.to_owned()).unwrap();
        assert_eq!(
            target.observed_identity(Some(&model)),
            Err(ReasoningReplayError::ReportedModelMismatch)
        );
    }
    let model = UpstreamModelId::new(PROFILE.model().to_owned()).unwrap();
    assert_eq!(target.observed_identity(Some(&model)), Ok(target.identity));
}

#[test]
fn identity_is_profile_bound_and_credential_rotation_fails_closed() {
    let baseline = target(SECRET);
    let input = canonical_input(&baseline);
    assert_eq!(target(SECRET).validate_input(&input), Ok(()));
    assert_eq!(
        target("rotated").validate_input(&input),
        Err(ReasoningReplayError::IncompatibleProvenance)
    );
    for policy in [
        ReasoningReplayPolicy::VllmPlaintext,
        ReasoningReplayPolicy::OpaqueResponses,
    ] {
        for model in [None, Some(PROFILE.model())] {
            let mut legacy = input.clone();
            reasoning_mut(&mut legacy).replay_provenance = Some(ReasoningProvenance::upstream(
                policy,
                upstream_identity(policy, PROFILE.endpoint(), Some(SECRET), PROFILE.model(), model),
            ));
            assert_eq!(
                baseline.validate_input(&legacy),
                Err(ReasoningReplayError::IncompatibleProvenance)
            );
        }
    }
    let diagnostic = format!("{baseline:?}");
    assert!(!diagnostic.contains(SECRET));
    assert!(!diagnostic.contains("opaque-secret"));
}

#[test]
fn manual_legacy_and_missing_opaque_state_are_not_upgraded() {
    let target = target(SECRET);
    for origin in [None, Some(ReasoningProvenance::client_submitted())] {
        let mut input = canonical_input(&target);
        reasoning_mut(&mut input).replay_provenance = origin;
        assert_eq!(
            target.validate_input(&input),
            Err(ReasoningReplayError::UnknownProvenance)
        );
    }
    for state in [None, Some(OpaqueReasoning::try_from(String::new()).unwrap())] {
        let mut input = canonical_input(&target);
        reasoning_mut(&mut input).encrypted_content = state;
        assert_eq!(
            target.validate_input(&input),
            Err(ReasoningReplayError::MissingOpaqueState)
        );
    }
    let mut input = canonical_input(&target);
    reasoning_mut(&mut input).status = Some(ReasoningStatus::InProgress);
    assert_eq!(
        target.validate_input(&input),
        Err(ReasoningReplayError::UnfinishedReasoning)
    );
}

#[test]
fn checks_leave_canonical_bytes_and_call_order_unchanged_even_on_failure() {
    let target = target(SECRET);
    let mut input = canonical_input(&target);
    let ResponsesInput::Items(items) = &mut input else {
        panic!("items")
    };
    let tail: Vec<InputItem> = serde_json::from_value(serde_json::json!([
        {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"},
        {"type":"function_call_output","call_id":"call_1","output":"done"}
    ]))
    .unwrap();
    items.extend(tail);
    let original = serde_json::to_vec(&input).unwrap();
    assert_eq!(target.validate_input(&input), Ok(()));
    assert_eq!(serde_json::to_vec(&input).unwrap(), original);
    let ResponsesInput::Items(items) = &mut input else {
        panic!("items")
    };
    items.push(InputItem::Reasoning(ReasoningOutput::new("untrusted_last_item")));
    let original = serde_json::to_vec(&input).unwrap();
    assert_eq!(
        target.validate_input(&input),
        Err(ReasoningReplayError::UnknownProvenance)
    );
    assert_eq!(serde_json::to_vec(&input).unwrap(), original);
}

#[test]
fn preflight_rejects_compaction_before_model_input_can_hide_it() {
    let context = context();
    for input in [
        serde_json::json!([{"type":"compaction_trigger"}]),
        serde_json::json!([{"type":"compaction","id":"cmp_1","encrypted_content":"plaintext-secret"}]),
    ] {
        let mut request = request();
        request.input = serde_json::from_value(input).unwrap();
        assert!(matches!(
            preflight_inference(&context, &request, Some(SECRET)),
            Err(ExecutorError::ReasoningReplay(
                ReasoningReplayError::UnsupportedCompaction
            ))
        ));
        assert_eq!(
            target(SECRET).validate_input(&request.input),
            Err(ReasoningReplayError::UnsupportedCompaction)
        );
    }
    let mut request = request();
    request.context_management = Some(
        serde_json::from_value(serde_json::json!([
            {"type":"compaction","compact_threshold":1000}
        ]))
        .unwrap(),
    );
    assert!(matches!(
        validate_rehydration_request(&context, &request),
        Err(ExecutorError::ReasoningReplay(
            ReasoningReplayError::UnsupportedCompaction
        ))
    ));
}

#[test]
fn compatibility_never_bypasses_the_qualification_gate() {
    let context = context();
    let mut request = request();
    request.input = canonical_input(&target(SECRET));
    assert!(matches!(
        preflight_inference(&context, &request, Some(SECRET)),
        Err(ExecutorError::ReasoningReplay(ReasoningReplayError::OpaqueNotEnabled))
    ));
    assert!(matches!(
        preflight_inference(&context, &request, Some("rotated")),
        Err(ExecutorError::ReasoningReplay(
            ReasoningReplayError::IncompatibleProvenance
        ))
    ));
    assert!(matches!(
        validate_rehydration_request(&context, &request),
        Err(ExecutorError::ReasoningReplay(ReasoningReplayError::OpaqueNotEnabled))
    ));
}

#[tokio::test]
async fn round_annotations_use_profile_identity_and_fail_atomically() {
    let context = context();
    // Build context through normal vLLM rehydration, then exercise the I/O-free
    // observation boundary directly. This is not a provider qualification test.
    let mut default_context = context.clone();
    default_context.responses_config = ResponsesConfig::default();
    let request = crate::executor::rehydrate::rehydrate_conversation(request(), &default_context)
        .await
        .unwrap();
    let mut payload: crate::types::ResponsePayload = serde_json::from_value(serde_json::json!({
        "id":"resp_test", "object":"response", "model":PROFILE.model(), "created_at":0,
        "status":"completed", "output":[
            {"type":"reasoning","id":"rs_1","encrypted_content":"opaque-secret"},
            {"type":"reasoning","id":"rs_2","encrypted_content":"second-secret"}
        ]
    }))
    .unwrap();
    assert!(record_round_provenance(&mut payload, None, &context, &request, Some(SECRET)).is_err());
    assert!(
        payload
            .output
            .iter()
            .all(|item| matches!(item, OutputItem::Reasoning(r) if r.replay_provenance.is_none()))
    );
    let model = UpstreamModelId::new(PROFILE.model().to_owned()).unwrap();
    record_round_provenance(&mut payload, Some(&model), &context, &request, Some(SECRET)).unwrap();
    let input = ResponsesInput::Items(payload.output.iter().filter_map(OutputItem::to_input_item).collect());
    assert_eq!(target(SECRET).validate_input(&input), Ok(()));
    assert!(serde_json::to_string(&payload).unwrap().contains("opaque-secret"));
    assert!(!serde_json::to_string(&payload).unwrap().contains("replay_provenance"));
}

#[tokio::test]
async fn a_round_without_reasoning_still_requires_the_pinned_reported_model() {
    let context = context();
    let mut default_context = context.clone();
    default_context.responses_config = ResponsesConfig::default();
    let request = crate::executor::rehydrate::rehydrate_conversation(request(), &default_context)
        .await
        .unwrap();
    let mut payload: crate::types::ResponsePayload = serde_json::from_value(serde_json::json!({
        "id":"resp_test", "object":"response", "model":PROFILE.model(), "created_at":0,
        "status":"completed", "output":[]
    }))
    .unwrap();
    for reported in [None, Some(UpstreamModelId::new("wrong-model".into()).unwrap())] {
        assert!(matches!(
            record_round_provenance(&mut payload, reported.as_ref(), &context, &request, Some(SECRET)),
            Err(ExecutorError::ReasoningReplay(
                ReasoningReplayError::ReportedModelMismatch
            ))
        ));
    }
    let model = UpstreamModelId::new(PROFILE.model().into()).unwrap();
    record_round_provenance(&mut payload, Some(&model), &context, &request, Some(SECRET)).unwrap();
}
