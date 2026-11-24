//! Resolve `file_id` references from the Files API for one request.
//!
//! This is the access seam between request execution and the gateway file store, next to
//! [`ConversationHandler`](super::ConversationHandler) and [`ResponseHandler`](super::ResponseHandler).
//! It owns the executor's one file service. One [`FileHandler::resolve`] returns the lookup used by
//! the model-facing lowering of the input it was given. Stored history, checkpoints, session
//! parents and agent tree snapshots keep the client's reference.

use std::collections::{HashMap, HashSet};

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::tool::file_search::FileSearchService;
use crate::types::file_search::{FileObject, FileSearchError};
use crate::types::io::ResponsesInput;
use crate::types::io::input::{FileReference, ResolvedFiles, file_references};

/// Reads the Files API content behind `file_id` references.
#[derive(Debug, Clone, Default)]
pub struct FileHandler {
    service: Option<FileSearchService>,
}

impl FileHandler {
    #[must_use]
    pub fn new(service: FileSearchService) -> Self {
        Self { service: Some(service) }
    }

    /// The executor's one file service, when file storage is configured.
    #[must_use]
    pub fn service(&self) -> Option<&FileSearchService> {
        self.service.as_ref()
    }

    /// Read every referenced file once and return the content keyed by `file_id`.
    ///
    /// `input` is the input this request lowers, so the lookup holds exactly the references
    /// the model-facing request will read. A missing file is reported with `OpenAI`'s
    /// `Files [<id>...] were not found` envelope. Each file kind is checked once, where its
    /// reference is lowered, and the resolved body is size-checked after serialization.
    ///
    /// # Errors
    /// Returns one not-found error listing every missing id or a file store error. The
    /// lookup is never stored.
    pub(crate) async fn resolve(&self, input: &ResponsesInput) -> ExecutorResult<ResolvedFiles> {
        let references = file_references(input);
        if references.is_empty() {
            return Ok(HashMap::new());
        }
        let Some(service) = self.service.as_ref() else {
            return Err(no_store_error(&references[0]));
        };

        let mut files = ResolvedFiles::new();
        let mut missing: Vec<String> = Vec::new();
        let mut read: HashSet<&str> = HashSet::new();
        for reference in &references {
            if !read.insert(reference.file_id.as_str()) {
                continue;
            }
            match read_file(service, &reference.file_id).await {
                Ok(file) => {
                    files.insert(reference.file_id.clone(), file);
                }
                Err(FileSearchError::FileNotFound { .. }) => {
                    missing.push(reference.file_id.clone());
                }
                Err(error) => return Err(error.into()),
            }
        }
        if !missing.is_empty() {
            return Err(ExecutorError::FileReferencesNotFound { ids: missing });
        }
        Ok(files)
    }
}

async fn read_file(service: &FileSearchService, file_id: &str) -> Result<(FileObject, Vec<u8>), FileSearchError> {
    let object = service.get_file(file_id).await?;
    let bytes = service.file_content(file_id).await?;
    Ok((object, bytes))
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
    use std::sync::Arc;

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

        let files = FileHandler::new(service).resolve(&input).await.unwrap();
        assert_eq!(files.len(), 2, "a repeated reference is read once");
        let (object, bytes) = files.get(&text.id).expect("text content");
        assert_eq!(object.filename, "notes.txt");
        assert_eq!(bytes, b"hello");
        assert_eq!(
            files.get(&image.id).map(|(object, _)| object.filename.as_str()),
            Some("plot.png")
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
        let error = FileHandler::new(service).resolve(&input).await.unwrap_err();
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
            let files = FileHandler::new(service.clone()).resolve(&input).await.unwrap();
            assert_eq!(files.len(), 1);
        }
    }

    #[tokio::test]
    async fn references_require_a_configured_file_store() {
        for (kind, part) in [("input_image", "input_image"), ("input_file", "input_file")] {
            let input = input(json!([{"role": "user", "content": [{"type": kind, "file_id": "file-1"}]}]));
            let error = FileHandler::default().resolve(&input).await.unwrap_err();
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
            FileHandler::default()
                .resolve(&ResponsesInput::default())
                .await
                .unwrap()
                .is_empty()
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

        let files = FileHandler::new(service).resolve(&input).await.unwrap();
        assert!(files.contains_key(&text.id));
    }
}
