//! Resolve `file_id` references from the Files API for one request.
//!
//! This is the access seam between request execution and the gateway file store, next to
//! [`ConversationHandler`](super::ConversationHandler) and [`ResponseHandler`](super::ResponseHandler).
//! It owns no state: it reads the executor's one file service. One [`FileHandler::resolve`]
//! returns the lookup used by the model-facing lowering of the input it was given. Stored
//! history, checkpoints, session parents and agent tree snapshots keep the client's reference.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::tool::file_search::FileSearchService;
use crate::types::file_search::FileSearchError;
use crate::types::io::ResponsesInput;
use crate::types::io::input::{FileReference, ResolvedFile, ResolvedFiles, file_references};

/// Resolves the Files API content behind `file_id` references.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileHandler<'a> {
    service: Option<&'a FileSearchService>,
}

impl<'a> FileHandler<'a> {
    pub(crate) const fn new(service: Option<&'a FileSearchService>) -> Self {
        Self { service }
    }

    /// Read every referenced file once and return the content keyed by `file_id`.
    ///
    /// `input` is the input this request lowers, so the lookup holds exactly the references
    /// the model-facing request will read. A missing file is reported with `OpenAI`'s
    /// `Files [<id>...] were not found` envelope; the resolved encoded size is checked
    /// here, before inference. Each file kind is checked once, where its reference is
    /// lowered.
    ///
    /// # Errors
    /// Returns one not-found error listing every missing id, a request-too-large error, or a
    /// file store error. The lookup is never stored.
    pub(crate) async fn resolve(
        &self,
        input: &ResponsesInput,
        max_encoded_bytes: usize,
    ) -> ExecutorResult<ResolvedFiles> {
        let references = file_references(input);
        if references.is_empty() {
            return Ok(HashMap::new());
        }
        let Some(service) = self.service else {
            return Err(no_store_error(&references[0]));
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
            // The size of the part this reference lowers to: an image data URL or the
            // JSON-escaped text value. A file the part will reject counts at its raw size;
            // lowering rejects it before inference.
            let reference_bytes = file
                .data_url_len()
                .or_else(|| file.text_json_len())
                .unwrap_or(file.bytes.len());
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

fn no_store_error(reference: &FileReference) -> ExecutorError {
    ExecutorError::InvalidRequest(format!(
        "{}: {} with file_id requires the gateway file store; configure file storage or send the content inline",
        reference.location, reference.part
    ))
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

    fn input(value: serde_json::Value) -> ResponsesInput {
        serde_json::from_value(value).expect("input items")
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
        let input = input(json!([
            {"role": "user", "content": [
                {"type": "input_image", "file_id": &image.id, "detail": "high"},
                {"type": "input_file", "file_id": &text.id}
            ]},
            {"type": "function_call_output", "call_id": "call_1", "output": [
                {"type": "input_file", "file_id": &text.id},
                {"type": "input_image", "file_id": &image.id}
            ]}
        ]));

        let files = FileHandler::new(Some(&service))
            .resolve(&input, usize::MAX)
            .await
            .unwrap();
        assert_eq!(files.len(), 2, "a repeated reference is read once");
        let resolved_text = files.get(&text.id).expect("text content");
        assert_eq!(resolved_text.filename, "notes.txt");
        assert_eq!(resolved_text.bytes, b"hello");
        assert!(
            files
                .get(&image.id)
                .expect("image content")
                .image_media_type()
                .is_some()
        );
    }

    #[tokio::test]
    async fn missing_references_use_openai_envelope_and_list_every_id() {
        let (service, _files) = file_search().await;
        let first = format!("file-{}", uuid::Uuid::now_v7());
        let second = format!("file-{}", uuid::Uuid::now_v7());
        let input = input(json!([
            {"role": "user", "content": [
                {"type": "input_text", "text": "a"},
                {"type": "input_file", "file_id": first},
                {"type": "input_file", "file_id": second}
            ]}
        ]));
        let error = FileHandler::new(Some(&service))
            .resolve(&input, usize::MAX)
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), http::StatusCode::NOT_FOUND);
        assert_eq!(error.error_type(), "invalid_request_error");
        assert_eq!(error.error_code(), "invalid_request_error");
        assert_eq!(error.error_param(), None);
        assert_eq!(error.to_string(), format!("Files [{first}, {second}] were not found"));
    }

    #[tokio::test]
    async fn references_resolve_without_checking_their_kind() {
        let (service, _files) = file_search().await;
        let text = service
            .upload_file("notes.txt", "text/plain", "assistants", b"hello".to_vec())
            .await
            .unwrap();
        let image = service
            .upload_file("colors.png", "image/png", "vision", b"png".to_vec())
            .await
            .unwrap();

        // The part kind is checked once, where each reference is lowered, not here.
        for (part, file_id) in [("input_image", &text.id), ("input_file", &image.id)] {
            let input = input(json!([{"role": "user", "content": [{"type": part, "file_id": file_id}]}]));
            let files = FileHandler::new(Some(&service))
                .resolve(&input, usize::MAX)
                .await
                .unwrap();
            assert_eq!(files.len(), 1);
        }
    }

    #[tokio::test]
    async fn references_require_a_configured_file_store() {
        for (kind, part) in [("input_image", "input_image"), ("input_file", "input_file")] {
            let input = input(json!([{"role": "user", "content": [{"type": kind, "file_id": "file-1"}]}]));
            let error = FileHandler::new(None).resolve(&input, usize::MAX).await.unwrap_err();
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
                .resolve(&ResponsesInput::default(), usize::MAX)
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
        let input = input(json!([
            {"role": "user", "content": [
                {"type": "input_file", "file_id": &text.id},
                {"type": "input_file", "file_id": &text.id}
            ]}
        ]));
        let error = FileHandler::new(Some(&service)).resolve(&input, 9).await.unwrap_err();
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
        let input = input(json!([
            {"role": "user", "content": [{"type": "input_file", "file_id": &text.id}]}
        ]));

        let resolved = FileHandler::new(Some(&service)).resolve(&input, escaped_bytes).await;
        assert!(resolved.is_ok(), "the exact escaped size fits the limit");

        let error = FileHandler::new(Some(&service))
            .resolve(&input, raw_bytes)
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
        let retained: Vec<crate::types::io::InputItem> = serde_json::from_value(json!([
            {"role": "user", "content": [{"type": "input_file", "file_id": &text.id}]}
        ]))
        .unwrap();
        let compaction = CompactionItem::carrying(None, "summary".into(), retained).unwrap();
        let input = ResponsesInput::from(vec![crate::types::io::InputItem::Compaction(compaction)]);

        let files = FileHandler::new(Some(&service))
            .resolve(&input, usize::MAX)
            .await
            .unwrap();
        assert!(files.contains_key(&text.id));
    }
}
