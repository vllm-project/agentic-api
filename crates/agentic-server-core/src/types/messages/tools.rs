//! Messages tool declarations, classified without interpreting provider extensions.
//!
//! Known tool kinds have distinct variants. Unknown versioned tools retain their
//! discriminator and opaque fields; recognition does not grant execution ownership.

use std::collections::HashMap;

use super::mcp::MessagesMcpToolset;
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::fmt;

mod versions;

pub use versions::{
    NATIVE_WEB_FETCH_TYPE, NATIVE_WEB_FETCH_VERSIONS, NATIVE_WEB_SEARCH_TYPE, NATIVE_WEB_SEARCH_VERSIONS,
    NativeToolVersion,
};

/// A Messages wire declaration. Add a variant and its discriminator mapping when
/// supporting a new kind; routing and execution remain the tool layer's decision.
#[derive(Debug, Clone)]
pub enum ToolParam {
    Function(MessagesToolFields),
    WebSearch(MessagesNativeTool),
    WebFetch(MessagesNativeTool),
    Provider(MessagesProviderTool),
    McpToolset(MessagesMcpToolset),
}

/// A recognized native tool declaration with its original protocol version.
/// The private version is selected from the shared support table during
/// deserialization; classification alone does not authorize execution.
#[derive(Debug, Clone)]
pub struct MessagesNativeTool {
    version: NativeToolVersion,
    pub fields: MessagesToolFields,
}

impl MessagesNativeTool {
    #[must_use]
    pub fn version(&self) -> NativeToolVersion {
        self.version
    }
}

/// Fields shared by the currently supported declarations. JSON Schema and
/// provider extensions remain opaque, preserving the existing wire contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MessagesToolFields {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// An unrecognized provider declaration, retained for pass-through. Future
/// versions must not silently acquire the semantics of a supported version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MessagesProviderTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    #[serde(flatten)]
    pub fields: MessagesToolFields,
}

impl ToolParam {
    #[must_use]
    pub fn fields(&self) -> Option<&MessagesToolFields> {
        match self {
            Self::Function(fields) => Some(fields),
            Self::WebSearch(tool) | Self::WebFetch(tool) => Some(&tool.fields),
            Self::Provider(tool) => Some(&tool.fields),
            Self::McpToolset(_) => None,
        }
    }

    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.fields().map(|fields| fields.name.as_str())
    }

    #[must_use]
    pub fn tool_type(&self) -> Option<&str> {
        match self {
            Self::Function(_) => None,
            Self::WebSearch(tool) | Self::WebFetch(tool) => Some(tool.version.type_name),
            Self::Provider(tool) => Some(&tool.tool_type),
            Self::McpToolset(_) => Some("mcp_toolset"),
        }
    }
}

/// The exact upstream-hosted search declarations supported by this connector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostedToolSearchVersion {
    Regex20251119,
    Bm25_20251119,
}

impl HostedToolSearchVersion {
    pub(crate) fn from_declaration(tool: &ToolParam) -> Option<Self> {
        match (tool.tool_type(), tool.name()) {
            (Some("tool_search_tool_regex_20251119"), Some("tool_search_tool_regex")) => Some(Self::Regex20251119),
            (Some("tool_search_tool_bm25_20251119"), Some("tool_search_tool_bm25")) => Some(Self::Bm25_20251119),
            _ => None,
        }
    }
}

impl ToolParam {
    pub(crate) fn is_hosted_tool_search(&self) -> bool {
        HostedToolSearchVersion::from_declaration(self).is_some()
    }
}

// Inspect the discriminator before decoding its fields: an MCP toolset has no
// function name, and malformed known declarations must never enter the fallback.
struct DeclarationFields;

impl<'de> Visitor<'de> for DeclarationFields {
    type Value = Map<String, Value>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Messages tool declaration")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut fields = Map::new();
        while let Some((key, value)) = access.next_entry::<String, Value>()? {
            if fields.contains_key(&key)
                && matches!(
                    key.as_str(),
                    "type"
                        | "name"
                        | "description"
                        | "input_schema"
                        | "mcp_server_name"
                        | "default_config"
                        | "configs"
                        | "cache_control"
                )
            {
                return Err(M::Error::custom(format!("duplicate tool declaration field `{key}`")));
            }
            fields.insert(key, value);
        }
        Ok(fields)
    }
}

