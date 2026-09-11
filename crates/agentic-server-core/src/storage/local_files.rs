//! Local blobs addressed exclusively by generated file IDs.
//!
//! A complete, synced staging file is linked atomically to its final key before
//! SQL metadata publication. Callers retain ownership of cleanup through commit.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::OnceCell,
};
use tokio_util::sync::CancellationToken;

use crate::types::file_search::FileSearchError;

#[derive(Clone)]
pub(crate) struct LocalFiles {
    directory: PathBuf,
    missing_directories: Arc<OnceCell<Vec<PathBuf>>>,
    directory_ready: Arc<OnceCell<()>>,
    #[cfg(all(test, unix))]
    directory_syncs: std::sync::Arc<std::sync::Mutex<Vec<PathBuf>>>,
}

impl LocalFiles {
    pub(crate) fn new(directory: PathBuf) -> Result<Self, FileSearchError> {
        if !directory.is_absolute() {
            return Err(FileSearchError::InvalidRequest(
                "files_storage_dir must be an absolute path".into(),
            ));
        }
        Ok(Self {
            directory,
            missing_directories: Arc::default(),
            directory_ready: Arc::default(),
            #[cfg(all(test, unix))]
            directory_syncs: std::sync::Arc::default(),
        })
    }

    pub(crate) fn validate_id(id: &str) -> Result<(), FileSearchError> {
        let valid = id
            .strip_prefix("file-")
            .and_then(|suffix| uuid::Uuid::parse_str(suffix).ok().map(|uuid| (suffix, uuid)))
            .is_some_and(|(suffix, uuid)| uuid.hyphenated().to_string() == suffix);
        if !valid {
            return Err(FileSearchError::InvalidRequest(
                "file ID must be a generated file-UUID key".into(),
            ));
        }
        Ok(())
    }

    fn path(&self, id: &str) -> Result<PathBuf, FileSearchError> {
        Self::validate_id(id)?;
        Ok(self.directory.join(id))
    }

    #[cfg(test)]
    pub(crate) async fn publish(
        &self,
        id: &str,
        bytes: &[u8],
        cancelled: &CancellationToken,
    ) -> Result<(), FileSearchError> {
        self.publish_stream(id, &mut std::io::Cursor::new(bytes), cancelled)
            .await
            .map(|_| ())
    }

