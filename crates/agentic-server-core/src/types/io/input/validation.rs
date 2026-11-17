//! Message-content validation for typed Responses execution.

use std::fmt;

use thiserror::Error;

use super::file_text::InputFileError;
use super::window::expand_carried_windows;
use super::{
    InputContent, InputItem, InputMessage, InputMessageContent, ResponsesInput, ToolCallOutput, ToolOutputContent,
};
use crate::types::io::{InOutItem, OutputItem};

/// Invalid message content, with its position in the request history when it has one.
#[derive(Debug, Error)]
pub struct InputValidationError {
    item_index: Option<usize>,
    #[source]
    source: MessageContentError,
}

impl fmt::Display for InputValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            MessageContentError::MutuallyExclusive { location, field } => write!(
                f,
                "Mutually exclusive parameters: '{location}'. Ensure you are only providing one of: \
                 'file_id' or '{field}'."
            ),
            MessageContentError::UnsupportedPart { location, .. }
            | MessageContentError::UnsupportedFile { location, .. } => {
                write!(f, "{location}: {}", self.source)
            }
            MessageContentError::Empty | MessageContentError::EncryptedReasoning => {
                write!(
                    f,
                    "input[{}].content: {}",
                    self.item_index
                        .expect("a content rejection is reported with its item position"),
                    self.source
                )
            }
            // The recorded envelope for a resolved file carries no request position.
            MessageContentError::ResolvedFile { reason } => f.write_str(&reason.to_string()),
        }
    }
}

/// One `file_id` reference in the request input and where it appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileReference {
    /// The client's `file_id`.
    pub(crate) file_id: String,
    /// The wire part type that carries the reference: `input_file` or `input_image`.
    pub(crate) part: &'static str,
    /// The request position: `input[i].content[p]` or `input[i].output[p]`.
    pub(crate) location: String,
}

/// Every `file_id` reference in `input`, in request order, with its position.
///
/// Carried compaction windows are expanded so a restored window's retained messages are
/// included. The executor resolves each reference before building the model-facing request;
/// split execution rejects input that has any.
pub(crate) fn file_references(input: &ResponsesInput) -> Vec<FileReference> {
    let mut references = Vec::new();
    for (item_index, item) in expand_carried_windows(input.input_items()).iter().enumerate() {
        match item.as_ref() {
            InputItem::Message(message) => collect_message_references(message, item_index, &mut references),
            InputItem::FunctionCallOutput(output) => {
                collect_tool_output_references(&output.output, item_index, &mut references);
            }
            InputItem::CustomToolCallOutput(output) => {
                collect_tool_output_references(&output.output, item_index, &mut references);
            }
            _ => {}
        }
    }
    references
}

fn collect_message_references(message: &InputMessage, item_index: usize, references: &mut Vec<FileReference>) {
    let InputMessageContent::Parts(parts) = &message.content else {
        return;
    };
    for (part_index, part) in parts.iter().enumerate() {
        let reference = match part {
            InputContent::InputFile(file) => file.file_id.as_deref().map(|file_id| (file_id, "input_file")),
            InputContent::InputImage(image) => image.file_id.as_deref().map(|file_id| (file_id, "input_image")),
            _ => None,
        };
        if let Some((file_id, part)) = reference {
            references.push(FileReference {
                file_id: file_id.to_owned(),
                part,
                location: content_location(item_index, part_index),
            });
        }
    }
}

fn collect_tool_output_references(output: &ToolCallOutput, item_index: usize, references: &mut Vec<FileReference>) {
    let ToolCallOutput::Content(parts) = output else {
        return;
    };
    for (part_index, part) in parts.iter().enumerate() {
        let reference = match part {
            ToolOutputContent::InputFile(file) => file.file_id.as_deref().map(|file_id| (file_id, "input_file")),
            ToolOutputContent::InputImage(image) => image.file_id.as_deref().map(|file_id| (file_id, "input_image")),
            ToolOutputContent::InputText(_) => None,
        };
        if let Some((file_id, part)) = reference {
            references.push(FileReference {
                file_id: file_id.to_owned(),
                part,
                location: output_location(item_index, part_index),
            });
        }
    }
}