impl<'de> Deserialize<'de> for ToolParam {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut fields = deserializer.deserialize_map(DeclarationFields)?;
        let tool_type: Option<String> = fields
            .remove("type")
            .map(serde_json::from_value::<Option<String>>)
            .transpose()
            .map_err(D::Error::custom)?
            .flatten();
        if tool_type.as_deref() == Some("mcp_toolset") {
            return serde_json::from_value(Value::Object(fields))
                .map(Self::McpToolset)
                .map_err(D::Error::custom);
        }
        let fields = serde_json::from_value(Value::Object(fields)).map_err(D::Error::custom)?;
        Ok(match tool_type {
            None => Self::Function(fields),
            Some(tool_type) => {
                if let Some(version) = NativeToolVersion::find(NATIVE_WEB_SEARCH_VERSIONS, Some(&tool_type)) {
                    Self::WebSearch(MessagesNativeTool { version, fields })
                } else if let Some(version) = NativeToolVersion::find(NATIVE_WEB_FETCH_VERSIONS, Some(&tool_type)) {
                    Self::WebFetch(MessagesNativeTool { version, fields })
                } else {
                    Self::Provider(MessagesProviderTool { tool_type, fields })
                }
            }
        })
    }
}

impl Serialize for ToolParam {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct WireToolRef<'a> {
            #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
            tool_type: Option<&'a str>,
            #[serde(flatten)]
            fields: &'a MessagesToolFields,
        }
        match self {
            Self::McpToolset(toolset) => {
                #[derive(Serialize)]
                struct McpWire<'a> {
                    #[serde(rename = "type")]
                    kind: &'static str,
                    #[serde(flatten)]
                    fields: &'a MessagesMcpToolset,
                }
                McpWire {
                    kind: "mcp_toolset",
                    fields: toolset,
                }
                .serialize(serializer)
            }
            Self::Function(fields)
            | Self::WebSearch(MessagesNativeTool { fields, .. })
            | Self::WebFetch(MessagesNativeTool { fields, .. })
            | Self::Provider(MessagesProviderTool { fields, .. }) => WireToolRef {
                tool_type: self.tool_type(),
                fields,
            }
            .serialize(serializer),
        }
    }
}

// Preserve the named-tool acceptance surface and add the separate MCP toolset shape.
#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for ToolParam {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::{
            ObjectBuilder,
            schema::{AdditionalProperties, AnyOfBuilder, SchemaType, Type},
        };
        let optional_string = || ObjectBuilder::new().schema_type(SchemaType::from_iter([Type::String, Type::Null]));
        let named = ObjectBuilder::new()
            .schema_type(SchemaType::new(Type::Object))
            .description(Some(
                "A function tool, a supported native web tool, or an opaque provider tool.",
            ))
            .property("name", ObjectBuilder::new().schema_type(SchemaType::new(Type::String)))
            .required("name")
            .property("description", optional_string())
            .property("input_schema", ObjectBuilder::new().schema_type(SchemaType::AnyValue))
            .property("type", optional_string().pattern(Some("^(?!mcp_toolset$)")))
            .additional_properties(Some(AdditionalProperties::FreeForm(true)));
        AnyOfBuilder::new()
            .item(named)
            .item(
                ObjectBuilder::new()
                    .schema_type(SchemaType::new(Type::Object))
                    .property(
                        "type",
                        ObjectBuilder::new()
                            .schema_type(SchemaType::new(Type::String))
                            .enum_values(Some(["mcp_toolset"])),
                    )
                    .required("type")
                    .property(
                        "mcp_server_name",
                        ObjectBuilder::new().schema_type(SchemaType::new(Type::String)),
                    )
                    .required("mcp_server_name")
                    .property(
                        "default_config",
                        AnyOfBuilder::new()
                            .item(utoipa::openapi::Ref::from_schema_name("McpToolConfig"))
                            .item(ObjectBuilder::new().schema_type(SchemaType::new(Type::Null))),
                    )
                    .property(
                        "configs",
                        AnyOfBuilder::new()
                            .item(
                                ObjectBuilder::new().additional_properties(Some(AdditionalProperties::RefOr(
                                    utoipa::openapi::Ref::from_schema_name("McpToolConfig").into(),
                                ))),
                            )
                            .item(ObjectBuilder::new().schema_type(SchemaType::new(Type::Null))),
                    )
                    .additional_properties(Some(AdditionalProperties::FreeForm(true))),
            )
            .into()
    }
}

#[cfg(feature = "openapi")]
impl utoipa::ToSchema for ToolParam {}

#[cfg(test)]
mod tests;
