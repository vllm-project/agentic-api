use super::*;
use serde_json::json;

#[test]
fn gateway_calls_keep_their_payload_and_unknown_types_are_rejected() {
    for wire in [
        json!({"type":"web_search_call", "id":"ws_1", "status":"completed", "action":{"type":"search", "query":"weather", "queries":["weather"], "sources":[{"url":"https://example.com"}]}}),
        json!({"type":"mcp_call", "id":"mcp_1", "name":"weather", "server_label":"weather", "arguments":"{}", "output":"sunny", "status":"completed"}),
    ] {
        let item: InOutItem = serde_json::from_value(wire.clone()).unwrap();
        let actual = serde_json::to_value(item).unwrap();
        for (key, value) in wire.as_object().unwrap() {
            assert_eq!(&actual[key], value, "{key}");
        }
    }
    assert!(serde_json::from_value::<InOutItem>(json!({"type":"future_item", "payload":42})).is_err());
    assert!(serde_json::from_value::<InOutItem>(json!({"type":"function_call_output"})).is_err());
}

#[test]
fn batch_requests_match_wire_contract() {
    let request: CreateItemRequest =
        serde_json::from_value(json!({"items":[{"role":"user", "content":"hello"}]})).unwrap();
    assert_eq!(request.items.len(), 1);
    assert!(serde_json::from_value::<CreateItemRequest>(json!({"item":{"role":"user", "content":"hello"}})).is_err());
}

#[test]
fn metadata_limits_match_openai() {
    let entries = |count: usize| (0..count).map(|i| (format!("k{i}"), "v".to_owned())).collect();
    assert_eq!(validate_metadata(&ConversationMetadata::new()), Ok(()));
    assert_eq!(validate_metadata(&entries(16)), Ok(()));
    assert_eq!(validate_metadata(&entries(17)), Err(MetadataError::TooManyEntries(17)));

    let one = |key: String, value: String| ConversationMetadata::from([(key, value)]);
    assert_eq!(validate_metadata(&one("k".repeat(64), "v".repeat(512))), Ok(()));
    assert_eq!(
        validate_metadata(&one(String::new(), "v".into())),
        Err(MetadataError::InvalidKeyLength(0))
    );
    assert_eq!(
        validate_metadata(&one("k".repeat(65), "v".into())),
        Err(MetadataError::InvalidKeyLength(65))
    );
    assert_eq!(
        validate_metadata(&one("k".into(), "v".repeat(513))),
        Err(MetadataError::ValueTooLong {
            key: "k".into(),
            len: 513
        })
    );
}
