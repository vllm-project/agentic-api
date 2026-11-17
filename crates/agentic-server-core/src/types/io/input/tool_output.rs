//! Model-facing lowering of structured tool-output `file_id` references.

use super::{InputValidationError, ResolvedFiles, ToolCallOutput, ToolOutputContent};

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
