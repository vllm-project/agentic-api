use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::types::io::ToolChoice;

pub(super) const fn default_true() -> bool {
    true
}

// serde requires a `&Option<T>` receiver, so the idiomatic `Option<&T>` does not apply here.
#[allow(clippy::ref_option)]
pub(super) fn is_absent_or_default_tool_choice(choice: &Option<ToolChoice>) -> bool {
    choice.as_ref().is_none_or(|choice| matches!(choice, ToolChoice::Auto))
}

// serde's `serialize_with` passes a reference to the field's concrete type.
#[allow(clippy::ref_option)]
pub(super) fn serialize_upstream_tool_choice<S>(choice: &Option<ToolChoice>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    choice
        .as_ref()
        .map(ToolChoice::normalized_for_upstream)
        .serialize(serializer)
}

/// Deserializes conversation from both string and object forms.
/// Accepts:
/// - String: `"conv_123"`
/// - Object: `{ "id": "conv_123" }`
/// - Null/absent: `None`
#[allow(clippy::ref_option)]
pub(super) fn deserialize_conversation<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Object(obj)) => obj
            .get("id")
            .and_then(Value::as_str)
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| serde::de::Error::custom("conversation object must have 'id' field")),
        Some(_) => Err(serde::de::Error::custom(
            "conversation must be a string or object with 'id' field",
        )),
    }
}

/// Serializes conversation as object form for responses.
/// Outputs: `{ "id": "conv_123" }` or `null`
#[allow(clippy::ref_option)]
pub(super) fn serialize_conversation_object<S>(
    conversation_id: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match conversation_id {
        None => serializer.serialize_none(),
        Some(id) => {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry("id", id)?;
            map.end()
        }
    }
}
