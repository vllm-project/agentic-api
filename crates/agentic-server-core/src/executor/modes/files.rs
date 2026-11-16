//! Resolve `file_id` references from the Files API for one request.
//!
//! This is the access seam between request execution and the gateway file store, next to
//! [`ConversationHandler`](super::ConversationHandler) and [`ResponseHandler`](super::ResponseHandler).
//! It owns the service and only reads: one [`FileHandler::resolve`] returns the per-turn lookup
//! used by the model-facing lowering. Stored history, checkpoints, session parents and agent tree
//! snapshots keep the client's reference.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::tool::file_search::FileSearchService;
use crate::types::file_search::FileSearchError;
use crate::types::io::input::{
    ResolvedFile, ResolvedFiles, context_stuffing_extensions, expand_carried_windows, extension_label, is_text_filename,
};
use crate::types::io::{InputContent, InputItem, InputMessageContent, ToolCallOutput, ToolOutputContent};

/// Which content kind carried a `file_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferenceKind {
    File,
    Image,
}

/// One `file_id` occurrence.
#[derive(Debug, Clone)]
struct Reference {
    file_id: String,
    kind: ReferenceKind,
    /// The request position, when the item came from the request's input rather than an agent history.
    location: Option<String>,
}

/// Resolves the Files API content behind `file_id` references.
#[derive(Debug, Clone, Default)]
pub(crate) struct FileHandler {
    service: Option<FileSearchService>,
}

impl FileHandler {
    pub(crate) const fn new(service: Option<FileSearchService>) -> Self {
        Self { service }
    }

    /// Read every referenced file once and return the content keyed by `file_id`.
    ///
    /// `items` is the final input plus every restored agent history, so a multi-agent run
    /// resolves each reference before any agent's round lowers it. A missing file is reported
    /// with `OpenAI`'s `Files [<id>...] were not found` envelope; wrong file types and the
    /// resolved encoded size are checked here, before inference.
    ///
    /// # Errors
    /// Returns one not-found error listing every missing id, a wrong-file-type error, a
    /// request-too-large error, or a file store error. The lookup is never stored.
    pub(crate) async fn resolve<'a>(
        &self,
        items: impl IntoIterator<Item = Cow<'a, InputItem>>,
        max_encoded_bytes: usize,
    ) -> ExecutorResult<ResolvedFiles> {
        let references = references(items);
        if references.is_empty() {
            return Ok(HashMap::new());
        }
        let Some(service) = &self.service else {
            return Err(no_store_error(&references));
        };

        let mut files: ResolvedFiles = HashMap::new();
        let mut missing: Vec<String> = Vec::new();
        let mut read: HashSet<&str> = HashSet::new();
        for reference in &references {
            if !read.insert(reference.file_id.as_str()) {
                continue;
            }
            match read_file(service, &reference.file_id).await {
                Ok(file) => {
                    files.insert(reference.file_id.clone(), Arc::new(file));
                }
                Err(ReferenceRead::Missing) => missing.push(reference.file_id.clone()),
                Err(ReferenceRead::Failed(error)) => return Err(*error),
            }
        }
        if !missing.is_empty() {
            return Err(ExecutorError::FileReferencesNotFound { ids: missing });
        }

        let mut encoded_bytes = 0usize;
        for reference in &references {
            let file = files
                .get(&reference.file_id)
                .expect("every distinct referenced file was read or reported missing");
            let reference_bytes = match reference.kind {
                ReferenceKind::Image => {
                    if !file.is_image() {
                        return Err(image_type_error(&file.filename));
                    }
                    file.data_url_len().ok_or_else(|| image_type_error(&file.filename))?
                }
                ReferenceKind::File => {
                    if !is_text_filename(&file.filename) {
                        return Err(file_type_error(&file.filename));
                    }
                    // Exact serialized JSON-escaped size; a non-UTF-8 file is rejected when
                    // its model-facing text part is built, so the raw count is only a fallback.
                    file.text_json_len().unwrap_or(file.bytes.len())
                }
            };
            encoded_bytes = encoded_bytes.saturating_add(reference_bytes);
        }
        if encoded_bytes > max_encoded_bytes {
            return Err(ExecutorError::PayloadTooLarge(format!(
                "resolved file_id references exceed the {max_encoded_bytes}-byte request body limit"
            )));
        }
        Ok(files)
    }
}

