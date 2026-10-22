use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeMap};
use serde_json::Value;

use crate::types::tools::{NonEmptyToolName, ResponsesTool};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
    Shell,
    Function {
        namespace: Option<String>,
        name: NonEmptyToolName,
    },
    Custom {
        name: NonEmptyToolName,
    },
    AllowedTools {
        mode: AllowedToolsMode,
        tools: Vec<AllowedTool>,
    },
    /// Forces a tool from one MCP server. Parsed so admission can reject it
    /// with a typed `tool_choice` error; the gateway does not execute it.
    Mcp {
        server_label: String,
        name: Option<NonEmptyToolName>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AllowedTool {
    #[serde(rename = "type")]
    pub type_: NonEmptyToolName,
    pub name: NonEmptyToolName,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum AllowedToolsMode {
    Auto,
    Required,
}

#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for ToolChoice {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::{
            ObjectBuilder, Ref,
            schema::{ArrayBuilder, OneOfBuilder, SchemaType, Type},
        };

        let str_type = || ObjectBuilder::new().schema_type(SchemaType::new(Type::String));

        let name_ref = || Ref::from_schema_name("NonEmptyToolName");

        OneOfBuilder::new()
            .item(str_type().enum_values(Some(["auto", "none", "required"])))
            .item(
                ObjectBuilder::new()
                    .property("type", str_type().enum_values(Some(["function"])))
                    .required("type")
                    .property("name", name_ref())
                    .required("name")
                    .property("namespace", str_type()),
            )
            .item(
                ObjectBuilder::new()
                    .property("type", str_type().enum_values(Some(["custom"])))
                    .required("type")
                    .property("name", name_ref())
                    .required("name"),
            )
            .item(
                ObjectBuilder::new()
                    .property("type", str_type().enum_values(Some(["allowed_tools"])))
                    .required("type")
                    .property("mode", Ref::from_schema_name("AllowedToolsMode"))
                    .required("mode")
                    .property("tools", ArrayBuilder::new().items(Ref::from_schema_name("AllowedTool")))
                    .required("tools"),
            )
            .item(
                ObjectBuilder::new()
                    .description(Some(
                        "Accepted for parsing but not supported: rejected with 400 unsupported_value.",
                    ))
                    .property("type", str_type().enum_values(Some(["mcp"])))
                    .required("type")
                    .property("server_label", str_type())
                    .required("server_label")
                    .property("name", name_ref()),
            )
            .item(
                ObjectBuilder::new()
                    .property(
                        "function",
                        ObjectBuilder::new()
                            .property("name", name_ref())
                            .required("name")
                            .property("namespace", str_type()),
                    )
                    .required("function"),
            )
            .into()
    }
}
#[cfg(feature = "openapi")]
impl utoipa::ToSchema for ToolChoice {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ToolChoice")
    }
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
            Self::Shell => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("type", "shell")?;
                map.end()
            }
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
            Self::AllowedTools { mode, tools } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "allowed_tools")?;
                map.serialize_entry("mode", mode)?;
                map.serialize_entry("tools", tools)?;
                map.end()
            }
            Self::Mcp { server_label, name } => {
                let mut map = serializer.serialize_map(Some(2 + usize::from(name.is_some())))?;
                map.serialize_entry("type", "mcp")?;
                map.serialize_entry("server_label", server_label)?;
                if let Some(name) = name {
                    map.serialize_entry("name", name.as_str())?;
                }
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
                if object.get("type").and_then(Value::as_str) == Some("shell") {
                    return Ok(Self::Shell);
                }
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

                if object.get("type").and_then(Value::as_str) == Some("allowed_tools") {
                    let mode = object
                        .get("mode")
                        .cloned()
                        .ok_or_else(|| de::Error::missing_field("mode"))?;
                    let mode = serde_json::from_value(mode).map_err(de::Error::custom)?;
                    let tools = object
                        .get("tools")
                        .cloned()
                        .ok_or_else(|| de::Error::missing_field("tools"))?;
                    let tools = serde_json::from_value(tools).map_err(de::Error::custom)?;
                    return Ok(Self::AllowedTools { mode, tools });
                }

                if object.get("type").and_then(Value::as_str) == Some("mcp") {
                    let server_label = object
                        .get("server_label")
                        .and_then(Value::as_str)
                        .ok_or_else(|| de::Error::missing_field("server_label"))?
                        .to_owned();
                    let name = match object.get("name") {
                        None | Some(Value::Null) => None,
                        Some(Value::String(name)) => {
                            Some(NonEmptyToolName::try_from(name.as_str()).map_err(de::Error::custom)?)
                        }
                        Some(_) => return Err(de::Error::custom("MCP tool_choice name must be a string")),
                    };
                    return Ok(Self::Mcp { server_label, name });
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
                    "expected tool_choice string, named tool object, or allowed_tools object",
                ))
            }
            _ => Err(de::Error::custom(
                "expected tool_choice string, named tool object, or allowed_tools object",
            )),
        }
    }
}

