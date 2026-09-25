use super::*;
use crate::types::{ReasoningOutput, ReasoningSummaryContent, ReasoningTextContent, RequestPayload};
use serde_json::{Value, json};

fn request() -> RequestPayload {
    serde_json::from_value(json!({"model":"test", "input":"hello", "store":true})).unwrap()
}

#[test]
fn opaque_view_borrows_exact_state_without_mutating_canonical_history() {
    let mut reasoning = ReasoningOutput::new("rs_1");
    reasoning
        .content
        .push(ReasoningTextContent::new("plaintext must not be projected"));
    reasoning.summary.push(ReasoningSummaryContent::new("public summary"));
    reasoning.encrypted_content = Some(OpaqueReasoning::try_from(" \nopaque-λ\\==\t".to_owned()).unwrap());
    reasoning.status = Some(ReasoningStatus::Completed);
    reasoning.replay_provenance = Some(crate::types::reasoning_replay::ReasoningProvenance::client_submitted());
    let mut request = request();
    request.input = ResponsesInput::Items(vec![InputItem::Reasoning(reasoning)]);
    let canonical = serde_json::to_value(&request).unwrap();
    let view = request.to_upstream_request(true).unwrap().with_opaque_replay();
    assert!(matches!(view.input.input, Cow::Borrowed(_)));
    let wire = serde_json::to_value(view).unwrap();
    assert_eq!(wire["store"], false);
    assert_eq!(wire["stream"], true);
    assert_eq!(
        wire["input"][0],
        json!({
            "type":"reasoning", "id":"rs_1", "status":"completed",
            "summary":[{"type":"summary_text", "text":"public summary"}],
            "encrypted_content":" \nopaque-λ\\==\t"
        })
    );
    assert_eq!(serde_json::to_value(&request).unwrap(), canonical);
    let ResponsesInput::Items(items) = &request.input else {
        panic!("items")
    };
    let InputItem::Reasoning(item) = &items[0] else {
        panic!("reasoning")
    };
    assert!(item.replay_provenance.is_some());
}

#[test]
fn optional_status_is_omitted_only_in_opaque_projection() {
    let mut request = request();
    request.input = ResponsesInput::Items(vec![InputItem::Reasoning(ReasoningOutput::new("rs_1"))]);
    let default: Value = serde_json::to_value(request.to_upstream_request(false).unwrap()).unwrap();
    assert!(default.get("store").is_none());
    assert_eq!(default["input"][0]["content"], json!([]));
    assert!(default["input"][0].get("status").unwrap().is_null());
    let opaque = serde_json::to_value(request.to_upstream_request(false).unwrap().with_opaque_replay()).unwrap();
    assert_eq!(
        opaque["input"][0],
        json!({"type":"reasoning", "id":"rs_1", "summary":[]})
    );
}

#[test]
fn initial_text_and_gateway_storage_policy_are_independent_of_upstream_store() {
    for store in [false, true] {
        let mut request = request();
        request.store = store;
        let wire = serde_json::to_value(request.to_upstream_request(false).unwrap().with_opaque_replay()).unwrap();
        assert_eq!(wire["input"], "hello");
        assert_eq!(wire["store"], false);
        assert_eq!(request.store, store);
        assert!(wire.get("previous_response_id").is_none());
        assert!(wire.get("conversation").is_none());
    }
}
