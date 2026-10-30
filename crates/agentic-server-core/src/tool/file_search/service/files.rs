//! File upload, retrieval, and deletion operations.

use super::{
    Arc, CancellationToken, Collection, DeleteObject, FileObject, FilePublicationFailure, FileSearchError,
    FileSearchService, ListParams, ListResponse, MAX_FILE_BYTES, OwnedSemaphorePermit, decode_content, deleted,
    invalid, page, size_i64, validate_file_id, validate_list,
};
use crate::storage::file_search::StoredFileContent;

impl FileSearchService {
    /// Stores uploaded bytes locally and publishes their SQL metadata afterwards.
    ///
    /// # Errors
    /// Returns validation, resource-limit, or storage errors.
    pub async fn upload_file(
        &self,
        filename: &str,
        content_type: &str,
        purpose: &str,
        bytes: Vec<u8>,
    ) -> Result<FileObject, FileSearchError> {
        let permit = self.permit()?;
        if filename.is_empty() || filename.len() > 255 || filename.chars().any(char::is_control) {
            return invalid("filename must contain 1 to 255 bytes without control characters");
        }
        if !matches!(purpose, "assistants" | "user_data") {
            return invalid("file search accepts purpose assistants or user_data");
        }
        if bytes.is_empty() || bytes.len() > MAX_FILE_BYTES {
            return invalid("file must contain 1 byte to 20 MiB");
        }
        if content_type.is_empty() || content_type.len() > 256 || content_type.chars().any(char::is_control) {
            return invalid("content_type must contain 1 to 256 bytes without control characters");
        }
        let file = FileObject {
            id: format!("file-{}", uuid::Uuid::now_v7()),
            object: "file".into(),
            bytes: size_i64(bytes.len())?,
            created_at: chrono::Utc::now().timestamp(),
            filename: filename.into(),
            purpose: purpose.into(),
            status: "processed".into(),
        };
        let storage = self.storage.clone();
        let files = self.files.clone();
        let content_type = content_type.to_owned();
        let cancelled = CancellationToken::new();
        let _cancel_on_drop = cancelled.clone().drop_guard();
        // The caller joins this operation normally. On caller cancellation it
        // retains its permit and finishes rollback/cleanup without abandoning I/O.
        tokio::spawn(async move {
            let _permit = permit;
            storage.stage_blob_cleanup(&file.id).await?;
            if let Err(error) = files.publish(&file.id, &bytes, &cancelled).await {
                files.remove(&file.id).await?;
                storage.complete_blob_cleanup(&file.id).await?;
                return Err(error);
            }
            match storage.upload_file_metadata(&file, &content_type, &cancelled).await {
                Ok(()) => {
                    if let Err(error) = storage.complete_blob_cleanup(&file.id).await {
                        tracing::warn!(file_id = %file.id, %error, "uploaded file is live; cleanup marker remains");
                    }
                    Ok(file)
                }
                Err(FilePublicationFailure::SafeToRemove(error)) => {
                    files.remove(&file.id).await?;
                    storage.complete_blob_cleanup(&file.id).await?;
                    Err(error)
                }
                Err(FilePublicationFailure::Indeterminate(error)) => Err(error),
            }
        })
        .await?
    }

    /// # Errors
    /// Returns pagination validation or storage errors.
    pub async fn list_files(&self, params: &ListParams) -> Result<ListResponse<FileObject>, FileSearchError> {
        validate_list(params)?;
        Ok(page(
            self.storage.list_files(params).await?,
            params,
            |file: &FileObject| &file.id,
        ))
    }

    /// # Errors
    /// Returns not-found or storage errors.
    pub async fn get_file(&self, id: &str) -> Result<FileObject, FileSearchError> {
        validate_file_id(id)?;
        self.storage.file_object(id).await
    }

    /// # Errors
    /// Returns not-found, resource-limit, or storage errors.
    pub async fn file_content(&self, id: &str) -> Result<Vec<u8>, FileSearchError> {
        let permit = self.permit()?;
        validate_file_id(id)?;
        let file = self.storage.file(id).await?;
        self.read_content(id, file.object.bytes, file.content, permit).await
    }

    /// Atomically deletes metadata, attachments, and chunks, then removes the local blob.
    ///
    /// # Errors
    /// Returns not-found or storage errors.
    pub async fn delete_file(&self, id: &str) -> Result<DeleteObject, FileSearchError> {
        validate_file_id(id)?;
        let permit = self.permit()?;
        let storage = self.storage.clone();
        let files = self.files.clone();
        let id = id.to_owned();
        // Once deletion starts, finish SQL cascading deletion and blob cleanup
        // even if the caller disconnects. The repository records a durable
        // cleanup marker in the same transaction as metadata deletion.
        tokio::spawn(async move {
            let _permit = permit;
            let blob = match storage.file(&id).await {
                Ok(file) => {
                    storage.delete(Collection::Files, &id, None).await?;
                    matches!(file.content, StoredFileContent::Blob)
                }
                Err(FileSearchError::NotFound(_)) if storage.pending_blob_cleanup(&id).await? => true,
                Err(error) => return Err(error),
            };
            if blob {
                files.remove(&id).await?;
                storage.complete_blob_cleanup(&id).await?;
            }
            Ok(deleted(&id, "file.deleted"))
        })
        .await?
    }

    /// Reconciles durable blob intents after a restart. A live metadata row
    /// owns its blob; otherwise the orphaned bytes are removed idempotently.
    ///
    /// # Errors
    /// Returns storage or blob removal errors, leaving the marker for retry.
    pub async fn reconcile_blob_cleanup(&self) -> Result<(), FileSearchError> {
        let mut after = String::new();
        loop {
            let batch = self.storage.blob_cleanup_batch(&after, 100).await?;
            if batch.is_empty() {
                return Ok(());
            }
            for id in &batch {
                match self.storage.file_object(id).await {
                    Ok(_) => {}
                    Err(FileSearchError::NotFound(_)) => self.files.remove(id).await?,
                    Err(error) => return Err(error),
                }
                self.storage.complete_blob_cleanup(id).await?;
            }
            after = batch.last().cloned().unwrap_or_default();
        }
    }

    pub(super) async fn read_content(
        &self,
        id: &str,
        expected_bytes: i64,
        content: StoredFileContent,
        permit: Arc<OwnedSemaphorePermit>,
    ) -> Result<Vec<u8>, FileSearchError> {
        match content {
            StoredFileContent::Blob => {
                let files = self.files.clone();
                let id = id.to_owned();
                tokio::spawn(async move {
                    let _permit = permit;
                    files.read(&id, expected_bytes, MAX_FILE_BYTES).await
                })
                .await?
            }
            StoredFileContent::LegacyInlineBase64(legacy) => {
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    decode_content(&legacy)
                })
                .await?
            }
        }
    }
}
