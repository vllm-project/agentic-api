//! Explicitly owned workers; service clones never create or cancel tasks.
use super::{FileSearchService, ingest, page, validate_list};
use crate::{
    storage::file_search::batches::{BatchFileOptions, ClaimOutcome, ClaimedFileJob},
    types::file_search::{
        AttachFileRequest, AttachmentStatus, ChunkingStrategy, CreateFileBatchRequest, FileBatchObject,
        FileSearchError, ListParams, ListResponse, StaticChunking, VectorStoreFileChunkingStrategy,
        VectorStoreFileError, VectorStoreFileErrorCode, VectorStoreFileObject, invalid, validate_attributes,
    },
};
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const LEASE_SECONDS: i64 = 30;
const HEARTBEAT: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(100);

impl FileSearchService {
    /// Atomically records batch membership and immediately visible pending attachments.
    /// # Errors
    /// Returns validation, missing parent, conflicting attachment, or storage errors.
    pub async fn create_file_batch(
        &self,
        store_id: &str,
        request: CreateFileBatchRequest,
    ) -> Result<FileBatchObject, FileSearchError> {
        let files = match (request.file_ids, request.files) {
            (Some(ids), None) => ids
                .into_iter()
                .map(|file_id| AttachFileRequest {
                    file_id,
                    attributes: request.attributes.clone(),
                    chunking_strategy: request.chunking_strategy.clone(),
                })
                .collect(),
            (None, Some(files)) => files,
            _ => return invalid("Supply exactly one of file_ids or files"),
        };
        if !(1..=2000).contains(&files.len()) {
            return invalid("File batches accept 1 to 2000 members");
        }
        let mut ids = std::collections::HashSet::new();
        let mut members = Vec::with_capacity(files.len());
        for mut file in files {
            if !ids.insert(file.file_id.clone()) {
                return invalid("Duplicate file IDs are not allowed in a batch");
            }
            validate_attributes(&file.attributes)?;
            let strategy = file.chunking_strategy.as_ref().unwrap_or(&ChunkingStrategy::Auto);
            let config = if matches!(strategy, ChunkingStrategy::Auto) {
                StaticChunking {
                    max_chunk_size_tokens: self.config.file_ingestion_params.default_chunk_size_tokens,
                    chunk_overlap_tokens: self.config.file_ingestion_params.default_chunk_overlap_tokens,
                }
            } else {
                ingest::chunking_config(strategy)?
            };
            if matches!(strategy, ChunkingStrategy::Auto) {
                file.chunking_strategy = Some(ChunkingStrategy::Static { config: config.clone() });
            }
            let contextual_identity = self.contextual_identity(&file)?;
            let object = VectorStoreFileObject {
                id: file.file_id.clone(),
                object: "vector_store.file".into(),
                created_at: 0,
                vector_store_id: store_id.into(),
                status: AttachmentStatus::InProgress,
                usage_bytes: 0,
                attributes: file.attributes.clone(),
                chunking_strategy: VectorStoreFileChunkingStrategy::Static { config },
                last_error: None,
            };
            members.push((
                BatchFileOptions {
                    request: file,
                    contextual_identity,
                },
                object,
            ));
        }
        self.compatible(&self.storage.store(store_id).await?)?;
        let identity = self.identity();
        retry(|| self.storage.create_batch(store_id, &identity, &members)).await
    }
    fn contextual_identity(&self, request: &AttachFileRequest) -> Result<Option<String>, FileSearchError> {
        let Some(ChunkingStrategy::Contextual { contextual }) = &request.chunking_strategy else {
            return Ok(None);
        };
        let (provider, model) = self.config.resolve(
            contextual.model_id.as_deref(),
            self.config.contextual_retrieval_params.model.as_ref(),
        )?;
        Ok(Some(format!("{}\n{model}", provider.endpoint("chat/completions")?)))
    }
    /// Retrieves durable batch counts.
    /// # Errors
    /// Returns not-found or storage errors.
    pub async fn get_file_batch(&self, store_id: &str, id: &str) -> Result<FileBatchObject, FileSearchError> {
        self.storage.batch(store_id, id).await
    }
    /// Cancels only unfinished members; completed and failed history remains unchanged.
    /// # Errors
    /// Returns not-found or storage errors.
    pub async fn cancel_file_batch(&self, store_id: &str, id: &str) -> Result<FileBatchObject, FileSearchError> {
        retry(|| self.storage.cancel_batch(store_id, id)).await
    }
    /// Lists membership snapshots, including results whose attachment has since been removed.
    /// # Errors
    /// Returns validation, not-found, or storage errors.
    pub async fn list_file_batch_files(
        &self,
        store_id: &str,
        id: &str,
        params: &ListParams,
    ) -> Result<ListResponse<VectorStoreFileObject>, FileSearchError> {
        validate_list(params)?;
        Ok(page(
            self.storage.batch_files(store_id, id, params).await?,
            params,
            |file| &file.id,
        ))
    }
}

