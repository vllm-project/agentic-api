use super::*;
use serde_json::json;

#[test]
fn thinking_preserves_known_and_future_provider_configurations() {
    for value in [
        json!({"type":"enabled","budget_tokens":1024,"extension":null}),
        json!({"type":"disabled","extension":true}),
        json!({"type":"adaptive"}),
        json!({"type":"between_tools"}),
        json!({"type":"provider_future","options":{"mode":"雪","values":[null,1]}}),
    ] {
        let configuration: ThinkingConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&configuration).unwrap(), value);
        assert_eq!(
            matches!(configuration, ThinkingConfig::Other(_)),
            value["type"] == "provider_future"
        );
    }
}

#[test]
fn unknown_thinking_fallback_does_not_hide_malformed_known_payloads() {
    for value in [
        json!(null),
        json!({}),
        json!({"type":42}),
        json!({"type":"enabled"}),
        json!({"type":"enabled","budget_tokens":"invalid"}),
        json!({"type":"enabled","budget_tokens":-1}),
        json!({"type":"enabled","budget_tokens":4_294_967_296_u64}),
    ] {
        assert!(
            serde_json::from_value::<ThinkingConfig>(value.clone()).is_err(),
            "{value}"
        );
    }
}

#[test]
fn cache_control_preserves_unknown_types_and_their_extension_fields() {
    for kind in ["ephemeral", "provider_future"] {
        let value = json!({"type":kind,"ttl":"1h","provider_extension":{"value":null}});
        let control: CacheControl = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&control).unwrap(), value);
        assert_eq!(
            matches!(control.kind, CacheControlKind::Other(_)),
            kind == "provider_future"
        );
    }
    for value in [json!({}), json!({"type":null}), json!({"type":4})] {
        assert!(serde_json::from_value::<CacheControl>(value).is_err());
    }
}
