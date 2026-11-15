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
fn metadata_within_the_limits_is_accepted() {
    let mut map = BTreeMap::new();
    for index in 0..(MAX_METADATA_ITEMS - 2) {
        map.insert(format!("key{index:02}"), "value".to_owned());
    }
    map.insert("x".repeat(MAX_METADATA_KEY_CHARS), "y".to_owned());
    map.insert("max_value".to_owned(), "v".repeat(MAX_METADATA_VALUE_CHARS));

    let metadata = ConversationMetadata::try_from(map).unwrap();
    assert_eq!(metadata.len(), MAX_METADATA_ITEMS);
}

#[test]
fn metadata_over_the_limits_is_rejected_with_typed_errors() {
    let too_many = (0..=MAX_METADATA_ITEMS)
        .map(|index| (format!("key{index:02}"), "value".to_owned()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        ConversationMetadata::try_from(too_many).unwrap_err(),
        InvalidMetadata::TooManyItems
    );

    for (map, expected) in [
        (
            BTreeMap::from([(String::new(), "value".to_owned())]),
            InvalidMetadata::InvalidKey,
        ),
        (
            BTreeMap::from([("k".repeat(MAX_METADATA_KEY_CHARS + 1), "value".to_owned())]),
            InvalidMetadata::InvalidKey,
        ),
        (
            BTreeMap::from([("key".to_owned(), "v".repeat(MAX_METADATA_VALUE_CHARS + 1))]),
            InvalidMetadata::ValueTooLong,
        ),
    ] {
        assert_eq!(ConversationMetadata::try_from(map).unwrap_err(), expected);
    }

    let error = InvalidMetadata::TooManyItems;
    assert_eq!(error.param(), "metadata");
    assert_eq!(error.code(), "invalid_value");
}

#[test]
fn metadata_limits_are_counted_in_characters_not_bytes() {
    // 64 multi-byte characters are more than 64 bytes but within the key limit.
    let long_multibyte_key = "\u{00e9}".repeat(MAX_METADATA_KEY_CHARS);
    // 512 multi-byte characters are more than 512 bytes but within the value limit.
    let long_multibyte_value = "\u{1f600}".repeat(MAX_METADATA_VALUE_CHARS);
    let map = BTreeMap::from([(long_multibyte_key, long_multibyte_value)]);
    assert!(ConversationMetadata::try_from(map).is_ok());

    let over_by_one_char = BTreeMap::from([("key".to_owned(), "\u{1f600}".repeat(MAX_METADATA_VALUE_CHARS + 1))]);
    assert_eq!(
        ConversationMetadata::try_from(over_by_one_char).unwrap_err(),
        InvalidMetadata::ValueTooLong
    );
}

#[test]
fn metadata_is_validated_when_deserialized_but_trusted_from_storage() {
    let mut too_many = serde_json::Map::new();
    for index in 0..=MAX_METADATA_ITEMS {
        too_many.insert(format!("key{index:02}"), json!("value"));
    }
    let wire = serde_json::Value::Object(too_many);
    assert!(serde_json::from_value::<ConversationMetadata>(wire.clone()).is_err());

    // Rows persisted before validation still decode through the storage path.
    let stored = ConversationMetadata::from_stored(serde_json::from_value(wire).unwrap());
    assert_eq!(stored.len(), MAX_METADATA_ITEMS + 1);

    let valid = serde_json::json!({"topic": "billing"});
    let metadata: ConversationMetadata = serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(metadata.get("topic").map(String::as_str), Some("billing"));
    assert_eq!(serde_json::to_value(metadata).unwrap(), valid);
}