impl InputValidationError {
    /// A reasoning item at `input[item_index]` that carries only provider-encrypted state, which vLLM cannot replay.
    pub(super) fn encrypted_reasoning(item_index: usize) -> Self {
        Self {
            item_index: Some(item_index),
            source: MessageContentError::EncryptedReasoning,
        }
    }

    /// A message file at `input[item_index].content[part_index]` that cannot be converted to text.
    ///
    /// A resolved file whose kind does not match its part uses the recorded envelope: the
    /// message names `input` and no request position.
    pub(super) fn unsupported_file(item_index: usize, part_index: usize, reason: InputFileError) -> Self {
        if reason.is_resolved_kind() {
            Self {
                item_index: None,
                source: MessageContentError::ResolvedFile { reason },
            }
        } else {
            Self {
                item_index: Some(item_index),
                source: file_error(content_location(item_index, part_index), reason),
            }
        }
    }

    /// A tool-output file at `input[item_index].output[part_index]` that cannot be converted.
    pub(super) fn unsupported_output_file(item_index: usize, part_index: usize, reason: InputFileError) -> Self {
        Self {
            item_index: Some(item_index),
            source: file_error(output_location(item_index, part_index), reason),
        }
    }

    /// The wrong-kind failure for a resolved file that does not match its part.
    pub(crate) fn resolved_file_reason(&self) -> Option<&InputFileError> {
        match &self.source {
            MessageContentError::ResolvedFile { reason } => Some(reason),
            _ => None,
        }
    }

    /// Whether this is the recorded mutually exclusive parameters rejection.
    pub(crate) fn is_mutually_exclusive(&self) -> bool {
        matches!(&self.source, MessageContentError::MutuallyExclusive { .. })
    }

    /// The `OpenAI` error code for this rejection, when it has a specific one.
    pub(crate) fn error_code(&self) -> Option<&'static str> {
        match &self.source {
            MessageContentError::MutuallyExclusive { .. } => Some("mutually_exclusive_parameters"),
            MessageContentError::Empty
            | MessageContentError::UnsupportedPart { .. }
            | MessageContentError::UnsupportedFile { .. }
            | MessageContentError::EncryptedReasoning
            | MessageContentError::ResolvedFile { .. } => None,
        }
    }

    /// The request parameter this rejection points at, when it has one.
    pub(crate) fn error_param(&self) -> Option<&str> {
        match &self.source {
            MessageContentError::MutuallyExclusive { location, .. } => Some(location),
            MessageContentError::Empty
            | MessageContentError::UnsupportedPart { .. }
            | MessageContentError::UnsupportedFile { .. }
            | MessageContentError::EncryptedReasoning
            | MessageContentError::ResolvedFile { .. } => None,
        }
    }
}

fn content_location(item_index: usize, part_index: usize) -> String {
    format!("input[{item_index}].content[{part_index}]")
}

fn output_location(item_index: usize, part_index: usize) -> String {
    format!("input[{item_index}].output[{part_index}]")
}

fn file_error(location: String, reason: InputFileError) -> MessageContentError {
    match reason {
        InputFileError::MutuallyExclusive { field } => MessageContentError::MutuallyExclusive { location, field },
        reason => MessageContentError::UnsupportedFile { location, reason },
    }
}

#[derive(Debug, Error)]
enum MessageContentError {
    #[error("a message must contain at least one content part")]
    Empty,
    #[error(
        "message content part type `{kind}` is not supported by the typed Responses executor; \
         provide input_text, input_image, or inline text input_file content instead"
    )]
    UnsupportedPart { location: String, kind: String },
    #[error("{reason}")]
    UnsupportedFile { location: String, reason: InputFileError },
    #[error("input content fields must not be combined")]
    MutuallyExclusive { location: String, field: &'static str },
    #[error(
        "reasoning item contains encrypted state without plaintext reasoning content and cannot be replayed to vLLM"
    )]
    EncryptedReasoning,
    /// A resolved file whose kind does not match the part that referenced it.
    #[error("{reason}")]
    ResolvedFile { reason: InputFileError },
}

