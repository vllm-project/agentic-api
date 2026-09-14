//! Anthropic tool-selection policies, validated before gateway execution.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::types::tools::NonEmptyToolName;

/// The tagged union accepted by the Messages `tool_choice` field.
/// Unknown fields survive, but unknown policies require explicit support.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagesToolChoice {
    Auto(MessagesToolChoiceOptions),
    Any(MessagesToolChoiceOptions),
    Tool {
        name: NonEmptyToolName,
        #[serde(flatten)]
        options: MessagesToolChoiceOptions,
    },
    None {
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
}

/// Settings shared by policies that permit tool use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MessagesToolChoiceOptions {
    #[cfg_attr(feature = "openapi", schema(nullable = false))]
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present_bool")]
    pub disable_parallel_tool_use: Option<bool>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

// Omission is allowed; a present value must be a Boolean, not null.
fn present_bool<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    bool::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_variants_round_trip_with_extensions() {
        for choice in [
            json!({"type":"auto"}),
            json!({"type":"any", "name":"extension", "disable_parallel_tool_use":false, "future":{"x":null}}),
            json!({"type":"tool", "name":"WebSearch", "disable_parallel_tool_use":true, "future":[1,2]}),
            json!({"type":"none", "name":false, "future":null}),
        ] {
            let typed: MessagesToolChoice = serde_json::from_value(choice.clone()).unwrap();
            assert_eq!(serde_json::to_value(typed).unwrap(), choice);
        }
    }

    #[test]
    fn malformed_policies_are_rejected() {
        for choice in [
            json!([]),
            json!(true),
            json!("any"),
            json!(null),
            json!({}),
            json!({"type":false}),
            json!({"type":"future"}),
            json!({"type":"tool"}),
            json!({"type":"tool", "name":""}),
            json!({"type":"tool", "name":42}),
            json!({"type":"any", "disable_parallel_tool_use":"true"}),
            json!({"type":"auto", "disable_parallel_tool_use":null}),
        ] {
            assert!(
                serde_json::from_value::<MessagesToolChoice>(choice.clone()).is_err(),
                "{choice}"
            );
        }
    }
}
