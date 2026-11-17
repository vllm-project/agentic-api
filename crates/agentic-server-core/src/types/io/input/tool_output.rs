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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::file_search::FileObject;
    use crate::types::io::input::InputFileError;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
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
    fn text_references_become_input_text_at_the_output_position() {
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
    fn image_references_become_data_urls_and_inline_parts_pass_through() {
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
    fn wrong_kind_references_use_the_recorded_envelope_without_a_position() {
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
    fn content_and_lookup_failures_keep_the_output_position() {
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