impl InputMessageContent {
    fn validate(&self, item_index: usize) -> Result<(), MessageContentError> {
        match self {
            Self::Text(_) => Ok(()),
            Self::Parts(parts) if parts.is_empty() => Err(MessageContentError::Empty),
            Self::Parts(parts) => parts.iter().enumerate().try_for_each(|(part_index, part)| {
                let location = content_location(item_index, part_index);
                let kind = match part {
                    InputContent::InputFile(file) => {
                        return file.validate().map_err(|reason| file_error(location, reason));
                    }
                    InputContent::InputImage(image) => {
                        return validate_image(image, location);
                    }
                    InputContent::Unknown(kind) => kind,
                    InputContent::InputText(_)
                    | InputContent::OutputText(_)
                    | InputContent::Refusal(_)
                    | InputContent::ReasoningText(_) => return Ok(()),
                };
                Err(MessageContentError::UnsupportedPart {
                    location,
                    kind: kind.to_owned(),
                })
            }),
        }
    }
}

fn validate_image(image: &super::InputImageContent, location: String) -> Result<(), MessageContentError> {
    if image.file_id.is_some() && image.image_url.is_some() {
        return Err(MessageContentError::MutuallyExclusive {
            location,
            field: "image_url",
        });
    }
    Ok(())
}

impl InputItem {
    fn validate(&self, item_index: usize) -> Result<(), MessageContentError> {
        match self {
            Self::Message(message) => message.content.validate(item_index),
            _ => Ok(()),
        }
    }
}

impl InOutItem {
    fn validate(&self, item_index: usize) -> Result<(), MessageContentError> {
        match self {
            Self::Input(item) => item.validate(item_index),
            Self::Output(OutputItem::Message(message)) if message.content.is_empty() => Err(MessageContentError::Empty),
            Self::Output(_) => Ok(()),
        }
    }
}

