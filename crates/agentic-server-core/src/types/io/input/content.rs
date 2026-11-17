//! Typed message content parts and their wire deserialization.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::utils::common::deserialize_from_value;

use super::file_text::{InputFileError, kinds, text_content_from_bytes};
use super::resolved_file::ResolvedFiles;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

impl InputImageContent {
    /// Replace a `file_id` with the resolved image's inline data URL.
    ///
    /// The stored filename decides the media type, as `OpenAI` does; a part without a
    /// reference is unchanged.
    pub(crate) fn inline_file(&mut self, files: &ResolvedFiles) -> Result<(), InputFileError> {
        let Some(file_id) = self.file_id.as_deref() else {
            return Ok(());
        };
        let Some((object, bytes)) = files.get(file_id) else {
            return Err(InputFileError::Unresolved);
        };
        let media_type = kinds::image_media_type(&object.filename)
            .ok_or_else(|| InputFileError::NotImage(kinds::extension_label(&object.filename)))?;
        let mut data_url = String::new();
        data_url.push_str("data:");
        data_url.push_str(media_type);
        data_url.push_str(";base64,");
        STANDARD.encode_string(bytes, &mut data_url);
        self.image_url = Some(data_url);
        self.file_id = None;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

impl InputFileContent {
    /// The model-facing `input_text` part for this file.
    ///
    /// Inline `file_data` and a resolved `file_id` share this conversion. A resolved
    /// file's stored filename decides the text kind and supplies the `File: <name>` prefix.
    pub(crate) fn to_text(&self, files: &ResolvedFiles) -> Result<InputTextContent, InputFileError> {
        let Some(file_id) = self.file_id.as_deref() else {
            return InputTextContent::try_from(self);
        };
        let Some((object, bytes)) = files.get(file_id) else {
            return Err(InputFileError::Unresolved);
        };
        if !kinds::is_text_filename(&object.filename) {
            return Err(InputFileError::NotTextFile {
                extension: kinds::extension_label(&object.filename),
            });
        }
        text_content_from_bytes(Some(&object.filename), bytes, &self.extra)
    }
}

/// A refusal in rehydrated assistant history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
