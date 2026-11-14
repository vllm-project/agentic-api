//! Shared lossless Messages blocks for requests and durable thread history.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;

use super::parameters::{CacheControl, RequestField};
use super::request::ToolResultContent;

/// Known blocks validate their payload; future provider blocks retain their fields.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ContentBlock {
    Known(KnownContentBlock),
    Provider(ProviderContentBlock),
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        // A malformed known block is an error, never a permissive fallback.
        let known = KnownContentBlock::deserialize(&value).map_err(serde::de::Error::custom)?;
        match known {
            KnownContentBlock::Unknown => ProviderContentBlock::deserialize(value)
                .map(Self::Provider)
                .map_err(serde::de::Error::custom),
            known => Ok(Self::Known(known)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KnownContentBlock {
    Text {
        #[serde(default)]
        text: String,
        #[serde(default, skip_serializing_if = "RequestField::is_missing")]
        citations: RequestField<Vec<Citation>>,
        #[serde(default, skip_serializing_if = "RequestField::is_missing")]
        cache_control: RequestField<CacheControl>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default, skip_serializing_if = "RequestField::is_missing")]
        signature: RequestField<String>,
        #[serde(default, skip_serializing_if = "RequestField::is_missing")]
        cache_control: RequestField<CacheControl>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    RedactedThinking {
        data: String,
        #[serde(default, skip_serializing_if = "RequestField::is_missing")]
        cache_control: RequestField<CacheControl>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "RequestField::is_missing")]
        input: RequestField<Value>,
        #[serde(default, skip_serializing_if = "RequestField::is_missing")]
        cache_control: RequestField<CacheControl>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default, skip_serializing_if = "RequestField::is_missing")]
        content: RequestField<ToolResultContent>,
        #[serde(default, skip_serializing_if = "RequestField::is_missing")]
        is_error: RequestField<bool>,
        #[serde(default, skip_serializing_if = "RequestField::is_missing")]
        cache_control: RequestField<CacheControl>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    McpToolUse {
        id: String,
        name: String,
        server_name: String,
        input: Map<String, Value>,
        #[serde(default, skip_serializing_if = "RequestField::is_missing")]
        cache_control: RequestField<CacheControl>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    McpToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(default)]
        is_error: bool,
        #[serde(default, skip_serializing_if = "RequestField::is_missing")]
        cache_control: RequestField<CacheControl>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    #[serde(other)]
    Unknown,
}

/// Provider blocks whose payload is not interpreted by the gateway. Media,
/// compaction, and upstream server-tool extensions survive without a second history type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    pub cache_control: RequestField<CacheControl>,
    #[serde(flatten)]
    pub fields: HashMap<String, Value>,
}

/// Citation metadata is retained alongside its typed location fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    pub cited_text: RequestField<String>,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    pub document_index: RequestField<u32>,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    pub document_title: RequestField<String>,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    pub start_char_index: RequestField<u32>,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    pub end_char_index: RequestField<u32>,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    pub start_page_number: RequestField<u32>,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    pub end_page_number: RequestField<u32>,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    pub start_block_index: RequestField<u32>,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    pub end_block_index: RequestField<u32>,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    pub url: RequestField<String>,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    pub title: RequestField<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl ContentBlock {
    pub(crate) fn cache_breakpoints(&self) -> usize {
        let (cache_control, content) = match self {
            Self::Known(
                KnownContentBlock::Text { cache_control, .. }
                | KnownContentBlock::Thinking { cache_control, .. }
                | KnownContentBlock::RedactedThinking { cache_control, .. }
                | KnownContentBlock::ToolUse { cache_control, .. }
                | KnownContentBlock::McpToolUse { cache_control, .. },
            ) => (cache_control, None),
            Self::Known(KnownContentBlock::ToolResult {
                cache_control, content, ..
            }) => (cache_control, content.as_ref()),
            Self::Known(KnownContentBlock::McpToolResult {
                cache_control, content, ..
            }) => (cache_control, Some(content)),
            Self::Known(KnownContentBlock::Unknown) => return 0,
            Self::Provider(block) => (&block.cache_control, None),
        };
        usize::from(cache_control.as_ref().is_some())
            + match content {
                Some(ToolResultContent::Text(_)) | None => 0,
                Some(ToolResultContent::Blocks(blocks)) => blocks
                    .iter()
                    .filter(|block| block.cache_control.as_ref().is_some())
                    .count(),
            }
    }

    pub(crate) fn is_compaction(&self) -> bool {
        matches!(self, Self::Provider(block) if block.kind == "compaction")
    }
}

