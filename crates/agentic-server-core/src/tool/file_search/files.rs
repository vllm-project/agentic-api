//! Bounded byte transport and owned publication tasks for the Files service.
use super::{
    FileObject, FileSearchError, FileSearchService, LocalFiles, MAX_FILE_BYTES, decode_content, invalid, size_i64,
};
use crate::storage::file_search::FilePublicationFailure;
use crate::types::file_search::FileExpiresAfter;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::{CancellationToken, DropGuard};

/// A bounded upload whose drop cancels publication and cleans staging bytes.
pub struct FileUpload {
    writer: tokio::io::DuplexStream,
    metadata: Option<oneshot::Sender<(String, Option<FileExpiresAfter>)>>,
    task: tokio::task::JoinHandle<Result<FileObject, FileSearchError>>,
    bytes: usize,
    _cancel: DropGuard,
}

impl FileUpload {
    /// Writes bytes with filesystem backpressure.
    /// # Errors
    /// Returns size-limit, cancellation, or filesystem errors.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), FileSearchError> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > MAX_FILE_BYTES {
            return invalid("File exceeds 512 MiB");
        }
        self.writer
            .write_all(bytes)
            .await
            .map_err(|source| FileSearchError::FileStorage {
                operation: "upload",
                source,
            })
    }

    /// Publishes only after every multipart field has been validated by the caller.
    /// # Errors
    /// Returns metadata validation, filesystem, or database errors.
    pub async fn finish(
        mut self,
        purpose: &str,
        expires: Option<FileExpiresAfter>,
    ) -> Result<FileObject, FileSearchError> {
        if self.bytes > MAX_FILE_BYTES {
            return invalid("File exceeds 512 MiB");
        }
        validate_purpose(purpose)?;
        if expires
            .as_ref()
            .is_some_and(|policy| !(3600..=2_592_000).contains(&policy.seconds))
        {
            return invalid("expires_after.seconds must be between 3600 and 2592000");
        }
        self.writer
            .shutdown()
            .await
            .map_err(|source| FileSearchError::FileStorage {
                operation: "upload finish",
                source,
            })?;
        if let Some(sender) = self.metadata.take() {
            let _ = sender.send((purpose.to_owned(), expires));
        }
        self.task.await?
    }
}

/// A bounded original-byte stream. Dropping it stops the owned reader after active I/O finishes.
pub struct FileDownload {
    pub bytes: u64,
    receiver: mpsc::Receiver<Result<bytes::Bytes, FileSearchError>>,
}

impl futures::Stream for FileDownload {
    type Item = Result<bytes::Bytes, FileSearchError>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

pub(super) fn validate_purpose(purpose: &str) -> Result<(), FileSearchError> {
    if !matches!(
        purpose,
        "assistants" | "batch" | "fine-tune" | "vision" | "user_data" | "evals"
    ) {
        return invalid("unsupported file purpose");
    }
    Ok(())
}

impl FileSearchService {
    /// Begins bounded staging; no file metadata becomes visible until `finish`.
    /// # Errors
    /// Returns invalid metadata or capacity errors.
    pub fn begin_file_upload(&self, filename: &str, content_type: &str) -> Result<FileUpload, FileSearchError> {
        if filename.is_empty() || filename.len() > 255 || filename.chars().any(char::is_control) {
            return invalid("filename must contain 1 to 255 bytes without control characters");
        }
        if content_type.is_empty() || content_type.len() > 256 || content_type.chars().any(char::is_control) {
            return invalid("content_type must contain 1 to 256 bytes without control characters");
        }
        let permit = self.permit()?;
        let (writer, mut reader) = tokio::io::duplex(64 * 1024);
        let (metadata, receiver) = oneshot::channel::<(String, Option<FileExpiresAfter>)>();
        let cancelled = CancellationToken::new();
        let guard = cancelled.clone().drop_guard();
        let storage = self.storage.clone();
        let files = self.files.clone();
        let filename = filename.to_owned();
        let content_type = content_type.to_owned();
        let task = tokio::spawn(async move {
            let _permit = permit;
            let id = format!("file-{}", uuid::Uuid::now_v7());
            let bytes = files.publish_stream(&id, &mut reader, &cancelled).await?;
            let metadata = tokio::select! {
                biased;
                () = cancelled.cancelled() => None,
                result = receiver => result.ok(),
            };
            let Some((purpose, expires)) = metadata else {
                files.remove(&id).await?;
                return Err(crate::storage::local_files::cancelled_error());
            };
            let created_at = chrono::Utc::now().timestamp();
            let seconds = expires
                .map(|policy| policy.seconds)
                .or_else(|| (purpose == "batch").then_some(2_592_000));
            let file = FileObject {
                id,
                object: "file".into(),
                bytes: size_i64(bytes)?,
                created_at,
                filename,
                purpose,
                expires_at: seconds.map(|seconds| created_at + i64::from(seconds)),
                status: "processed".into(),
            };
            match storage.upload(&file, &content_type, &cancelled).await {
                Ok(()) => Ok(file),
                Err(FilePublicationFailure::SafeToRemove(error)) => {
                    files.remove(&file.id).await?;
                    Err(error)
                }
                Err(FilePublicationFailure::Indeterminate(error)) => Err(error.into()),
            }
        });
        Ok(FileUpload {
            writer,
            metadata: Some(metadata),
            task,
            bytes: 0,
            _cancel: guard,
        })
    }