    pub(crate) async fn publish_stream(
        &self,
        id: &str,
        reader: &mut (impl tokio::io::AsyncRead + Unpin),
        cancelled: &CancellationToken,
    ) -> Result<usize, FileSearchError> {
        let destination = self.path(id)?;
        let temporary = self.directory.join(format!(".upload-{id}-{}", uuid::Uuid::now_v7()));
        self.ensure_directory().await?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o660).custom_flags(libc::O_NOFOLLOW);
        let mut file = options
            .open(&temporary)
            .await
            .map_err(|source| io_error("staging creation", source))?;
        let written = async {
            let mut total = 0usize;
            let mut buffer = vec![0; 64 * 1024];
            loop {
                let len = tokio::select! {
                    biased;
                    () = cancelled.cancelled() => return Err(cancelled_error()),
                    result = reader.read(&mut buffer) => result.map_err(|source| io_error("upload read", source))?,
                };
                if len == 0 {
                    break;
                }
                total = total.saturating_add(len);
                if total > crate::tool::file_search::MAX_FILE_BYTES {
                    return Err(FileSearchError::InvalidRequest("File exceeds 512 MiB".into()));
                }
                file.write_all(&buffer[..len])
                    .await
                    .map_err(|source| io_error("write", source))?;
            }
            file.flush().await.map_err(|source| io_error("flush", source))?;
            file.sync_all().await.map_err(|source| io_error("sync", source))?;
            Ok(total)
        }
        .await;
        // Tokio may have a buffered write in flight when cancellation was observed.
        // Finish it before unlinking so cleanup also works on Windows.
        let flushed = file.flush().await.map_err(|source| io_error("flush", source));
        drop(file);
        let total = match written.and_then(|total| flushed.map(|()| total)) {
            Ok(total) => total,
            Err(error) => {
                remove_if_present(&temporary).await?;
                return Err(error);
            }
        };
        if cancelled.is_cancelled() {
            remove_if_present(&temporary).await?;
            return Err(cancelled_error());
        }
        // A hard link gives atomic create-if-absent semantics without overwriting
        // another blob on the same filesystem. No caller-controlled path is used.
        if let Err(source) = fs::hard_link(&temporary, &destination).await {
            remove_if_present(&temporary).await?;
            return Err(io_error("publication", source));
        }
        if let Err(error) = remove_if_present(&temporary).await {
            remove_if_present(&destination).await?;
            return Err(error);
        }
        if let Err(error) = self.sync_directory().await {
            remove_if_present(&destination).await?;
            return Err(error);
        }
        Ok(total)
    }

    pub(crate) async fn read(
        &self,
        id: &str,
        expected_bytes: i64,
        max_bytes: usize,
    ) -> Result<Vec<u8>, FileSearchError> {
        let file = self.open_read(id, expected_bytes, max_bytes).await?;
        let expected = u64::try_from(expected_bytes)
            .map_err(|_| io_error("read", io::Error::new(io::ErrorKind::InvalidData, "invalid blob size")))?;
        let mut bytes = Vec::new();
        file.take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|source| io_error("read", source))?;
        if bytes.len() > max_bytes || bytes.len() as u64 != expected {
            return Err(io_error(
                "read",
                io::Error::new(io::ErrorKind::InvalidData, "blob size changed while reading"),
            ));
        }
        Ok(bytes)
    }

    async fn open_read(&self, id: &str, expected_bytes: i64, max_bytes: usize) -> Result<fs::File, FileSearchError> {
        let path = self.path(id)?;
        let metadata = fs::symlink_metadata(&path)
            .await
            .map_err(|source| io_error("read", source))?;
        if !metadata.file_type().is_file() {
            return Err(io_error(
                "read",
                io::Error::new(io::ErrorKind::InvalidData, "blob is not a regular file"),
            ));
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options.open(path).await.map_err(|source| io_error("read", source))?;
        let metadata = file.metadata().await.map_err(|source| io_error("read", source))?;
        let expected = u64::try_from(expected_bytes)
            .map_err(|_| io_error("read", io::Error::new(io::ErrorKind::InvalidData, "invalid blob size")))?;
        if !metadata.is_file() || metadata.len() > max_bytes as u64 || metadata.len() != expected {
            return Err(io_error(
                "read",
                io::Error::new(io::ErrorKind::InvalidData, "blob size mismatch or limit exceeded"),
            ));
        }
        Ok(file)
    }

    pub(crate) async fn stream(
        &self,
        id: &str,
        expected_bytes: i64,
        sender: tokio::sync::mpsc::Sender<Result<bytes::Bytes, FileSearchError>>,
        ready: tokio::sync::oneshot::Sender<Result<(), FileSearchError>>,
    ) {
        let mut file = match self
            .open_read(id, expected_bytes, crate::tool::file_search::MAX_FILE_BYTES)
            .await
        {
            Ok(file) => file,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        if ready.send(Ok(())).is_err() {
            return;
        }
        let result = async {
            let mut remaining = u64::try_from(expected_bytes)
                .map_err(|_| io_error("read", io::Error::new(io::ErrorKind::InvalidData, "invalid blob size")))?;
            loop {
                if sender.is_closed() {
                    return Ok(());
                }
                let mut buffer = vec![0; 64 * 1024];
                let len = file
                    .read(&mut buffer)
                    .await
                    .map_err(|source| io_error("read", source))?;
                if len as u64 > remaining || (len == 0 && remaining != 0) {
                    return Err(io_error(
                        "read",
                        io::Error::new(io::ErrorKind::InvalidData, "blob size changed while reading"),
                    ));
                }
                if len == 0 {
                    break;
                }
                remaining -= len as u64;
                // Verify EOF before emitting the final declared bytes: an HTTP
                // client may consider Content-Length satisfied immediately.
                if remaining == 0 {
                    let mut extra = [0];
                    let read = file.read(&mut extra).await.map_err(|source| io_error("read", source))?;
                    let metadata = file.metadata().await.map_err(|source| io_error("read", source))?;
                    if read != 0 || metadata.len() != u64::try_from(expected_bytes).unwrap_or(u64::MAX) {
                        return Err(io_error(
                            "read",
                            io::Error::new(io::ErrorKind::InvalidData, "blob size changed while reading"),
                        ));
                    }
                }
                buffer.truncate(len);
                if sender.send(Ok(bytes::Bytes::from(buffer))).await.is_err() {
                    return Ok(());
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let _ = sender.send(Err(error)).await;
        }
    }

    pub(crate) async fn remove(&self, id: &str) -> Result<(), FileSearchError> {
        remove_if_present(&self.path(id)?).await?;
        self.ensure_directory().await?;
        self.sync_directory().await
    }

    async fn ensure_directory(&self) -> Result<(), FileSearchError> {
        self.directory_ready
            .get_or_try_init(|| async {
                // Remember the plan across retries: a failed parent sync must not
                // turn a directory we just created into an assumed durable base.
                let missing = self
                    .missing_directories
                    .get_or_try_init(|| async {
                        let mut missing = Vec::new();
                        let mut current = self.directory.as_path();
                        loop {
                            match fs::metadata(current).await {
                                Ok(metadata) if metadata.is_dir() => break,
                                Ok(_) => {
                                    return Err(io_error(
                                        "directory inspection",
                                        io::Error::new(
                                            io::ErrorKind::NotADirectory,
                                            "storage ancestor is not a directory",
                                        ),
                                    ));
                                }
                                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                    missing.push(current.to_owned());
                                    current = current
                                        .parent()
                                        .ok_or_else(|| io_error("directory inspection", error))?;
                                }
                                Err(error) => return Err(io_error("directory inspection", error)),
                            }
                        }
                        Ok(missing)
                    })
                    .await?;
                // An existing ancestor is the deployment's durable base. We do not
                // open higher ancestors, which may legitimately be execute-only.
                for directory in missing.iter().rev() {
                    let mut builder = fs::DirBuilder::new();
                    #[cfg(unix)]
                    builder.mode(0o770);
                    match builder.create(directory).await {
                        Ok(()) => (),
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            if !fs::metadata(directory)
                                .await
                                .map_err(|source| io_error("directory inspection", source))?
                                .is_dir()
                            {
                                return Err(io_error("directory creation", error));
                            }
                        }
                        Err(error) => return Err(io_error("directory creation", error)),
                    }
                    // Also sync when another creator won mkdir, before declaring
                    // this initialization complete to concurrent local uploads.
                    let parent = directory.parent().ok_or_else(|| {
                        io_error(
                            "directory creation",
                            io::Error::new(io::ErrorKind::InvalidInput, "storage directory has no parent"),
                        )
                    })?;
                    self.sync_directory_path(parent).await?;
                }
                Ok(())
            })
            .await?;
        Ok(())
    }

    async fn sync_directory(&self) -> Result<(), FileSearchError> {
        self.sync_directory_path(&self.directory).await
    }

    async fn sync_directory_path(&self, directory: &Path) -> Result<(), FileSearchError> {
        #[cfg(unix)]
        fs::File::open(directory)
            .await
            .map_err(|source| io_error("directory sync", source))?
            .sync_all()
            .await
            .map_err(|source| io_error("directory sync", source))?;
        #[cfg(all(test, unix))]
        self.directory_syncs.lock().unwrap().push(directory.to_owned());
        Ok(())
    }
}

async fn remove_if_present(path: &Path) -> Result<(), FileSearchError> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error("cleanup", source)),
    }
}

