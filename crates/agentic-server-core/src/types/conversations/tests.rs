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
fn metadata_limits_are_enforced_while_parsing() {
    let pairs = |count: usize| {
        (0..count)
            .map(|i| (format!("k{i}"), "v".to_owned()))
            .collect::<BTreeMap<_, _>>()
    };
    let one = |key: String, value: String| BTreeMap::from([(key, value)]);
    assert_eq!(ConversationMetadata::try_from(pairs(16)).map(|m| m.len()), Ok(16));
    assert_eq!(
        ConversationMetadata::try_from(pairs(17)),
        Err(InvalidMetadata::TooManyPairs(17))
    );
    assert!(ConversationMetadata::try_from(one("k".repeat(64), "v".repeat(512))).is_ok());
    // The limits count characters, as OpenAI documents them, not bytes.
    assert!(ConversationMetadata::try_from(one("\u{4e2d}".repeat(64), "\u{4e2d}".repeat(512))).is_ok());
    assert_eq!(
        ConversationMetadata::try_from(one(String::new(), "v".into())),
        Err(InvalidMetadata::KeyLength(0))
    );
    assert_eq!(
        ConversationMetadata::try_from(one("k".repeat(65), "v".into())),
        Err(InvalidMetadata::KeyLength(65))
    );
    assert_eq!(
        ConversationMetadata::try_from(one("k".into(), "\u{4e2d}".repeat(513))),
        Err(InvalidMetadata::ValueTooLong {
            key: "k".into(),
            chars: 513
        })
    );
    let error = InvalidMetadata::TooManyPairs(17);
    assert_eq!((error.param(), error.code()), ("metadata", "invalid_value"));

    // Every request that carries the type rejects invalid metadata while it is decoded.
    let too_many = serde_json::Value::Object((0..17).map(|i| (format!("k{i}"), json!("v"))).collect());
    let error = serde_json::from_value::<UpdateConversationRequest>(json!({"metadata": too_many.clone()})).unwrap_err();
    assert!(error.to_string().contains("at most 16 key-value pairs"), "{error}");
    assert!(serde_json::from_value::<CreateConversationRequest>(json!({"metadata": too_many.clone()})).is_err());
    assert!(
        serde_json::from_value::<crate::types::request_response::RequestPayload>(
            json!({"model": "m", "input": "hi", "metadata": too_many.clone()})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<crate::types::file_search::CreateVectorStoreRequest>(json!({"metadata": too_many}))
            .is_err()
    );
    let request: UpdateConversationRequest = serde_json::from_value(json!({"metadata": {"k": "v"}})).unwrap();
    assert_eq!(serde_json::to_value(&request).unwrap(), json!({"metadata": {"k": "v"}}));
}

#[test]
fn returned_conversations_decode_stored_metadata_unchecked() {
    let too_many = serde_json::Value::Object((0..17).map(|i| (format!("k{i}"), json!("v"))).collect());
    let response: ConversationResponse = serde_json::from_value(
        json!({"id": "conv_1", "object": "conversation", "created_at": 1, "metadata": too_many}),
    )
    .unwrap();
    assert_eq!(response.metadata.len(), 17);
}
