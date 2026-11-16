//! Which input items carry a `file_id` the gateway file store must resolve.
//!
//! Resolution itself belongs to `executor/modes/files.rs`; this module only classifies the
//! request so the executor and split execution can decide whether that seam is needed.

use super::{InputContent, InputItem, InputMessageContent, ResponsesInput, ToolCallOutput, ToolOutputContent};

impl ResponsesInput {
    /// Whether the input carries a `file_id` reference the gateway file store must resolve.
    #[must_use]
    pub(crate) fn has_file_references(&self) -> bool {
        self.input_items().any(|item| match item.as_ref() {
            InputItem::Message(message) => match &message.content {
                InputMessageContent::Parts(parts) => parts.iter().any(part_has_file_reference),
                InputMessageContent::Text(_) => false,
            },
            InputItem::FunctionCallOutput(output) => tool_output_has_file_reference(&output.output),
            InputItem::CustomToolCallOutput(output) => tool_output_has_file_reference(&output.output),
            _ => false,
        })
    }
}

fn part_has_file_reference(part: &InputContent) -> bool {
    match part {
        InputContent::InputFile(file) => file.file_id.is_some(),
        InputContent::InputImage(image) => image.file_id.is_some(),
        _ => false,
    }
}

fn tool_output_has_file_reference(output: &ToolCallOutput) -> bool {
    let ToolCallOutput::Content(parts) = output else {
        return false;
    };
    parts.iter().any(|part| match part {
        ToolOutputContent::InputFile(file) => file.file_id.is_some(),
        ToolOutputContent::InputImage(image) => image.file_id.is_some(),
        ToolOutputContent::InputText(_) => false,
    })
}