pub(crate) fn cancelled_error() -> FileSearchError {
    FileSearchError::Unavailable("File upload was cancelled".into())
}

fn io_error(operation: &'static str, source: io::Error) -> FileSearchError {
    FileSearchError::FileStorage { operation, source }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_upload_syncs_new_directory_entries_before_publishing() {
        let base = tempfile::tempdir().unwrap();
        let parent = base.path().join("new-parent");
        let directory = parent.join("files");
        let files = LocalFiles::new(directory.clone()).unwrap();
        files
            .publish(
                &format!("file-{}", uuid::Uuid::now_v7()),
                b"durable",
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let synced = files.directory_syncs.lock().unwrap();
        assert!(
            synced.contains(&base.path().to_owned()),
            "the new parent's directory entry must be synced"
        );
        assert!(
            synced.contains(&parent),
            "the new storage directory's entry must be synced"
        );
        assert!(synced.contains(&directory), "the blob's directory entry must be synced");
    }

    #[tokio::test]
    async fn directory_creation_races_still_sync_parent_entries() {
        let base = tempfile::tempdir().unwrap();
        let parent = base.path().join("new-parent");
        let directory = parent.join("files");
        let files = LocalFiles::new(directory.clone()).unwrap();
        files
            .missing_directories
            .set(vec![directory.clone(), parent.clone()])
            .unwrap();
        // Another creator wins after discovery and before our mkdir calls.
        fs::create_dir_all(&directory).await.unwrap();
        files
            .publish(
                &format!("file-{}", uuid::Uuid::now_v7()),
                b"race",
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let synced = files.directory_syncs.lock().unwrap();
        assert!(synced.contains(&base.path().to_owned()));
        assert!(synced.contains(&parent));
    }

    #[tokio::test]
    async fn existing_storage_does_not_require_read_access_to_higher_ancestors() {
        use std::os::unix::fs::PermissionsExt as _;
        let base = tempfile::tempdir().unwrap();
        let ancestor = base.path().join("execute-only");
        let directory = ancestor.join("files");
        fs::create_dir_all(&directory).await.unwrap();
        fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o111))
            .await
            .unwrap();
        let files = LocalFiles::new(directory).unwrap();
        let result = files
            .publish(
                &format!("file-{}", uuid::Uuid::now_v7()),
                b"accessible",
                &CancellationToken::new(),
            )
            .await;
        fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        result.unwrap();
        assert!(!files.directory_syncs.lock().unwrap().contains(&ancestor));
    }
}