/// Non-clone runtime handle. Call consuming `shutdown` before releasing the server owner.
#[must_use = "the runtime must be explicitly shut down and joined"]
pub struct FileSearchRuntime {
    stop: CancellationToken,
    tasks: JoinSet<()>,
}
impl FileSearchRuntime {
    /// Starts bounded workers after service and server initialization. Polls SQL for restart recovery.
    pub fn start(service: FileSearchService) -> Self {
        Self::start_with_shutdown(service, CancellationToken::new())
    }
    /// Starts workers whose admission and model work stop when the server token is cancelled.
    /// The runtime owner must still call `shutdown` to join them.
    pub fn start_with_shutdown(service: FileSearchService, stop: CancellationToken) -> Self {
        let mut tasks = JoinSet::new();
        // The shared four-operation semaphore also bounds synchronous ingestion and parsing.
        for _ in 0..4 {
            let service = service.clone();
            let stop = stop.clone();
            tasks.spawn(async move {
                worker(service, stop).await;
            });
        }
        let cleanup_stop = stop.clone();
        tasks.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(
                service.config.file_batch_params.cleanup_interval_seconds,
            ));
            loop {
                tokio::select! {biased;()=cleanup_stop.cancelled()=>break,_=tick.tick()=>{}}
                let limit = service.config.file_batch_params.file_batch_chunk_size;
                if let Err(error) = service.cleanup_expired_files(limit).await {
                    tracing::warn!(%error,"expired file cleanup failed");
                    tick.reset_after(HEARTBEAT);
                }
                if let Err(error) = service.cleanup_expired_vector_stores(limit).await {
                    tracing::warn!(%error,"expired store cleanup failed");
                    tick.reset_after(HEARTBEAT);
                }
            }
        });
        Self { stop, tasks }
    }
    /// Stops admission, cooperatively cancels preparation, releases owned claims, and joins all tasks.
    /// Shutdown does not cancel the API batch. Committing transactions are allowed to finish.
    /// # Errors
    /// Reports worker panics after all other owned tasks have joined.
    pub async fn shutdown(mut self) -> Result<(), FileSearchError> {
        self.stop.cancel();
        let mut failure = None;
        while let Some(result) = self.tasks.join_next().await {
            if let Err(error) = result {
                failure = Some(error);
            }
        }
        tracing::info!("file search workers stopped");
        failure.map_or(Ok(()), |error| Err(error.into()))
    }
}
impl Drop for FileSearchRuntime {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn worker(service: FileSearchService, stop: CancellationToken) {
    loop {
        if stop.is_cancelled() {
            break;
        }
        let Ok(permit) = service.permit() else {
            tokio::select! {()=stop.cancelled()=>break,()=tokio::time::sleep(POLL)=>{}}
            continue;
        };
        let params = &service.config.file_batch_params;
        let claim = service
            .storage
            .claim_next(
                LEASE_SECONDS,
                params.max_concurrent_files_per_batch,
                params.file_batch_chunk_size,
            )
            .await;
        match claim {
            Ok(Some(job)) => {
                run_job(&service, &job, permit, &stop).await;
            }
            Ok(None) => {
                drop(permit);
                tokio::select! {()=stop.cancelled()=>break,()=tokio::time::sleep(POLL)=>{}}
            }
            Err(error) => {
                drop(permit);
                tracing::warn!(%error,"batch claim failed; retrying durable queue");
                tokio::select! {()=stop.cancelled()=>break,()=tokio::time::sleep(POLL)=>{}}
            }
        }
    }
}
async fn run_job(
    service: &FileSearchService,
    job: &ClaimedFileJob,
    permit: std::sync::Arc<tokio::sync::OwnedSemaphorePermit>,
    stop: &CancellationToken,
) {
    let cancellation = CancellationToken::new();
    let prepare_service = service.clone();
    let prepare_job = job.clone();
    let prepare_cancellation = cancellation.clone();
    let mut prepare = tokio::spawn(async move {
        let service = &prepare_service;
        let job = &prepare_job;
        let cancellation = &prepare_cancellation;
        if job.identity != service.identity() || job.contextual_identity != service.contextual_identity(&job.options)? {
            return Err(FileSearchError::Conflict(
                "Restore the batch ingestion model configuration before ingestion".into(),
            ));
        }
        let store = service.storage.store(&job.store_id).await?;
        let dimensions = usize::try_from(store.embedding_dimensions).ok().filter(|n| *n > 0);
        service
            .prepare_cancellable(&job.store_id, job.options.clone(), dimensions, permit, cancellation)
            .await
    });
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    let result = loop {
        tokio::select! {biased;
            ()=stop.cancelled()=>break None,
            _=heartbeat.tick()=>{
                match service.storage.renew_claim(job,LEASE_SECONDS).await {
                    Ok(ClaimOutcome::Applied)=>{},
                    Ok(ClaimOutcome::LostClaim)=>break None,
                    Err(error)=>{tracing::warn!(%error,"claim renewal failed");break None;}
                }
            }
            result=&mut prepare=>break Some(result.unwrap_or_else(|error|Err(error.into()))),
        }
    };
    if let Some(result) = result {
        let result = result.map_err(|error| file_error(&error));
        if let Err(error) = service.storage.finish_claim(job, result).await {
            tracing::warn!(%error,"batch publication failed");
            if transient(&error) {
                if let Err(failure) = service.storage.release_claim(job).await {
                    tracing::warn!(%failure,"transient publication claim release failed");
                }
                return;
            }
            // A failed/indeterminate COMMIT may have succeeded. Token fencing makes this safe.
            if let Err(failure) = service.storage.finish_claim(job, Err(file_error(&error))).await {
                tracing::warn!(%failure,"batch failure recording failed");
            }
        }
    } else {
        cancellation.cancel();
        if let Err(error) = prepare.await {
            tracing::warn!(%error,"cancelled preparation task failed");
        }
        if let Err(error) = service.storage.release_claim(job).await {
            tracing::warn!(%error,"claim release failed; lease will expire");
        }
    }
}
fn file_error(error: &FileSearchError) -> VectorStoreFileError {
    VectorStoreFileError {
        code: match error {
            FileSearchError::InvalidRequest(_) => VectorStoreFileErrorCode::InvalidFile,
            FileSearchError::UnsupportedFile(_) => VectorStoreFileErrorCode::UnsupportedFile,
            #[cfg(feature = "file-search-pdf")]
            FileSearchError::PdfParse(_) => VectorStoreFileErrorCode::InvalidFile,
            _ => VectorStoreFileErrorCode::ServerError,
        },
        message: error.to_string(),
    }
}

fn transient(error: &FileSearchError) -> bool {
    let FileSearchError::Storage(sqlx::Error::Database(error)) = error else {
        return false;
    };
    matches!(
        error.code().as_deref(),
        Some("5" | "6" | "261" | "517" | "40001" | "40P01" | "55P03")
    )
}
async fn retry<T, F, Fut>(mut attempt: F) -> Result<T, FileSearchError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, FileSearchError>>,
{
    for tries in 0..3 {
        match attempt().await {
            Err(error) if tries < 2 && transient(&error) => tokio::time::sleep(POLL).await,
            result => return result,
        }
    }
    unreachable!("the final attempt always returns")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[tokio::test]
    async fn cancelled_preparation_joins_parser_and_releases_all_capacity() {
        let pool = crate::storage::create_pool_with_schema(Some("sqlite::memory:"))
            .await
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let service = FileSearchService::new(
            pool,
            Arc::new(reqwest::Client::new()),
            crate::types::file_search::FileSearchConfig {
                files_storage_dir: Some(directory.path().into()),
                ..Default::default()
            },
        )
        .unwrap();
        let file = service
            .upload_file(
                "cancelled.txt",
                "text/plain",
                "assistants",
                b"bounded parser ".repeat(100_000),
            )
            .await
            .unwrap();
        let store = service
            .create_vector_store(serde_json::from_str("{}").unwrap())
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let permit = service.permit().unwrap();
        assert_eq!(service.workers.available_permits(), 3);
        let result = service
            .prepare_cancellable(
                &store.id,
                AttachFileRequest {
                    file_id: file.id.clone(),
                    ..Default::default()
                },
                None,
                permit,
                &cancellation,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(
            service.workers.available_permits(),
            4,
            "parser handle and its capacity must finish before returning"
        );
        service.delete_vector_store(&store.id).await.unwrap();
        service.delete_file(&file.id).await.unwrap();
    }
    #[tokio::test]
    async fn runtime_sweeps_expiration_and_replays_committed_blob_cleanup() {
        let pool = crate::storage::create_pool_with_schema(Some("sqlite::memory:"))
            .await
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let service = FileSearchService::new(
            pool.clone(),
            Arc::new(reqwest::Client::new()),
            crate::types::file_search::FileSearchConfig {
                files_storage_dir: Some(directory.path().into()),
                ..Default::default()
            },
        )
        .unwrap();
        let expired = service
            .upload_file("expired.txt", "text/plain", "assistants", b"expired".to_vec())
            .await
            .unwrap();
        let retained = service
            .upload_file("retained.txt", "text/plain", "assistants", b"retained".to_vec())
            .await
            .unwrap();
        let replay = service
            .upload_file("replay.txt", "text/plain", "assistants", b"replay".to_vec())
            .await
            .unwrap();
        let store = service
            .create_vector_store(serde_json::from_value(serde_json::json!({"file_ids":[retained.id]})).unwrap())
            .await
            .unwrap();
        sqlx::query("UPDATE file_search_files SET expires_at=1 WHERE id=$1")
            .bind(&expired.id)
            .execute(pool.as_ref())
            .await
            .unwrap();
        sqlx::query("UPDATE file_search_stores SET expires_at=1 WHERE id=$1")
            .bind(&store.id)
            .execute(pool.as_ref())
            .await
            .unwrap();
        // Simulate a prior process stopping after the durable delete transaction committed.
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO file_search_blob_cleanup (file_id) VALUES ($1)")
            .bind(&replay.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("DELETE FROM file_search_files WHERE id=$1")
            .bind(&replay.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(directory.path().join(&replay.id).exists());
        let runtime = FileSearchRuntime::start(service.clone());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_attachments WHERE store_id=$1")
                    .bind(&store.id)
                    .fetch_one(pool.as_ref())
                    .await
                    .unwrap();
                if count == 0
                    && !directory.path().join(&expired.id).exists()
                    && !directory.path().join(&replay.id).exists()
                {
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .unwrap();
        runtime.shutdown().await.unwrap();
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_blob_cleanup")
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
        assert_eq!(pending, 0);
        assert!(service.get_file(&retained.id).await.is_ok());
        assert_eq!(service.workers.available_permits(), 4);
        service.delete_vector_store(&store.id).await.unwrap();
        service.delete_file(&retained.id).await.unwrap();
    }
}
