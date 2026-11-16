use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Streaming controls for a Responses request, forwarded to the upstream verbatim.
///
/// The gateway does not gate SSE usage on this object: terminal `response.completed`
/// frames emit usage unconditionally. Known members are typed for validation; every
/// other member is preserved unchanged for upstream compatibility.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct StreamOptions {
    /// The member defined by the Responses SDK.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_obfuscation: Option<bool>,
    /// Unmodeled streaming members (for example, `include`) preserved for upstream compatibility.
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_known_and_unknown_members() {
        for wire in [
            serde_json::json!({}),
            serde_json::json!({"include_obfuscation": true}),
            serde_json::json!({"include_obfuscation": false}),
            serde_json::json!({"include": ["usage"]}),
            serde_json::json!({"include": ["usage"], "include_usage": true}),
        ] {
            let options: StreamOptions = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(&options).unwrap(), wire, "{wire}");
        }
    }

    #[test]
    fn non_boolean_include_obfuscation_is_rejected() {
        for value in [
            serde_json::json!("true"),
            serde_json::json!(1),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            assert!(
                serde_json::from_value::<StreamOptions>(serde_json::json!({"include_obfuscation": value})).is_err(),
                "non-boolean include_obfuscation must be rejected: {value}"
            );
        }
    }

    #[test]
    fn non_object_stream_options_is_rejected() {
        for value in [
            serde_json::json!("usage"),
            serde_json::json!(1),
            serde_json::json!(true),
            serde_json::json!(["usage"]),
        ] {
            assert!(
                serde_json::from_value::<StreamOptions>(value.clone()).is_err(),
                "stream_options must be an object: {value}"
            );
        }
    }
}
