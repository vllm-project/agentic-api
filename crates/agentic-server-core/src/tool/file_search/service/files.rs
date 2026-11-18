//! File upload, retrieval, and deletion operations.

use super::{
    Arc, CancellationToken, Collection, DeleteObject, FileObject, FilePublicationFailure, FileSearchError,
    FileSearchService, ListParams, ListResponse, MAX_FILE_BYTES, OwnedSemaphorePermit, decode_content, deleted, page,
    size_i64, validate_file_id, validate_list,
};
use crate::storage::file_search::StoredFileContent;
use crate::types::file_search::FILE_PURPOSES;
use crate::types::io::input::{IMAGE_MEDIA_TYPES, is_image_media_type};
use std::time::Duration;
use tokio::time::Instant;

const UPLOAD_LEASE_SECONDS: i64 = 60;
const UPLOAD_RENEW_INTERVAL: Duration = Duration::from_secs(20);

/// A Files API request the recorded responses reject: `invalid_request_error`, `code: null`.
fn rejected(message: impl Into<String>, param: Option<&'static str>) -> FileSearchError {
    FileSearchError::FileRejected {
        message: message.into(),
        param,
    }
}

/// The recorded response reports any unusable ID as a missing file, whatever the lookup failed with.
fn file_lookup_error(error: FileSearchError, id: &str, param: &'static str) -> FileSearchError {
    match error {
        FileSearchError::NotFound(_) | FileSearchError::InvalidRequest(_) | FileSearchError::FileNotFound { .. } => {
            FileSearchError::FileNotFound {
                id: id.to_owned(),
                param,
            }
        }
        error => error,
    }
}

