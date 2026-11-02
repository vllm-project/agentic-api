//! `configuration_update` input items: a reasoning-effort change between responses.
//!
//! The item stays where the client put it, in stored and replayed history and in the upstream input. The request-level
//! `reasoning.effort` is never rewritten, so the request prefix before the update is unchanged.

use serde::{Deserialize, Serialize};

use super::{InputItem, ResponsesInput};
use crate::types::io::InOutItem;

/// An update to the response configuration. It applies from its position in the history to later responses until
/// another update replaces it. A field the gateway does not model, such as a `text` update, fails parsing instead of
/// being dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ConfigurationUpdateItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub reasoning: ConfigurationUpdateReasoning,
}

/// The reasoning settings an update changes. Effort is the only one that can change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ConfigurationUpdateReasoning {
    #[serde(deserialize_with = "super::deserialize_non_blank_string")]
    pub effort: String,
}

impl ResponsesInput {
    /// The effort set by the last `configuration_update` in this input, if any.
    #[must_use]
    pub fn reasoning_effort_update(&self) -> Option<&str> {
        let Self::Items(items) = self else {
            return None;
        };
        items.iter().rev().find_map(|item| match item {
            InOutItem::Input(InputItem::ConfigurationUpdate(update)) => Some(update.reasoning.effort.as_str()),
            InOutItem::Input(_) | InOutItem::Output(_) => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use serde_json::{Value, json};

    use super::*;
    use crate::types::request_response::RequestPayload;

    fn history(update: &Value) -> Value {
        json!([
            {"type": "message", "role": "user", "content": "Plan the migration."},
            update,
            {"type": "message", "role": "user", "content": "Now check the edge cases."}
        ])
    }

    #[test]
    fn configuration_update_keeps_its_payload_and_position() {
        for update in [
            json!({"type": "configuration_update", "reasoning": {"effort": "high"}}),
            json!({"type": "configuration_update", "id": "item_update", "reasoning": {"effort": "low"}}),
        ] {
            let wire = history(&update);
            let input: ResponsesInput = serde_json::from_value(wire.clone()).expect("update parses");
            let ResponsesInput::Items(items) = &input else {
                panic!("expected an item list");
            };
            assert!(matches!(
                &items[1],
                InOutItem::Input(InputItem::ConfigurationUpdate(item))
                    if item.reasoning.effort == update["reasoning"]["effort"]
            ));
            assert_eq!(
                serde_json::to_value(&input).unwrap(),
                wire,
                "the item serializes as sent"
            );

            let model = input.model_input();
            assert!(matches!(model, Cow::Borrowed(_)), "the model sees the input unchanged");
            assert_eq!(serde_json::to_value(&model).unwrap(), wire);
        }
    }

    #[test]
    fn malformed_configuration_updates_are_rejected() {
        for reasoning in [
            None,
            Some(json!({})),
            Some(json!({"effort": null})),
            Some(json!({"effort": 3})),
            Some(json!({"effort": " "})),
            Some(json!({"effort": "high", "summary": "auto"})),
            Some(json!("high")),
        ] {
            let mut wire = json!({"type": "configuration_update"});
            if let Some(reasoning) = reasoning {
                wire["reasoning"] = reasoning;
            }
            assert!(serde_json::from_value::<InputItem>(wire.clone()).is_err(), "{wire}");
            assert!(
                serde_json::from_value::<ResponsesInput>(history(&wire)).is_err(),
                "{wire}"
            );
        }
        let text_update = json!({"type": "configuration_update", "reasoning": {"effort": "high"},
            "text": {"format": {"type": "text"}}});
        assert!(
            serde_json::from_value::<InputItem>(text_update).is_err(),
            "a text update fails instead of being dropped"
        );
    }

    #[test]
    fn the_last_update_sets_the_effective_effort() {
        let request = |reasoning: Option<Value>, input: Value| -> RequestPayload {
            let mut body = json!({"model": "test", "input": input});
            if let Some(reasoning) = reasoning {
                body["reasoning"] = reasoning;
            }
            serde_json::from_value(body).unwrap()
        };
        let low = Some(json!({"effort": "low"}));
        let update = |effort: &str| json!({"type": "configuration_update", "reasoning": {"effort": effort}});

        assert_eq!(request(None, json!("hi")).effective_reasoning_effort(), None);
        assert_eq!(
            request(low.clone(), json!("hi")).effective_reasoning_effort(),
            Some("low")
        );
        assert_eq!(
            request(None, history(&update("high"))).effective_reasoning_effort(),
            Some("high")
        );

        let wire = json!([
            {"type": "message", "role": "user", "content": "one"},
            update("high"),
            {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
             "content": [{"type": "output_text", "text": "done"}]},
            update("medium"),
            {"type": "message", "role": "user", "content": "two"}
        ]);
        let payload = request(low, wire.clone());
        assert_eq!(payload.input.reasoning_effort_update(), Some("medium"));
        assert_eq!(payload.effective_reasoning_effort(), Some("medium"));
        assert_eq!(
            payload.reasoning.as_deref().and_then(|r| r.effort.as_deref()),
            Some("low")
        );

        let upstream = serde_json::to_value(payload.to_upstream_request(false).unwrap()).unwrap();
        assert_eq!(
            upstream["reasoning"],
            json!({"effort": "low"}),
            "request-level effort is not rewritten"
        );
        assert_eq!(upstream["input"], wire, "both updates reach the upstream in place");
    }
}