impl ResponsesInput {
    /// Validate message content before typed execution or upstream construction.
    ///
    /// Run after routing: raw-proxy requests preserve their original bytes and
    /// leave content support decisions to the upstream. Restored history must
    /// pass the same validation. Structured tool outputs keep their own content
    /// contract and are not inspected here.
    ///
    /// A message `file_id` is valid input: it is resolved from the gateway file
    /// store for the model-facing request, while history keeps the reference. A
    /// message part that combines `file_id` with another content field is rejected
    /// with `OpenAI`'s mutually exclusive parameters error.
    ///
    /// # Errors
    /// Returns the item and content-part location of an empty message array, a
    /// message content type the typed executor cannot convey, a message file that is
    /// not an inline base64 UTF-8 text file, or a message part that combines `file_id`
    /// with another content field.
    pub fn validate(&self) -> Result<(), InputValidationError> {
        match self {
            Self::Text(_) => Ok(()),
            Self::Items(items) => items.iter().enumerate().try_for_each(|(item_index, item)| {
                item.validate(item_index).map_err(|source| InputValidationError {
                    item_index: Some(item_index),
                    source,
                })
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::event::MessageStatus;
    use crate::types::io::{OutputMessage, OutputTextContent};
    use serde_json::json;

    #[test]
    fn validation_preserves_original_item_and_content_positions() {
        let input: ResponsesInput = serde_json::from_value(json!([
            {"type":"function_call_output", "call_id":"call_1", "output":"done"},
            {"role":"user", "content":[{"type":"input_text", "text":"hello"}, {"type":"future_content"}]}
        ]))
        .unwrap();
        let error = input.validate().unwrap_err();
        assert_eq!(error.item_index, Some(1));
        assert!(matches!(
            &error.source,
            MessageContentError::UnsupportedPart { location, kind }
                if location == "input[1].content[1]" && kind == "future_content"
        ));
        assert!(error.to_string().starts_with("input[1].content[1]:"));
    }

    #[test]
    fn unsupported_files_keep_positions_without_echoing_references() {
        let file = json!({"type":"input_file", "file_url":"https://example.invalid/document.pdf"});
        let input: ResponsesInput = serde_json::from_value(json!([
            {"type":"function_call_output", "call_id":"call_1", "output":"done"},
            {"role":"user", "content":[{"type":"input_text", "text":"hello"}, file]}
        ]))
        .unwrap();
        let error = input.validate().unwrap_err();
        assert_eq!(error.item_index, Some(1));
        assert!(matches!(
            error.source,
            MessageContentError::UnsupportedFile {
                ref location,
                reason: InputFileError::FileUrl
            } if location == "input[1].content[1]"
        ));
        let message = error.to_string();
        assert!(message.starts_with("input[1].content[1]: input_file with file_url is not supported"));
        assert!(!message.contains("example.invalid"), "{message}");
    }

    #[test]
    fn file_references_are_valid_input() {
        let input: ResponsesInput = serde_json::from_value(json!([
            {"role":"user", "content":[{"type":"input_file", "file_id":"file-1"}]}
        ]))
        .unwrap();
        input.validate().unwrap();
    }

    #[test]
    fn image_references_are_valid_input() {
        let input: ResponsesInput = serde_json::from_value(json!([
            {"role":"user", "content":[{"type":"input_image", "file_id":"file-1", "detail":"high"}]}
        ]))
        .unwrap();
        input.validate().unwrap();
    }

    #[test]
    fn references_combined_with_inline_fields_use_openai_mutually_exclusive_errors() {
        for (part, field) in [
            (
                json!({"type":"input_file", "file_id":"file-1", "filename":"a.txt"}),
                "filename",
            ),
            (
                json!({"type":"input_file", "file_id":"file-1", "file_data":"aGk="}),
                "file_data",
            ),
            (
                json!({"type":"input_file", "file_id":"file-1", "file_url":"https://example.invalid/a.txt"}),
                "file_url",
            ),
            (
                json!({"type":"input_image", "file_id":"file-1", "image_url":"data:image/png;base64,AA=="}),
                "image_url",
            ),
        ] {
            let input: ResponsesInput = serde_json::from_value(json!([{"role":"user", "content":[part]}])).unwrap();
            let error = input.validate().unwrap_err();
            assert_eq!(error.error_code(), Some("mutually_exclusive_parameters"));
            assert_eq!(error.error_param(), Some("input[0].content[0]"));
            assert_eq!(
                error.to_string(),
                format!(
                    "Mutually exclusive parameters: 'input[0].content[0]'. Ensure you are only providing one of: \
                     'file_id' or '{field}'."
                )
            );
        }
    }

    #[test]
    fn inline_text_files_pass_in_new_input_and_restored_history() {
        let file = json!({"type":"input_file", "file_data":"data:text/plain;base64,aGk=", "filename":"a.txt"});
        let input: ResponsesInput = serde_json::from_value(json!([
            {"role":"user", "content":[file.clone()]},
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"ok"}]},
            {"role":"user", "content":[{"type":"input_text", "text":"again"}, file]}
        ]))
        .unwrap();
        input.validate().unwrap();
    }

    #[test]
    fn output_messages_are_validated_without_restricting_tool_results() {
        let mut message = OutputMessage::new("msg_1", MessageStatus::Completed);
        let input = ResponsesInput::from(vec![InOutItem::Output(OutputItem::Message(message.clone()))]);
        let error = input.validate().unwrap_err();
        assert!(matches!(error.source, MessageContentError::Empty));
        assert_eq!(
            error.to_string(),
            "input[0].content: a message must contain at least one content part"
        );

        message.content.push(OutputTextContent::new("answer").into());
        ResponsesInput::from(vec![InOutItem::Output(OutputItem::Message(message))])
            .validate()
            .unwrap();
        let tool_result: ResponsesInput = serde_json::from_value(json!([
            {"type":"function_call_output", "call_id":"call_1", "output":[
                {"type":"input_file", "file_id":"file_1"}
            ]}
        ]))
        .unwrap();
        tool_result.validate().unwrap();
    }
}