    /// Opens a verified stream; visibility is linearized before opening the download.
    /// # Errors
    /// Returns not-found, integrity, capacity, or storage errors.
    pub async fn download_file(&self, id: &str) -> Result<FileDownload, FileSearchError> {
        LocalFiles::validate_id(id)?;
        let permit = self.permit()?;
        let uploaded = self.storage.file(id).await?;
        let object: FileObject = serde_json::from_str(&uploaded.data)?;
        let expected = u64::try_from(object.bytes).map_err(|_| FileSearchError::ProviderProtocol)?;
        let (sender, receiver) = mpsc::channel(2);
        let (ready, opened) = oneshot::channel();
        let files = self.files.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let retained_permit = permit;
            if uploaded.content_base64.is_empty() {
                files.stream(&id, object.bytes, sender, ready).await;
            } else {
                let decode_permit = retained_permit.clone();
                let decoded = tokio::task::spawn_blocking(move || {
                    let _permit = decode_permit;
                    decode_content(&uploaded.content_base64)
                })
                .await
                .map_err(FileSearchError::from)
                .and_then(std::convert::identity);
                match decoded {
                    Ok(bytes) if bytes.len() as u64 == expected => {
                        if ready.send(Ok(())).is_ok() {
                            for chunk in bytes.chunks(64 * 1024) {
                                if sender.send(Ok(bytes::Bytes::copy_from_slice(chunk))).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Ok(_) => {
                        let _ = ready.send(Err(FileSearchError::Unavailable("Legacy blob size mismatch".into())));
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            }
        });
        opened
            .await
            .map_err(|_| FileSearchError::Unavailable("Download reader stopped".into()))??;
        Ok(FileDownload {
            bytes: expected,
            receiver,
        })
    }

    /// Deletes due files transactionally and replays durable blob deletion intents.
    /// # Errors
    /// Returns invalid batch size or storage errors. Failed intents remain retryable.
    pub async fn cleanup_expired_files(&self, limit: usize) -> Result<usize, FileSearchError> {
        if !(1..=1000).contains(&limit) {
            return invalid("cleanup limit must be between 1 and 1000");
        }
        let permit = self.permit()?;
        let storage = self.storage.clone();
        let files = self.files.clone();
        tokio::spawn(async move {
            let _permit = permit;
            storage.expire_files(limit).await?;
            let pending = storage.pending_blob_cleanup(limit).await?;
            for id in &pending {
                files.remove(id).await?;
                storage.acknowledge_blob_cleanup(id).await?;
            }
            Ok(pending.len())
        })
        .await?
    }
}
