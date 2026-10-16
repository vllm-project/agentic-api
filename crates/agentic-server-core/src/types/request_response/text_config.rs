use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Responses text-generation settings forwarded to the upstream service.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ResponseTextConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ResponseTextFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<String>,
    /// Unmodeled extension fields preserved for upstream compatibility.
    #[serde(default)]
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Output format requested through [`ResponseTextConfig`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResponseTextFormat {
    Text {
        /// Unmodeled extension fields preserved for upstream compatibility.
        #[serde(default)]
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    JsonObject {
        /// Unmodeled extension fields preserved for upstream compatibility.
        #[serde(default)]
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    JsonSchema {
        name: String,
        schema: serde_json::Map<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
        /// Unmodeled extension fields preserved for upstream compatibility.
        #[serde(default)]
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
}