/// Collect every message and structured-tool-output `file_id`, expanding carried compaction
/// windows so a restored window's retained messages are resolved too.
fn references<'a>(items: impl IntoIterator<Item = Cow<'a, InputItem>>) -> Vec<Reference> {
    let items = expand_carried_windows(items);
    let mut references = Vec::new();
    for (item_index, item) in items.iter().enumerate() {
        match item.as_ref() {
            InputItem::Message(message) => {
                let InputMessageContent::Parts(parts) = &message.content else {
                    continue;
                };
                for (part_index, part) in parts.iter().enumerate() {
                    let location = Some(content_location(item_index, part_index));
                    match part {
                        InputContent::InputFile(file) => {
                            if let Some(file_id) = file.file_id.as_deref() {
                                references.push(Reference {
                                    file_id: file_id.to_owned(),
                                    kind: ReferenceKind::File,
                                    location,
                                });
                            }
                        }
                        InputContent::InputImage(image) => {
                            if let Some(file_id) = image.file_id.as_deref() {
                                references.push(Reference {
                                    file_id: file_id.to_owned(),
                                    kind: ReferenceKind::Image,
                                    location,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            InputItem::FunctionCallOutput(output) => collect_tool_output(&output.output, item_index, &mut references),
            InputItem::CustomToolCallOutput(output) => {
                collect_tool_output(&output.output, item_index, &mut references);
            }
            _ => {}
        }
    }
    references
}

fn collect_tool_output(output: &ToolCallOutput, item_index: usize, references: &mut Vec<Reference>) {
    let ToolCallOutput::Content(parts) = output else {
        return;
    };
    for (part_index, part) in parts.iter().enumerate() {
        let location = Some(output_location(item_index, part_index));
        match part {
            ToolOutputContent::InputFile(file) => {
                if let Some(file_id) = file.file_id.as_deref() {
                    references.push(Reference {
                        file_id: file_id.to_owned(),
                        kind: ReferenceKind::File,
                        location,
                    });
                }
            }
            ToolOutputContent::InputImage(image) => {
                if let Some(file_id) = image.file_id.as_deref() {
                    references.push(Reference {
                        file_id: file_id.to_owned(),
                        kind: ReferenceKind::Image,
                        location,
                    });
                }
            }
            ToolOutputContent::InputText(_) => {}
        }
    }
}

fn content_location(item_index: usize, part_index: usize) -> String {
    format!("input[{item_index}].content[{part_index}]")
}

fn output_location(item_index: usize, part_index: usize) -> String {
    format!("input[{item_index}].output[{part_index}]")
}

enum ReferenceRead {
    Missing,
    Failed(Box<ExecutorError>),
}

async fn read_file(service: &FileSearchService, file_id: &str) -> Result<ResolvedFile, ReferenceRead> {
    let object = service.get_file(file_id).await.map_err(read_error)?;
    let bytes = service.file_content(file_id).await.map_err(read_error)?;
    Ok(ResolvedFile::new(object.filename, bytes))
}

fn read_error(error: FileSearchError) -> ReferenceRead {
    match error {
        FileSearchError::NotFound(_) => ReferenceRead::Missing,
        error => ReferenceRead::Failed(Box::new(error.into())),
    }
}

fn no_store_error(references: &[Reference]) -> ExecutorError {
    let reference = &references[0];
    let part = match reference.kind {
        ReferenceKind::File => "input_file",
        ReferenceKind::Image => "input_image",
    };
    let location = reference
        .location
        .as_deref()
        .map(|location| format!("{location}: "))
        .unwrap_or_default();
    ExecutorError::InvalidRequest(format!(
        "{location}{part} with file_id requires the gateway file store; configure file storage or send the content \
         inline"
    ))
}

fn image_type_error(filename: &str) -> ExecutorError {
    ExecutorError::UnsupportedValue {
        param: Some("input".into()),
        code: None,
        message: format!(
            "Invalid input: Expected image type to be a supported format: .jpeg, .jpg, .png, .gif, .webp but got {}.",
            extension_label(filename)
        ),
    }
}

fn file_type_error(filename: &str) -> ExecutorError {
    ExecutorError::UnsupportedValue {
        param: Some("input".into()),
        code: None,
        message: format!(
            "Invalid input: Expected context stuffing file type to be a supported format: {} but got {}.",
            context_stuffing_extensions(),
            extension_label(filename)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::create_pool_with_schema;
    use crate::types::file_search::FileSearchConfig;
    use serde_json::json;

    async fn file_search() -> (FileSearchService, tempfile::TempDir) {
        let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
        let files = tempfile::tempdir().unwrap();
        let config = FileSearchConfig {
            files_storage_dir: Some(files.path().to_owned()),
            ..FileSearchConfig::default()
        };
        let service =
            FileSearchService::new(pool, Arc::new(reqwest::Client::new()), config).expect("file search service");
        (service, files)
    }

    fn input(value: serde_json::Value) -> Vec<InputItem> {
        serde_json::from_value(value).expect("input items")
    }

    fn owned(items: &[InputItem]) -> Vec<Cow<'_, InputItem>> {
        items.iter().map(Cow::Borrowed).collect()
    }

    fn message(error: ExecutorError) -> String {
        match error {
            ExecutorError::InvalidRequest(message) => message,
            other => panic!("expected an invalid request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolves_message_and_tool_output_references_once() {
        let (service, _files) = file_search().await;
        let text = service
            .upload_file("notes.txt", "text/plain", "assistants", b"hello".to_vec())
            .await
            .unwrap();
        let image = service
            .upload_file("plot.png", "image/png", "vision", b"png bytes".to_vec())
            .await
            .unwrap();
        let items = input(json!([
            {"role": "user", "content": [
                {"type": "input_image", "file_id": &image.id, "detail": "high"},
                {"type": "input_file", "file_id": &text.id}
            ]},
            {"type": "function_call_output", "call_id": "call_1", "output": [
                {"type": "input_file", "file_id": &text.id},
                {"type": "input_image", "file_id": &image.id}
            ]}
        ]));

        let files = FileHandler::new(Some(service))
            .resolve(owned(&items), usize::MAX)
            .await
            .unwrap();
        assert_eq!(files.len(), 2, "a repeated reference is read once");
        let resolved_text = files.get(&text.id).expect("text content");
        assert_eq!(resolved_text.filename, "notes.txt");
        assert_eq!(resolved_text.bytes, b"hello");
        assert!(files.get(&image.id).expect("image content").is_image());
    }

    #[tokio::test]
    async fn missing_references_use_openai_envelope_and_list_every_id() {
        let (service, _files) = file_search().await;
        let first = format!("file-{}", uuid::Uuid::now_v7());
        let second = format!("file-{}", uuid::Uuid::now_v7());
        let items = input(json!([
            {"role": "user", "content": [
                {"type": "input_text", "text": "a"},
                {"type": "input_file", "file_id": first},
                {"type": "input_file", "file_id": second}
            ]}
        ]));
        let error = FileHandler::new(Some(service))
            .resolve(owned(&items), usize::MAX)
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), http::StatusCode::NOT_FOUND);
        assert_eq!(error.error_type(), "invalid_request_error");
        assert_eq!(error.error_code(), "invalid_request_error");
        assert_eq!(error.error_param(), None);
        assert_eq!(error.to_string(), format!("Files [{first}, {second}] were not found"));
    }

    #[tokio::test]
    async fn wrong_file_types_use_openai_envelope() {
        let (service, _files) = file_search().await;
        let text = service
            .upload_file("notes.txt", "text/plain", "assistants", b"hello".to_vec())
            .await
            .unwrap();
        let image = service
            .upload_file("colors.png", "image/png", "vision", b"png".to_vec())
            .await
            .unwrap();

        let as_image = input(json!([
            {"role": "user", "content": [{"type": "input_image", "file_id": &text.id}]}
        ]));
        let error = FileHandler::new(Some(service.clone()))
            .resolve(owned(&as_image), usize::MAX)
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(error.error_type(), "invalid_request_error");
        assert_eq!(error.error_param(), Some("input"));
        assert_eq!(
            error.to_string(),
            "Invalid input: Expected image type to be a supported format: .jpeg, .jpg, .png, .gif, .webp but got \
             .txt."
        );

        let as_file = input(json!([
            {"role": "user", "content": [{"type": "input_file", "file_id": &image.id}]}
        ]));
        let error = FileHandler::new(Some(service))
            .resolve(owned(&as_file), usize::MAX)
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), http::StatusCode::BAD_REQUEST);
        assert!(
            error
                .to_string()
                .starts_with("Invalid input: Expected context stuffing file type to be a supported format: .")
        );
        assert!(error.to_string().ends_with("but got .png."), "{error}");
    }

    #[tokio::test]
    async fn references_require_a_configured_file_store() {
        for (kind, part) in [("input_image", "input_image"), ("input_file", "input_file")] {
            let items = input(json!([{"role": "user", "content": [{"type": kind, "file_id": "file-1"}]}]));
            let error = FileHandler::new(None)
                .resolve(owned(&items), usize::MAX)
                .await
                .unwrap_err();
            assert_eq!(error.http_status(), http::StatusCode::BAD_REQUEST);
            assert_eq!(
                message(error),
                format!(
                    "input[0].content[0]: {part} with file_id requires the gateway file store; configure file \
                     storage or send the content inline"
                )
            );
        }
        assert!(
            FileHandler::new(None)
                .resolve(Vec::new(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn resolved_content_counts_against_the_request_body_limit() {
        let (service, _files) = file_search().await;
        let text = service
            .upload_file("notes.txt", "text/plain", "assistants", b"hello".to_vec())
            .await
            .unwrap();
        let items = input(json!([
            {"role": "user", "content": [
                {"type": "input_file", "file_id": &text.id},
                {"type": "input_file", "file_id": &text.id}
            ]}
        ]));
        let error = FileHandler::new(Some(service))
            .resolve(owned(&items), 9)
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutorError::PayloadTooLarge(_)), "{error}");
    }

    #[tokio::test]
    async fn text_references_count_their_json_escaped_size() {
        let (service, _files) = file_search().await;
        let bytes = b"\n\n\n\n\n\n\n\n".to_vec();
        let text = service
            .upload_file("notes.txt", "text/plain", "assistants", bytes.clone())
            .await
            .unwrap();
        let model_text = format!("File: notes.txt\n{}", String::from_utf8(bytes.clone()).unwrap());
        let raw_bytes = model_text.len();
        let escaped_bytes = serde_json::to_string(&model_text).unwrap().len() - 2;
        assert!(escaped_bytes > raw_bytes);
        let items = input(json!([
            {"role": "user", "content": [{"type": "input_file", "file_id": &text.id}]}
        ]));

        let resolved = FileHandler::new(Some(service.clone()))
            .resolve(owned(&items), escaped_bytes)
            .await;
        assert!(resolved.is_ok(), "the exact escaped size fits the limit");

        let error = FileHandler::new(Some(service))
            .resolve(owned(&items), raw_bytes)
            .await
            .unwrap_err();
        assert!(
            matches!(error, ExecutorError::PayloadTooLarge(_)),
            "raw stored bytes fit, but the serialized size does not: {error}"
        );
    }

    #[tokio::test]
    async fn restored_compaction_windows_are_resolved() {
        use crate::types::io::CompactionItem;
        let (service, _files) = file_search().await;
        let text = service
            .upload_file("notes.txt", "text/plain", "assistants", b"hello".to_vec())
            .await
            .unwrap();
        let retained = input(json!([
            {"role": "user", "content": [{"type": "input_file", "file_id": &text.id}]}
        ]));
        let compaction = CompactionItem::carrying(None, "summary".into(), retained).unwrap();
        let items = vec![InputItem::Compaction(compaction)];

        let files = FileHandler::new(Some(service))
            .resolve(owned(&items), usize::MAX)
            .await
            .unwrap();
        assert!(files.contains_key(&text.id));
    }
}
