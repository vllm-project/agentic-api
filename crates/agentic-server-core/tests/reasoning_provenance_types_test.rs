//! Provenance is bounded internal state, never a client-controlled wire field.

use agentic_core::types::io::{InputItem, OutputItem, ReasoningOutput};
use agentic_core::types::reasoning_replay::{
    MAX_REASONING_PROVENANCE_BYTES, ReasoningProvenance, ReasoningReplayError, ReasoningReplayPolicy,
};

#[test]
fn opaque_policy_is_recognized_but_not_executable() {
    assert_eq!(ReasoningReplayPolicy::default(), ReasoningReplayPolicy::VllmPlaintext);
    assert!(ReasoningReplayPolicy::default().validate().is_ok());
    let policy: ReasoningReplayPolicy = serde_json::from_str("\"opaque_responses\"").unwrap();
    assert_eq!(policy.validate(), Err(ReasoningReplayError::OpaqueNotEnabled));
    assert!(serde_json::from_str::<ReasoningReplayPolicy>("\"auto\"").is_err());
}

#[test]
fn provenance_versions_and_identity_size_are_closed() {
    let mut valid = serde_json::json!({
        "version": "1",
        "source": {"origin": "upstream", "policy": "vllm_plaintext", "identity": ([255; 32].to_vec())}
    });
    let provenance: ReasoningProvenance = serde_json::from_value(valid.clone()).unwrap();
    let encoded = serde_json::to_string(&provenance).unwrap();
    assert!(encoded.len() <= MAX_REASONING_PROVENANCE_BYTES);
    assert_eq!(
        serde_json::from_str::<ReasoningProvenance>(&encoded).unwrap(),
        provenance
    );
    assert!(format!("{provenance:?}").contains("<redacted>"));
    assert!(!format!("{provenance:?}").contains("255"));

    for invalid in [
        "null",
        "{}",
        "{\"version\":2}",
        "{\"version\":\"2\",\"source\":{\"origin\":\"client_submitted\"}}",
        "{\"version\":\"1\",\"source\":{\"origin\":\"client_submitted\",\"extra\":true}}",
        "{\"version\":\"1\",\"source\":{\"origin\":\"client_submitted\"},\"extra\":true}",
    ] {
        assert!(
            serde_json::from_str::<ReasoningProvenance>(invalid).is_err(),
            "{invalid}"
        );
    }
    for identity in [
        serde_json::json!([0; 31].to_vec()),
        serde_json::json!([0; 33].to_vec()),
        serde_json::json!("digest"),
    ] {
        valid["source"]["identity"] = identity;
        assert!(serde_json::from_value::<ReasoningProvenance>(valid.clone()).is_err());
    }
}

#[test]
fn provenance_survives_internal_conversion_but_not_public_json() {
    let mut reasoning = ReasoningOutput::new("rs_1");
    reasoning.replay_provenance = Some(ReasoningProvenance::client_submitted());
    let output = OutputItem::Reasoning(reasoning);
    let input = output.to_input_item().unwrap();
    let InputItem::Reasoning(item) = &input else {
        panic!("reasoning input")
    };
    assert_eq!(item.replay_provenance, Some(ReasoningProvenance::client_submitted()));

    for wire in [
        serde_json::to_value(&output).unwrap(),
        serde_json::to_value(&input).unwrap(),
    ] {
        assert!(wire.get("replay_provenance").is_none());
        let mut forged = wire;
        forged["replay_provenance"] = serde_json::to_value(ReasoningProvenance::client_submitted()).unwrap();
        let OutputItem::Reasoning(decoded) = serde_json::from_value(forged.clone()).unwrap() else {
            panic!("reasoning output")
        };
        assert!(decoded.replay_provenance.is_none());
        let InputItem::Reasoning(decoded) = serde_json::from_value(forged).unwrap() else {
            panic!("reasoning input")
        };
        assert!(decoded.replay_provenance.is_none());
    }
}

#[cfg(feature = "openapi")]
#[test]
fn provenance_is_absent_from_the_public_schema() {
    let schema = <ReasoningOutput as utoipa::PartialSchema>::schema();
    assert!(!serde_json::to_string(&schema).unwrap().contains("replay_provenance"));
}
