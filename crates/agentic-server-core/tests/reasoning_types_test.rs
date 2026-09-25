//! Schema and safety tests; provider wire behavior is covered by recorded cassettes.

use agentic_core::types::io::reasoning::MAX_OPAQUE_REASONING_BYTES;
use agentic_core::{InputItem, OpaqueReasoning, OpaqueReasoningError, OutputItem, ReasoningOutput, ReasoningStatus};
use serde_json::json;

#[test]
fn opaque_reasoning_round_trips_without_interpreting_or_normalizing() {
    for original in ["", " opaque+/=\n\t", "{\"ciphertext\":\"not parsed\"}", "λ🔐"] {
        let wire = serde_json::to_string(original).unwrap();
        let state: OpaqueReasoning = serde_json::from_str(&wire).unwrap();
        assert_eq!(state.as_str(), original);
        assert_eq!(serde_json::to_string(&state).unwrap(), wire);
    }
}

#[test]
fn opaque_reasoning_rejects_non_string_values() {
    for value in [
        json!({"ciphertext": "sensitive-state"}),
        json!(["sensitive-state"]),
        json!(true),
        json!(7),
        json!(null),
    ] {
        let error = serde_json::from_value::<OpaqueReasoning>(value).unwrap_err();
        assert!(!error.to_string().contains("sensitive-state"));
    }
}

#[test]
fn opaque_reasoning_debug_is_redacted_even_inside_an_item() {
    let mut item = ReasoningOutput::new("rs_1");
    item.encrypted_content = Some(OpaqueReasoning::try_from("sensitive-state".to_owned()).unwrap());
    let debug = format!("{item:?}");
    assert!(!debug.contains("sensitive-state"));
    assert!(debug.contains("bytes: 15"));
}

#[test]
fn opaque_reasoning_ceiling_counts_decoded_utf8_bytes() {
    let at_limit = "λ".repeat(MAX_OPAQUE_REASONING_BYTES / 2);
    let wire = serde_json::to_string(&at_limit).unwrap();
    let state = OpaqueReasoning::try_from(at_limit).unwrap();
    assert_eq!(state.as_str().len(), MAX_OPAQUE_REASONING_BYTES);
    assert_eq!(serde_json::from_str::<OpaqueReasoning>(&wire).unwrap(), state);

    let oversized = format!("{}x", state.as_str());
    let wire = serde_json::to_string(&oversized).unwrap();
    assert_eq!(
        OpaqueReasoning::try_from(oversized),
        Err(OpaqueReasoningError::TooLarge)
    );
    let error = serde_json::from_str::<OpaqueReasoning>(&wire).unwrap_err();
    assert!(error.to_string().contains("byte limit"));
    assert!(!error.to_string().contains('λ'));
}

#[test]
fn complete_reasoning_survives_the_single_output_to_input_conversion() {
    let wire = json!({
        "type": "reasoning", "id": "rs_1", "status": "completed",
        "content": [{"type": "reasoning_text", "text": "first"}, {"type": "reasoning_text", "text": "second"}],
        "summary": [{"type": "summary_text", "text": "public summary"}],
        "encrypted_content": " opaque+/=\n"
    });
    let output: OutputItem = serde_json::from_value(wire.clone()).unwrap();
    let input = output.to_input_item().unwrap();
    assert_eq!(serde_json::to_value(&output).unwrap(), wire);
    assert_eq!(serde_json::to_value(&input).unwrap(), wire);
    let InputItem::Reasoning(reasoning) = input else {
        panic!("reasoning input expected")
    };
    assert_eq!(reasoning.status, Some(ReasoningStatus::Completed));
    assert_eq!(reasoning.summary[0].text, "public summary");
}

#[test]
fn nullable_reasoning_fields_keep_the_existing_wire_contract() {
    for wire in [
        json!({"id": "rs_1"}),
        json!({
            "id": "rs_1", "content": null, "summary": null, "encrypted_content": null, "status": null
        }),
    ] {
        let reasoning: ReasoningOutput = serde_json::from_value(wire).unwrap();
        assert!(reasoning.content.is_empty());
        assert!(reasoning.summary.is_empty());
        assert!(reasoning.encrypted_content.is_none());
        assert!(reasoning.status.is_none());
    }
}

#[test]
fn known_reasoning_items_reject_untyped_or_misspelled_fields() {
    for patch in [
        json!({"encrypted_content": {"ciphertext": "opaque"}}),
        json!({"summary": ["summary"]}),
        json!({"summary": [{"type": "summary_text", "text": 7}]}),
        json!({"summary": [{"type": "reasoning_text", "text": "wrong kind"}]}),
        json!({"summary": [{"type": "summary_text", "text": "summary", "unmodeled": "field"}]}),
        json!({"content": [{"type": "summary_text", "text": "wrong kind"}]}),
        json!({"status": "complete"}),
    ] {
        let mut item = json!({"id": "rs_1", "type": "reasoning"});
        item.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
        assert!(serde_json::from_value::<InputItem>(item.clone()).is_err(), "{item}");
        assert!(serde_json::from_value::<OutputItem>(item.clone()).is_err(), "{item}");
    }
}
