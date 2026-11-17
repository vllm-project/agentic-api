//! Typed message content parts and their wire deserialization.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::types::file_search::FileObject;
use crate::utils::common::deserialize_from_value;

use super::file_text::{InputFileError, kinds, text_content_from_bytes};
use super::{InputValidationError, ToolCallOutput, ToolOutputContent};

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

/// Files resolved for one model-facing request, keyed by the client's `file_id`.
///
/// `FileHandler::resolve` builds it and only `normalized_model_input` reads it, to replace a
/// client's `file_id` with inline content in the model copy. The value is the stored object,
/// whose filename decides the file kind, and its bytes.
pub type ResolvedFiles = HashMap<String, (FileObject, Vec<u8>)>;

impl ToolCallOutput {
    /// Replace `file_id` references in structured output parts with inline content.
    ///
    /// Parts without a reference keep their content contract and pass through unchanged.
    pub(crate) fn inline_files(
        &mut self,
        files: &ResolvedFiles,
        item_index: usize,
    ) -> Result<(), InputValidationError> {
        let Self::Content(parts) = self else {
            return Ok(());
        };
        for (part_index, part) in parts.iter_mut().enumerate() {
            match part {
                ToolOutputContent::InputFile(file) if file.file_id.is_some() => {
                    let text = file.to_text(files).map_err(|reason| {
                        InputValidationError::unsupported_output_file(item_index, part_index, reason)
                    })?;
                    *part = ToolOutputContent::InputText(text);
                }
                ToolOutputContent::InputImage(image) if image.file_id.is_some() => {
                    image.inline_file(files).map_err(|reason| {
                        InputValidationError::unsupported_output_file(item_index, part_index, reason)
                    })?;
                }
                ToolOutputContent::InputText(_)
                | ToolOutputContent::InputFile(_)
                | ToolOutputContent::InputImage(_) => {}
            }
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn resolved(filename: &str, bytes: &[u8]) -> (FileObject, Vec<u8>) {
        (
            FileObject {
                id: format!("file-{filename}"),
                object: "file".to_owned(),
                bytes: i64::try_from(bytes.len()).expect("test fixture fits i64"),
                created_at: 0,
                filename: filename.to_owned(),
                purpose: "user_data".to_owned(),
                status: "processed".to_owned(),
            },
            bytes.to_vec(),
        )
    }

    fn files() -> ResolvedFiles {
        ResolvedFiles::from([
            ("file-notes".to_owned(), resolved("notes.txt", b"hello")),
            ("file-report".to_owned(), resolved("report.pdf", b"%PDF")),
            ("file-plot".to_owned(), resolved("plot.png", b"png")),
            ("file-binary".to_owned(), resolved("notes.txt", &[0xff])),
        ])
    }

    fn output(parts: Value) -> ToolCallOutput {
        serde_json::from_value(parts).expect("tool output")
    }

    #[test]
    fn resolved_text_file_uses_the_stored_name_and_keeps_extensions() {
        let file: InputFileContent = serde_json::from_value(json!({
            "type": "input_file", "file_id": "file-notes",
            "prompt_cache_breakpoint": {"mode": "explicit"}
        }))
        .unwrap();
        let text = file.to_text(&files()).unwrap();
        assert_eq!(text.text, "File: notes.txt\nhello");
        assert_eq!(
            text.extra.get("prompt_cache_breakpoint"),
            Some(&json!({"mode": "explicit"}))
        );
    }

    #[test]
    fn inline_text_file_uses_the_same_conversion() {
        let file: InputFileContent = serde_json::from_value(json!({
            "type": "input_file", "filename": "notes.txt", "file_data": "data:text/plain;base64,aGVsbG8="
        }))
        .unwrap();
        let text = file.to_text(&ResolvedFiles::new()).unwrap();
        assert_eq!(text.text, "File: notes.txt\nhello");
    }

    #[test]
    fn resolved_text_file_rejects_a_wrong_kind_or_an_unresolved_id() {
        let pdf = InputFileContent {
            file_id: Some("file-report".into()),
            ..InputFileContent::default()
        };
        assert_eq!(
            pdf.to_text(&files()),
            Err(InputFileError::NotTextFile {
                extension: ".pdf".into()
            })
        );
        let missing = InputFileContent {
            file_id: Some("file-missing".into()),
            ..InputFileContent::default()
        };
        assert_eq!(missing.to_text(&files()), Err(InputFileError::Unresolved));
    }

    #[test]
    fn inline_image_replaces_the_reference_and_keeps_other_fields() {
        let mut image: InputImageContent = serde_json::from_value(json!({
            "type": "input_image", "file_id": "file-plot", "detail": "high", "extra_field": "keep"
        }))
        .unwrap();
        image.inline_file(&files()).unwrap();
        let expected = format!("data:image/png;base64,{}", STANDARD.encode(b"png"));
        assert_eq!(image.image_url.as_deref(), Some(expected.as_str()));
        assert!(image.file_id.is_none());
        assert_eq!(image.detail.as_deref(), Some("high"));
        assert_eq!(image.extra.get("extra_field"), Some(&json!("keep")));
    }

    #[test]
    fn inline_image_rejects_a_wrong_kind_or_an_unresolved_id() {
        let mut text = InputImageContent {
            file_id: Some("file-notes".into()),
            ..InputImageContent::default()
        };
        assert_eq!(text.inline_file(&files()), Err(InputFileError::NotImage(".txt".into())));
        let mut missing = InputImageContent {
            file_id: Some("file-missing".into()),
            ..InputImageContent::default()
        };
        assert_eq!(missing.inline_file(&files()), Err(InputFileError::Unresolved));
    }

    #[test]
    fn tool_output_text_references_become_input_text_at_the_output_position() {
        let mut output = output(json!([
            {"type": "input_text", "text": "prefix"},
            {"type": "input_file", "file_id": "file-notes"}
        ]));
        output.inline_files(&files(), 1).unwrap();
        assert_eq!(
            serde_json::to_value(&output).unwrap(),
            json!([
                {"type": "input_text", "text": "prefix"},
                {"type": "input_text", "text": "File: notes.txt\nhello"}
            ])
        );
    }

    #[test]
    fn tool_output_image_references_become_data_urls_and_inline_parts_pass_through() {
        let mut output = output(json!([
            {"type": "input_image", "file_id": "file-plot", "detail": "high"},
            {"type": "input_file", "filename": "local.txt", "file_data": "aGk="},
            {"type": "input_text", "text": "tail"}
        ]));
        output.inline_files(&files(), 0).unwrap();
        let expected = format!("data:image/png;base64,{}", STANDARD.encode(b"png"));
        assert_eq!(
            serde_json::to_value(&output).unwrap(),
            json!([
                {"type": "input_image", "image_url": expected, "detail": "high"},
                {"type": "input_file", "filename": "local.txt", "file_data": "aGk="},
                {"type": "input_text", "text": "tail"}
            ])
        );
    }

    #[test]
    fn tool_output_wrong_kind_references_use_the_recorded_envelope_without_a_position() {
        for (part, expected) in [
            (
                json!({"type": "input_file", "file_id": "file-report"}),
                InputFileError::NotTextFile {
                    extension: ".pdf".into(),
                },
            ),
            (
                json!({"type": "input_image", "file_id": "file-notes"}),
                InputFileError::NotImage(".txt".into()),
            ),
        ] {
            let error = output(json!([part])).inline_files(&files(), 2).unwrap_err();
            assert_eq!(error.resolved_file_reason(), Some(&expected));
            assert_eq!(error.to_string(), expected.to_string());
            assert!(
                !error.to_string().starts_with("input["),
                "the recorded envelope has no position: {error}"
            );
        }
    }

    #[test]
    fn tool_output_content_and_lookup_failures_keep_the_output_position() {
        let error = output(json!([{"type": "input_file", "file_id": "file-binary"}]))
            .inline_files(&files(), 2)
            .unwrap_err();
        assert!(error.resolved_file_reason().is_none());
        assert!(
            error.to_string().starts_with("input[2].output[0]: "),
            "content failures keep their position: {error}"
        );

        let error = output(json!([{"type": "input_file", "file_id": "file-missing"}]))
            .inline_files(&files(), 2)
            .unwrap_err();
        assert!(error.resolved_file_reason().is_none());
        assert!(
            error.to_string().starts_with("input[2].output[0]: "),
            "a skipped lookup is reported, not panicked: {error}"
        );
    }
}
