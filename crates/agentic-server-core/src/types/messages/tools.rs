//! Messages tool declarations, classified without interpreting provider extensions.
//!
//! Known tool kinds have distinct variants. Unknown versioned tools retain their
//! discriminator and opaque fields; recognition does not grant execution ownership.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

pub const NATIVE_WEB_SEARCH_TYPE: &str = "web_search_20250305";
pub const NATIVE_WEB_FETCH_TYPE: &str = "web_fetch_20250910";

/// A Messages wire declaration. Add a variant and its discriminator mapping when
/// supporting a new kind; routing and execution remain the tool layer's decision.
#[derive(Debug, Clone)]
pub enum ToolParam {
    Function(MessagesToolFields),
    WebSearch(MessagesToolFields),
    WebFetch(MessagesToolFields),
    Provider(MessagesProviderTool),
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
    pub fn fields(&self) -> &MessagesToolFields {
        match self {
            Self::Function(fields) | Self::WebSearch(fields) | Self::WebFetch(fields) => fields,
            Self::Provider(tool) => &tool.fields,
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.fields().name
    }

    #[must_use]
    pub fn tool_type(&self) -> Option<&str> {
        match self {
            Self::Function(_) => None,
            Self::WebSearch(_) => Some(NATIVE_WEB_SEARCH_TYPE),
            Self::WebFetch(_) => Some(NATIVE_WEB_FETCH_TYPE),
            Self::Provider(tool) => Some(&tool.tool_type),
        }
    }
}

// Dispatch on the discriminator explicitly: an untagged enum with a permissive
// fallback could otherwise accept a malformed known declaration as another kind.
#[derive(Deserialize)]
struct WireTool {
    #[serde(rename = "type", default)]
    tool_type: Option<String>,
    #[serde(flatten)]
    fields: MessagesToolFields,
}

impl<'de> Deserialize<'de> for ToolParam {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let WireTool { tool_type, fields } = WireTool::deserialize(deserializer)?;
        Ok(match tool_type {
            None => Self::Function(fields),
            Some(tool_type) => match tool_type.as_str() {
                NATIVE_WEB_SEARCH_TYPE => Self::WebSearch(fields),
                NATIVE_WEB_FETCH_TYPE => Self::WebFetch(fields),
                _ => Self::Provider(MessagesProviderTool { tool_type, fields }),
            },
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
        WireToolRef {
            tool_type: self.tool_type(),
            fields: self.fields(),
        }
        .serialize(serializer)
    }
}

// Keep the published schema's acceptance surface: named tools may omit type or
// use any provider discriminator. This refactor does not enable new tool kinds.
#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for ToolParam {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::{
            ObjectBuilder,
            schema::{AdditionalProperties, SchemaType, Type},
        };
        let optional_string = || ObjectBuilder::new().schema_type(SchemaType::from_iter([Type::String, Type::Null]));
        ObjectBuilder::new()
            .schema_type(SchemaType::new(Type::Object))
            .description(Some(
                "A function tool, a supported native web tool, or an opaque provider tool.",
            ))
            .property("name", ObjectBuilder::new().schema_type(SchemaType::new(Type::String)))
            .required("name")
            .property("description", optional_string())
            .property("input_schema", ObjectBuilder::new().schema_type(SchemaType::AnyValue))
            .property("type", optional_string())
            .additional_properties(Some(AdditionalProperties::FreeForm(true)))
            .into()
    }
}

#[cfg(feature = "openapi")]
impl utoipa::ToSchema for ToolParam {}

#[cfg(test)]
mod tests;
