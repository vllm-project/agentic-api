use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeMap};
use serde_json::Value;

use crate::types::tools::{NonEmptyToolName, ResponsesTool};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionTool {
    #[serde(rename = "type")]
    pub type_: String,
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<Value>,
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Function {
        namespace: Option<String>,
        name: NonEmptyToolName,
    },
    Custom {
        name: NonEmptyToolName,
    },
}

impl Serialize for ToolChoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::None => serializer.serialize_str("none"),
            Self::Required => serializer.serialize_str("required"),
            Self::Function { namespace, name } => {
                let mut map = serializer.serialize_map(Some(2 + usize::from(namespace.is_some())))?;
                map.serialize_entry("type", "function")?;
                if let Some(namespace) = namespace {
                    map.serialize_entry("namespace", namespace)?;
                }
                map.serialize_entry("name", name.as_str())?;
                map.end()
            }
            Self::Custom { name } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "custom")?;
                map.serialize_entry("name", name.as_str())?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ToolChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(choice) => match choice.as_str() {
                "auto" => Ok(Self::Auto),
                "none" => Ok(Self::None),
                "required" => Ok(Self::Required),
                other => Err(de::Error::unknown_variant(
                    other,
                    &["auto", "none", "required", "function", "custom"],
                )),
            },
            Value::Object(object) => {
                if object.get("type").and_then(Value::as_str) == Some("function") {
                    let namespace = object.get("namespace").and_then(Value::as_str).map(str::to_string);
                    let name = object
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| de::Error::missing_field("name"))?;
                    let name = NonEmptyToolName::try_from(name).map_err(de::Error::custom)?;
                    return Ok(Self::Function { namespace, name });
                }

                if object.get("type").and_then(Value::as_str) == Some("custom") {
                    let name = object
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| de::Error::missing_field("name"))?;
                    let name = NonEmptyToolName::try_from(name).map_err(de::Error::custom)?;
                    return Ok(Self::Custom { name });
                }

                if let Some(function) = object.get("function").and_then(Value::as_object) {
                    let namespace = function.get("namespace").and_then(Value::as_str).map(str::to_string);
                    let name = function
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| de::Error::missing_field("name"))?;
                    let name = NonEmptyToolName::try_from(name).map_err(de::Error::custom)?;
                    return Ok(Self::Function { namespace, name });
                }

                Err(de::Error::custom(
                    "expected tool_choice string, function object, or custom object",
                ))
            }
            _ => Err(de::Error::custom(
                "expected tool_choice string, function object, or custom object",
            )),
        }
    }
}

/// Returns the effective tool list, preferring `request_tools` when explicitly
/// set by the caller, otherwise falling back to the stored configuration.
#[inline]
pub(crate) fn resolve_tools(
    request_tools: Option<&[ResponsesTool]>,
    stored_tools: Option<&[ResponsesTool]>,
    tools_explicitly_set: bool,
) -> Option<Vec<ResponsesTool>> {
    if tools_explicitly_set {
        request_tools
    } else {
        stored_tools
    }
    .map(<[_]>::to_vec)
}

/// Returns the effective tool choice using the same precedence as [`resolve_tools`].
#[inline]
pub(crate) fn resolve_tool_choice(
    request_choice: Option<&ToolChoice>,
    stored_choice: &ToolChoice,
    explicitly_set: bool,
) -> ToolChoice {
    if explicitly_set {
        request_choice.cloned().unwrap_or_default()
    } else {
        stored_choice.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_tool_choice_rejects_empty_name() {
        assert!(
            serde_json::from_value::<ToolChoice>(serde_json::json!({
                "type": "function",
                "name": ""
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ToolChoice>(serde_json::json!({
                "function": {
                    "name": ""
                }
            }))
            .is_err()
        );
    }

    #[test]
    fn custom_tool_choice_round_trips() {
        let expected = serde_json::json!({
            "type": "custom",
            "name": "apply_patch"
        });

        let choice: ToolChoice = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(
            choice,
            ToolChoice::Custom {
                name: NonEmptyToolName::try_from("apply_patch").unwrap()
            }
        );
        assert_eq!(serde_json::to_value(choice).unwrap(), expected);
    }

    #[test]
    fn custom_tool_choice_rejects_empty_name() {
        assert!(
            serde_json::from_value::<ToolChoice>(serde_json::json!({
                "type": "custom",
                "name": ""
            }))
            .is_err()
        );
    }
}
