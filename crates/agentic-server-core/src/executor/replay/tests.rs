use super::*;
use crate::types::io::ReasoningOutput;

#[test]
fn identities_bind_policy_endpoint_and_both_models_without_plaintext_auth() {
    let baseline = upstream_identity(
        ReasoningReplayPolicy::VllmPlaintext,
        "https://provider/v1/responses",
        Some("secret"),
        "alias",
        Some("snapshot"),
    );
    assert_eq!(
        baseline,
        upstream_identity(
            ReasoningReplayPolicy::VllmPlaintext,
            "https://provider/v1/responses",
            Some("secret"),
            "alias",
            Some("snapshot"),
        )
    );
    for changed in [
        upstream_identity(
            ReasoningReplayPolicy::OpaqueResponses,
            "https://provider/v1/responses",
            Some("secret"),
            "alias",
            Some("snapshot"),
        ),
        upstream_identity(
            ReasoningReplayPolicy::VllmPlaintext,
            "https://other/v1/responses",
            Some("secret"),
            "alias",
            Some("snapshot"),
        ),
        upstream_identity(
            ReasoningReplayPolicy::VllmPlaintext,
            "https://provider/v1/responses",
            Some("secret"),
            "other",
            Some("snapshot"),
        ),
        upstream_identity(
            ReasoningReplayPolicy::VllmPlaintext,
            "https://provider/v1/responses",
            Some("secret"),
            "alias",
            Some("other"),
        ),
    ] {
        assert_ne!(baseline, changed);
    }
    for auth in [None, Some(""), Some("other")] {
        assert_eq!(
            baseline,
            upstream_identity(
                ReasoningReplayPolicy::VllmPlaintext,
                "https://provider/v1/responses",
                auth,
                "alias",
                Some("snapshot")
            )
        );
    }
    assert!(!format!("{baseline:?}").contains("secret"));
    assert!(!serde_json::to_string(&baseline).unwrap().contains("secret"));
}

#[test]
fn identity_field_boundaries_and_missing_auth_are_unambiguous() {
    let identity = |auth, requested, reported| {
        upstream_identity(
            ReasoningReplayPolicy::OpaqueResponses,
            "https://provider/v1/responses",
            auth,
            requested,
            reported,
        )
    };
    assert_ne!(identity(None, "a", Some("bc")), identity(Some(""), "a", Some("bc")));
    assert_ne!(identity(None, "a", Some("bc")), identity(None, "ab", Some("c")));
    assert_ne!(identity(None, "a", None), identity(None, "a", Some("")));
    assert_ne!(identity(None, "a", None), identity(None, "a", Some("a")));
}

#[test]
fn manual_submission_never_keeps_or_upgrades_provider_provenance() {
    let identity = upstream_identity(ReasoningReplayPolicy::VllmPlaintext, "endpoint", None, "model", None);
    let mut output = [OutputItem::Reasoning(ReasoningOutput::new("rs_1"))];
    record_upstream_provenance(&mut output, ReasoningReplayPolicy::VllmPlaintext, identity);
    let mut input = ResponsesInput::Items(output.iter().filter_map(OutputItem::to_input_item).collect());
    mark_client_input(&mut input);
    let ResponsesInput::Items(items) = input else {
        panic!("item input")
    };
    let InputItem::Reasoning(item) = &items[0] else {
        panic!("reasoning")
    };
    assert_eq!(item.replay_provenance, Some(ReasoningProvenance::client_submitted()));
    mark_external_output(&mut output);
    let OutputItem::Reasoning(item) = &output[0] else {
        panic!("reasoning")
    };
    assert_eq!(item.replay_provenance, Some(ReasoningProvenance::client_submitted()));
}