impl ToolChoice {
    /// Whether the gateway can honor this selector; an MCP selector is parsed only to be rejected.
    #[must_use]
    pub const fn is_supported(&self) -> bool {
        match self {
            Self::Mcp { .. } => false,
            Self::Auto
            | Self::None
            | Self::Required
            | Self::Shell
            | Self::Function { .. }
            | Self::Custom { .. }
            | Self::AllowedTools { .. } => true,
        }
    }

    /// Converts client-facing custom-tool selectors to the function-tool shape
    /// used by the normalized upstream tool declarations.
    #[must_use]
    pub(crate) fn normalized_for_upstream(&self) -> Self {
        match self {
            Self::Shell => Self::Function {
                namespace: None,
                name: NonEmptyToolName::try_from(crate::tool::shell::SHELL_FUNCTION_NAME)
                    .expect("shell is a non-empty tool name"),
            },
            Self::Custom { name } => Self::Function {
                namespace: None,
                name: name.clone(),
            },
            Self::AllowedTools { mode, tools } => Self::AllowedTools {
                mode: *mode,
                tools: tools.iter().cloned().map(normalize_allowed_tool).collect(),
            },
            // An MCP selector never reaches inference: admission rejects it.
            choice @ (Self::Auto | Self::None | Self::Required | Self::Function { .. } | Self::Mcp { .. }) => {
                choice.clone()
            }
        }
    }
}

fn normalize_allowed_tool(mut tool: AllowedTool) -> AllowedTool {
    if tool.type_.as_str() == "custom" {
        tool.type_ = NonEmptyToolName::try_from("function").expect("function is a non-empty tool type");
    }
    tool
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
/// An explicit tool list selects the request's own choice or its default, so
/// inherited selectors cannot refer to tools that the request has removed.
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
    fn mcp_tool_choice_round_trips_and_is_unsupported() {
        for wire in [
            serde_json::json!({"type": "mcp", "server_label": "docs"}),
            serde_json::json!({"type": "mcp", "server_label": "docs", "name": "search"}),
        ] {
            let choice: ToolChoice = serde_json::from_value(wire.clone()).expect("MCP tool_choice parses");
            assert_eq!(serde_json::to_value(&choice).expect("serialize"), wire);
            assert!(!choice.is_supported());
        }
        assert!(serde_json::from_value::<ToolChoice>(serde_json::json!({"type": "mcp"})).is_err());
        assert!(
            serde_json::from_value::<ToolChoice>(serde_json::json!({"type": "mcp", "server_label": "docs", "name": 1}))
                .is_err()
        );
        assert!(ToolChoice::Required.is_supported());
    }

    #[test]
    fn custom_tool_choice_round_trips() {
        let custom = serde_json::json!({
            "type": "custom",
            "name": "apply_patch"
        });

        let choice: ToolChoice = serde_json::from_value(custom.clone()).unwrap();
        assert_eq!(
            choice,
            ToolChoice::Custom {
                name: NonEmptyToolName::try_from("apply_patch").unwrap()
            }
        );
        assert_eq!(serde_json::to_value(choice).unwrap(), custom);
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

    #[test]
    fn allowed_tools_round_trip() {
        let expected = serde_json::json!({
            "type": "allowed_tools",
            "mode": "required",
            "tools": [
                {"type": "function", "name": "get_weather"},
                {"type": "custom", "name": "code_exec"}
            ]
        });

        let choice: ToolChoice = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(choice).unwrap(), expected);
    }

    #[test]
    fn allowed_tools_require_non_empty_type_and_name() {
        for invalid_tool in [
            serde_json::json!({"type": "function"}),
            serde_json::json!({"type": "", "name": "get_weather"}),
            serde_json::json!({"type": "function", "name": ""}),
        ] {
            assert!(
                serde_json::from_value::<ToolChoice>(serde_json::json!({
                    "type": "allowed_tools",
                    "mode": "auto",
                    "tools": [invalid_tool]
                }))
                .is_err()
            );
        }
    }
}
