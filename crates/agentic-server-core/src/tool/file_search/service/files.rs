//! File upload, retrieval, and deletion operations.

use super::{
    Arc, CancellationToken, Collection, DeleteObject, FileObject, FilePublicationFailure, FileSearchError,
    FileSearchService, ListParams, ListResponse, MAX_FILE_BYTES, OwnedSemaphorePermit, decode_content, deleted,
    invalid, page, size_i64, validate_file_id, validate_list,
};
use crate::storage::file_search::StoredFileContent;
use std::time::Duration;
use tokio::time::Instant;

const UPLOAD_LEASE_SECONDS: i64 = 60;
const UPLOAD_RENEW_INTERVAL: Duration = Duration::from_secs(20);

fn lease_deadline() -> i64 {
    chrono::Utc::now().timestamp() + UPLOAD_LEASE_SECONDS
}

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
            let owner_token = uuid::Uuid::now_v7().to_string();
            storage
                .stage_blob_cleanup(&file.id, &owner_token, lease_deadline())
                .await?;
            // Keep the lease live while a pluggable blob provider publishes.
            // If another instance claims an expired lease, SQL publication is
            // fenced by this token and cannot expose missing content.
            let published = {
                let mut publish = Box::pin(files.publish(&file.id, &bytes, &cancelled));
                let mut renewal = tokio::time::interval_at(
                    Instant::now() + UPLOAD_RENEW_INTERVAL,
                    UPLOAD_RENEW_INTERVAL,
                );
                loop {
                    tokio::select! {
                        result = &mut publish => break result,
                        _ = renewal.tick() => {
                            match storage.refresh_blob_cleanup(&file.id, &owner_token, lease_deadline()).await {
                                Ok(true) => {},
                                Ok(false) => break Err(FileSearchError::Unavailable("Upload ownership expired; retry the upload".into())),
                                Err(error) => break Err(error),
                            }
                        }
                    }
                }
            };
            if let Err(error) = published {
                files.remove(&file.id).await?;
                storage.complete_blob_cleanup(&file.id, &owner_token).await?;
                return Err(error);
            }
            match storage
                .upload_file_metadata(&file, &content_type, &owner_token, &cancelled)
                .await
            {
                Ok(()) => Ok(file),
                Err(FilePublicationFailure::SafeToRemove(error)) => {
                    files.remove(&file.id).await?;
                    storage.complete_blob_cleanup(&file.id, &owner_token).await?;
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
            let cleanup_token = match storage.file(&id).await {
                Ok(file) => {
                    storage.delete(Collection::Files, &id, None).await?;
                    if matches!(file.content, StoredFileContent::Blob) {
                        Some(storage.pending_blob_cleanup(&id).await?.ok_or_else(|| {
                            FileSearchError::Unavailable("File deletion cleanup marker is missing".into())
                        })?)
                    } else {
                        None
                    }
                }
                Err(FileSearchError::NotFound(_)) => storage
                    .pending_blob_cleanup(&id)
                    .await?
                    .ok_or_else(|| FileSearchError::NotFound("File not found".into()))
                    .map(Some)?,
                Err(error) => return Err(error),
            };
            if let Some(cleanup_token) = cleanup_token {
                files.remove(&id).await?;
                storage.complete_blob_cleanup(&id, &cleanup_token).await?;
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
            let now = chrono::Utc::now().timestamp();
            let batch = self.storage.blob_cleanup_batch(&after, 100, now).await?;
            if batch.is_empty() {
                return Ok(());
            }
            for id in &batch {
                let owner_token = uuid::Uuid::now_v7().to_string();
                if !self
                    .storage
                    .claim_blob_cleanup(id, &owner_token, now, lease_deadline())
                    .await?
                {
                    continue;
                }
                match self.storage.file_object(id).await {
                    Ok(_) => {}
                    Err(FileSearchError::NotFound(_)) => self.files.remove(id).await?,
                    Err(error) => return Err(error),
                }
                self.storage.complete_blob_cleanup(id, &owner_token).await?;
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
