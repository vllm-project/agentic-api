//! Typed message content parts and their wire deserialization.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::utils::common::deserialize_from_value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InputTextContent {
    pub text: String,
    /// Unmodeled extension fields, preserved so the typed path forwards the
    /// part exactly as the client sent it and leaves support decisions to the
    /// upstream, like the raw proxy path does.
    #[serde(default, flatten, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl InputTextContent {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InputImageContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Unmodeled extension fields, preserved so the typed path forwards the
    /// part exactly as the client sent it and leaves support decisions to the
    /// upstream, like the raw proxy path does.
    #[serde(default, flatten, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InputFileContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Unmodeled extension fields, preserved so the typed path forwards the
    /// part exactly as the client sent it and leaves support decisions to the
    /// upstream, like the raw proxy path does.
    #[serde(default, flatten, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// A refusal in rehydrated assistant history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RefusalContent {
    pub refusal: String,
    /// Unmodeled extension fields, preserved so the typed path forwards the
    /// part exactly as the client sent it and leaves support decisions to the
    /// upstream, like the raw proxy path does.
    #[serde(default, flatten, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// Content item inside a message input.
///
/// Serialized as an internally-tagged enum — `"type"` is the variant
/// discriminant so the inner structs must NOT redeclare a `type_` field.
/// `output_text` and `reasoning_text` reuse [`InputTextContent`] since they
/// carry only a `text` field; they and `refusal` are preserved so the upstream
/// sees the full assistant history.
///
/// Deserialization is hand-written so a part of a type the gateway does not
/// model keeps its type name in [`InputContent::Unknown`]. That variant never
/// serializes: typed paths reject it before the request reaches storage or the
/// upstream, so no synthetic part is ever forwarded or persisted in place of
/// what the client sent.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContent {
    InputText(InputTextContent),
    InputImage(InputImageContent),
    /// Preserved on the wire; support is validated after the routing decision.
    InputFile(InputFileContent),
    /// Assistant output text in rehydrated history.
    OutputText(InputTextContent),
    /// Assistant refusal in rehydrated history.
    Refusal(RefusalContent),
    /// Reasoning step text in rehydrated history.
    ReasoningText(InputTextContent),
    /// A content type this gateway does not model, carrying the type name the
    /// client sent so the rejection can name it.
    #[serde(skip)]
    Unknown(String),
}

impl<'de> Deserialize<'de> for InputContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut value = Value::deserialize(deserializer)?;
        let kind = value
            .as_object_mut()
            .and_then(|object| object.remove("type"))
            .and_then(|kind| match kind {
                Value::String(kind) => Some(kind),
                _ => None,
            })
            .ok_or_else(|| serde::de::Error::custom("message content part is missing a string `type`"))?;
        let part = match kind.as_str() {
            "input_text" => deserialize_from_value(value).map(Self::InputText),
            "input_image" => deserialize_from_value(value).map(Self::InputImage),
            "input_file" => deserialize_from_value(value).map(Self::InputFile),
            "output_text" => deserialize_from_value(value).map(Self::OutputText),
            "refusal" => deserialize_from_value(value).map(Self::Refusal),
            "reasoning_text" => deserialize_from_value(value).map(Self::ReasoningText),
            _ => return Ok(Self::Unknown(kind)),
        };
        part.map_err(serde::de::Error::custom)
    }
}