#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for ContentBlock {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::{
            ObjectBuilder,
            schema::{AnyOfBuilder, SchemaType, Type},
        };

        let str_type = || ObjectBuilder::new().schema_type(SchemaType::new(Type::String));

        let cache_control = || {
            AnyOfBuilder::new()
                .item(utoipa::openapi::Ref::from_schema_name("CacheControl"))
                .item(ObjectBuilder::new().schema_type(SchemaType::new(Type::Null)))
        };

        AnyOfBuilder::new()
            .item(
                ObjectBuilder::new()
                    .property("type", str_type().enum_values(Some(["text"])))
                    .required("type")
                    .property("text", str_type()),
            )
            .item(
                ObjectBuilder::new()
                    .property("type", str_type().enum_values(Some(["thinking"])))
                    .required("type")
                    .property("thinking", str_type())
                    .property("signature", str_type()),
            )
            .item(
                ObjectBuilder::new()
                    .property("type", str_type().enum_values(Some(["tool_use"])))
                    .required("type")
                    .property("id", str_type())
                    .required("id")
                    .property("name", str_type())
                    .required("name")
                    .property("input", ObjectBuilder::new()),
            )
            .item(
                ObjectBuilder::new()
                    .property("type", str_type().enum_values(Some(["tool_result"])))
                    .required("type")
                    .property("tool_use_id", str_type())
                    .required("tool_use_id")
                    .property("content", utoipa::openapi::Ref::from_schema_name("ToolResultContent")),
            )
            .item(
                ObjectBuilder::new()
                    .property("type", str_type().enum_values(Some(["mcp_tool_use"])))
                    .required("type")
                    .property("id", str_type())
                    .required("id")
                    .property("name", str_type())
                    .required("name")
                    .property("server_name", str_type())
                    .required("server_name")
                    .property("input", ObjectBuilder::new().schema_type(SchemaType::new(Type::Object)))
                    .required("input")
                    .property("cache_control", cache_control()),
            )
            .item(
                ObjectBuilder::new()
                    .property("type", str_type().enum_values(Some(["mcp_tool_result"])))
                    .required("type")
                    .property("tool_use_id", str_type())
                    .required("tool_use_id")
                    .property(
                        "is_error",
                        ObjectBuilder::new().schema_type(SchemaType::new(Type::Boolean)),
                    )
                    .property("content", utoipa::openapi::Ref::from_schema_name("ToolResultContent"))
                    .required("content")
                    .property("cache_control", cache_control()),
            )
            .item(
                ObjectBuilder::new()
                    .property(
                        "type",
                        str_type().pattern(Some("^(?!(?:mcp_tool_use|mcp_tool_result)$)")),
                    )
                    .required("type"),
            )
            .into()
    }
}
#[cfg(feature = "openapi")]
impl utoipa::ToSchema for ContentBlock {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ContentBlock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_blocks_preserve_known_nullable_metadata_and_provider_payloads() {
        for value in [
            json!({"type":"text","text":"snow 雪","citations":null,"cache_control":null,"extension":{"a":1}}),
            json!({"type":"text","text":"proof","citations":[{"type":"char_location","cited_text":"proof","document_index":0,"document_title":null,"start_char_index":0,"end_char_index":5,"provider_extension":"keep"}]}),
            json!({"type":"thinking","thinking":"private","signature":"signed","extension":true}),
            json!({"type":"redacted_thinking","data":"opaque"}),
            json!({"type":"tool_result","tool_use_id":"t","is_error":true}),
            json!({"type":"tool_use","id":"t","name":"client"}),
            json!({"type":"tool_use","id":"t","name":"client","input":null}),
            json!({"type":"tool_result","tool_use_id":"t","is_error":true,"content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="},"cache_control":null}]}),
            json!({"type":"document","source":{"type":"text","media_type":"text/plain","data":"proof"},"citations":{"enabled":true}}),
            json!({"type":"server_tool_use","id":"server-t","name":"search","input":{"q":"q"}}),
            json!({"type":"future_block","opaque":{"nested":[1,null,"雪"]}}),
            json!({"type":"mcp_tool_use","id":"m","name":"echo","server_name":"test","input":{"text":"雪"},"cache_control":null,"opaque":{"keep":true}}),
            json!({"type":"mcp_tool_result","tool_use_id":"m","is_error":false,"content":[{"type":"text","text":"ok","cache_control":{"type":"ephemeral"},"opaque":true}],"extension":null}),
        ] {
            let block: ContentBlock = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(block).unwrap(), value);
        }
    }

    #[test]
    fn malformed_known_blocks_cannot_fall_through_to_provider_extensions() {
        for value in [
            json!({"type":"tool_use","name":"missing-id"}),
            json!({"type":"redacted_thinking","data":42}),
            json!({"type":"text","text":"a","citations":42}),
            json!({"type":"mcp_tool_use","id":"m","name":"echo","input":{}}),
            json!({"type":"mcp_tool_use","id":"m","name":"echo","server_name":"s","input":[]}),
            json!({"type":"mcp_tool_result","is_error":false,"content":[]}),
            json!({"type":"mcp_tool_result","tool_use_id":"m","is_error":false}),
            json!({"type":"mcp_tool_result","tool_use_id":"m","is_error":null,"content":[]}),
        ] {
            assert!(serde_json::from_value::<ContentBlock>(value).is_err());
        }
    }
}