/// Applies the Files API upload rules in the order the recorded responses check them.
fn validate_upload(filename: &str, content_type: &str, purpose: &str, bytes: &[u8]) -> Result<(), FileSearchError> {
    if filename.is_empty() || filename.len() > 255 || filename.chars().any(char::is_control) {
        return Err(rejected(
            "filename must contain 1 to 255 bytes without control characters",
            None,
        ));
    }
    if !FILE_PURPOSES.contains(&purpose) {
        let accepted = FILE_PURPOSES.map(|purpose| format!("'{purpose}'")).join(", ");
        return Err(rejected(
            format!("'{purpose}' is not one of [{accepted}] - 'purpose'"),
            Some("purpose"),
        ));
    }
    let media_type = content_type
        .split_once(';')
        .map_or(content_type, |(media_type, _)| media_type)
        .trim();
    if purpose == "vision" && !is_image_media_type(media_type) {
        let supported = IMAGE_MEDIA_TYPES
            .map(|accepted| format!("\"{}\"", accepted.strip_prefix("image/").unwrap_or(accepted)))
            .join(", ");
        return Err(rejected(
            format!("Invalid file format {media_type}. Supported formats: {supported}"),
            None,
        ));
    }
    if bytes.is_empty() {
        return Err(rejected("File is empty.", None));
    }
    if bytes.len() > MAX_FILE_BYTES {
        return Err(rejected("file must contain 1 byte to 20 MiB", None));
    }
    if content_type.is_empty() || content_type.len() > 256 || content_type.chars().any(char::is_control) {
        return Err(rejected(
            "content_type must contain 1 to 256 bytes without control characters",
            None,
        ));
    }
    Ok(())
}

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
        validate_upload(filename, content_type, purpose, &bytes)?;
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
                let mut renewal_failure = None;
                let result = loop {
                    tokio::select! {
                        biased;
                        _ = renewal.tick(), if renewal_failure.is_none() => {
                            let failure = match storage.refresh_blob_cleanup(&file.id, &owner_token, lease_deadline()).await {
                                Ok(true) => None,
                                Ok(false) => Some(FileSearchError::Unavailable("Upload ownership expired; retry the upload".into())),
                                Err(error) => Some(error),
                            };
                            if let Some(error) = failure {
                                cancelled.cancel();
                                renewal_failure = Some(error);
                            }
                        }
                        result = &mut publish => break result,
                    }
                };
                // Cancellation is only a signal. Wait until the provider has
                // drained its in-flight filesystem/network work before remove.
                renewal_failure.map_or(result, Err)
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
    /// Returns pagination validation, missing cursor, or storage errors.
    pub async fn list_files(&self, params: &ListParams) -> Result<ListResponse<FileObject>, FileSearchError> {
        validate_list(params)?;
        for (cursor, param) in [(params.after.as_deref(), "after"), (params.before.as_deref(), "before")] {
            if let Some(cursor) = cursor {
                self.get_file(cursor)
                    .await
                    .map_err(|error| file_lookup_error(error, cursor, param))?;
            }
        }
        Ok(page(
            self.storage.list_files(params).await?,
            params,
            |file: &FileObject| &file.id,
        ))
    }

    /// # Errors
    /// Returns not-found or storage errors.
    pub async fn get_file(&self, id: &str) -> Result<FileObject, FileSearchError> {
        validate_file_id(id).map_err(|error| file_lookup_error(error, id, "id"))?;
        self.storage
            .file_object(id)
            .await
            .map_err(|error| file_lookup_error(error, id, "id"))
    }

    /// # Errors
    /// Returns not-found, resource-limit, or storage errors.
    pub async fn file_content(&self, id: &str) -> Result<Vec<u8>, FileSearchError> {
        let permit = self.permit()?;
        validate_file_id(id).map_err(|error| file_lookup_error(error, id, "id"))?;
        let file = self
            .storage
            .file(id)
            .await
            .map_err(|error| file_lookup_error(error, id, "id"))?;
        self.read_content(id, file.object.bytes, file.content, permit).await
    }

    /// Atomically deletes metadata, attachments, and chunks, then removes the local blob.
    ///
    /// # Errors
    /// Returns not-found or storage errors.
    pub async fn delete_file(&self, id: &str) -> Result<DeleteObject, FileSearchError> {
        validate_file_id(id).map_err(|error| file_lookup_error(error, id, "id"))?;
        let permit = self.permit()?;
        let storage = self.storage.clone();
        let files = self.files.clone();
        let id = id.to_owned();
        // Once deletion starts, finish SQL cascading deletion and blob cleanup
        // even if the caller disconnects. The repository records a durable
        // cleanup marker in the same transaction as metadata deletion.
        tokio::spawn(async move {
            let _permit = permit;
            let (needs_blob_cleanup, cleanup_token) =
                match storage.file(&id).await {
                    Ok(file) => {
                        storage.delete(Collection::Files, &id, None).await?;
                        if matches!(file.content, StoredFileContent::Blob) {
                            (true, storage.pending_blob_cleanup(&id).await?)
                        } else {
                            (false, None)
                        }
                    }
                    Err(FileSearchError::NotFound(_)) => (
                        true,
                        Some(storage.pending_blob_cleanup(&id).await?.ok_or_else(|| {
                            FileSearchError::FileNotFound {
                                id: id.clone(),
                                param: "id",
                            }
                        })?),
                    ),
                    Err(error) => return Err(file_lookup_error(error, &id, "id")),
                };
            if needs_blob_cleanup {
                // Reconciliation may have removed the marker and bytes after metadata deletion.
                // Blob removal is idempotent, so missing marker is still a successful deletion.
                files.remove(&id).await?;
                if let Some(cleanup_token) = cleanup_token {
                    storage.complete_blob_cleanup(&id, &cleanup_token).await?;
                }
            }
            Ok(deleted(&id, "file"))
        })
        .await?
    }

    /// Reconciles durable blob intents after a restart. A live metadata row
    /// owns its blob; otherwise the orphaned bytes are removed idempotently.
    /// Claimed uploads retain a tombstone because an expired publisher may
    /// still complete an in-flight write after this pass.
    ///
    /// # Errors
    /// Returns storage or blob removal errors, leaving the marker for retry.
    pub async fn reconcile_blob_cleanup(&self) -> Result<(), FileSearchError> {
        let now = chrono::Utc::now().timestamp();
        loop {
            let batch = self.storage.blob_cleanup_batch(100, now).await?;
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
