//! Durable batch membership and portable claims. Parent writes serialize every job transition.
use super::{FileSearchStorage, PreparedAttachment, database_now, lifecycle, publish_attachment, serialize_chunks};
use crate::{
    storage::DbTransaction,
    types::file_search::{
        AttachFileRequest, AttachmentStatus, BatchStatus, FileBatchObject, FileCounts, FileSearchError, ListOrder,
        ListParams, VectorStoreFileError, VectorStoreFileObject, invalid,
    },
};

// Per-storage timing controls keep concurrency regressions deterministic without global failpoints.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct BatchTestHooks {
    pub after_commit: Option<CommitBarrier>,
    pub renewal_delay: std::time::Duration,
    pub renewal_started: tokio::sync::Notify,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct CommitBarrier {
    pub reached: tokio::sync::Notify,
    pub resume: tokio::sync::Notify,
    used: std::sync::atomic::AtomicBool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct BatchFileOptions {
    pub request: AttachFileRequest,
    pub contextual_identity: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum JobState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TryFrom<&str> for JobState {
    type Error = FileSearchError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(FileSearchError::Unavailable("Invalid durable job state".into())),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct JobId(String);
#[derive(Clone, Debug)]
pub(crate) struct ClaimToken(String);
#[derive(Clone, Debug)]
pub(crate) struct AttachmentGeneration(String);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaimOutcome {
    Applied,
    LostClaim,
}
#[derive(Clone, Debug)]
pub(crate) struct ClaimedFileJob {
    id: JobId,
    token: ClaimToken,
    generation: AttachmentGeneration,
    pub store_id: String,
    pub options: AttachFileRequest,
    pub identity: String,
    pub contextual_identity: Option<String>,
}
#[derive(sqlx::FromRow)]
struct Job {
    id: String,
    store_id: String,
    generation: String,
    options: String,
    identity: String,
}

impl FileSearchStorage {
    pub(crate) async fn create_batch(
        &self,
        store_id: &str,
        identity: &str,
        members: &[(BatchFileOptions, VectorStoreFileObject)],
    ) -> Result<FileBatchObject, FileSearchError> {
        let mut tx = self.pool.begin().await?;
        let mut ids = members
            .iter()
            .map(|(r, _)| r.request.file_id.as_str())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        for id in ids {
            sqlx::query("UPDATE file_search_files SET id = id WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        lifecycle::lock_store(&mut tx, store_id).await?;
        let now = database_now(&mut tx).await?;
        lifecycle::require_live_store(&mut tx, store_id, now).await?;
        let id = format!("vsfb_{}", uuid::Uuid::now_v7().simple());
        sqlx::query("INSERT INTO file_search_batches (id,store_id,created_at) VALUES ($1,$2,$3)")
            .bind(&id)
            .bind(store_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        let mut counts = FileCounts::default();
        for (saved_options, queued) in members {
            let options = &saved_options.request;
            let exists: Option<String> = sqlx::query_scalar(
                "SELECT id FROM file_search_files WHERE id=$1 AND (expires_at IS NULL OR expires_at>$2)",
            )
            .bind(&options.file_id)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?;
            if exists.is_none() {
                return Err(FileSearchError::NotFound(
                    "Batch source file not found or expired".into(),
                ));
            }
            let existing: Option<String> =
                sqlx::query_scalar("SELECT data FROM file_search_attachments WHERE store_id=$1 AND file_id=$2")
                    .bind(store_id)
                    .bind(&options.file_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            let mut object = queued.clone();
            object.created_at = now;
            let generation = uuid::Uuid::now_v7().to_string();
            let state = if let Some(existing) = existing {
                object = serde_json::from_str(&existing)?;
                if object.status != AttachmentStatus::Completed {
                    return Err(FileSearchError::Conflict(
                        "Batch file already has an unfinished or unsuccessful attachment; detach it before retrying"
                            .into(),
                    ));
                }
                counts.completed += 1;
                "completed"
            } else {
                sqlx::query("INSERT INTO file_search_attachments (store_id,file_id,created_at,usage_bytes,storage_bytes,data,status,generation) VALUES ($1,$2,$3,0,0,$4,'in_progress',$5)")
                    .bind(store_id).bind(&options.file_id).bind(now).bind(serde_json::to_string(&object)?).bind(&generation).execute(&mut *tx).await?;
                counts.in_progress += 1;
                "queued"
            };
            counts.total += 1;
            sqlx::query("INSERT INTO file_search_jobs (id,batch_id,store_id,file_id,generation,created_at,updated_at,state,options,identity,snapshot) VALUES ($1,$2,$3,$4,$5,$6,$6,$7,$8,$9,$10)")
                .bind(uuid::Uuid::now_v7().to_string()).bind(&id).bind(store_id).bind(&options.file_id).bind(generation).bind(now).bind(state).bind(serde_json::to_string(saved_options)?).bind(identity).bind(serde_json::to_string(&object)?).execute(&mut *tx).await?;
        }
        // Build the response from this transaction's membership. After COMMIT succeeds,
        // no fallible response read may turn success into a retry of batch creation.
        let object = FileBatchObject {
            id,
            object: "vector_store.files_batch".into(),
            created_at: now,
            vector_store_id: store_id.into(),
            status: if counts.in_progress > 0 {
                BatchStatus::InProgress
            } else {
                BatchStatus::Completed
            },
            file_counts: counts,
        };
        tx.commit().await?;
        #[cfg(test)]
        if let Some(barrier) = self
            .batch_test_hooks
            .as_ref()
            .and_then(|hooks| hooks.after_commit.as_ref())
        {
            if !barrier.used.swap(true, std::sync::atomic::Ordering::SeqCst) {
                barrier.reached.notify_one();
                barrier.resume.notified().await;
            }
        }
        Ok(object)
    }

    pub(crate) async fn batch(&self, store_id: &str, id: &str) -> Result<FileBatchObject, FileSearchError> {
        let mut tx = self.pool.begin().await?;
        lifecycle::lock_store(&mut tx, store_id).await?;
        let now = database_now(&mut tx).await?;
        lifecycle::require_live_store(&mut tx, store_id, now).await?;
        let row: Option<(i64, i64)> =
            sqlx::query_as("SELECT created_at,cancelled FROM file_search_batches WHERE id=$1 AND store_id=$2")
                .bind(id)
                .bind(store_id)
                .fetch_optional(&mut *tx)
                .await?;
        let (created_at, cancelled) = row.ok_or_else(|| FileSearchError::NotFound("File batch not found".into()))?;
        let totals: Vec<(String, i64)> =
            sqlx::query_as("SELECT state,COUNT(*) FROM file_search_jobs WHERE batch_id=$1 GROUP BY state")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        let mut counts = FileCounts::default();
        for (state, count) in totals {
            match JobState::try_from(state.as_str())? {
                JobState::Queued | JobState::Running => counts.in_progress += count,
                JobState::Completed => counts.completed += count,
                JobState::Failed => counts.failed += count,
                JobState::Cancelled => counts.cancelled += count,
            }
            counts.total += count;
        }
        let status = if cancelled != 0 {
            BatchStatus::Cancelled
        } else if counts.in_progress > 0 {
            BatchStatus::InProgress
        } else {
            BatchStatus::Completed
        };
        tx.commit().await?;
        Ok(FileBatchObject {
            id: id.into(),
            object: "vector_store.files_batch".into(),
            created_at,
            vector_store_id: store_id.into(),
            status,
            file_counts: counts,
        })
    }

    pub(crate) async fn batch_files(
        &self,
        store_id: &str,
        id: &str,
        params: &ListParams,
    ) -> Result<Vec<VectorStoreFileObject>, FileSearchError> {
        self.batch(store_id, id).await?;
        let cursor = params.after.as_deref().or(params.before.as_deref()).unwrap_or("");
        if !cursor.is_empty() {
            let found: Option<String> =
                sqlx::query_scalar("SELECT file_id FROM file_search_jobs WHERE batch_id=$1 AND file_id=$2")
                    .bind(id)
                    .bind(cursor)
                    .fetch_optional(self.pool.as_ref())
                    .await?;
            if found.is_none() {
                return invalid("Pagination cursor does not exist in this batch");
            }
        }
        // All members share the atomic batch creation timestamp, so file_id breaks every tie.
        let ascending = matches!(params.order.unwrap_or_default(), ListOrder::Asc) == params.before.is_none();
        let operator = if ascending { ">" } else { "<" };
        let order = if ascending { "ASC" } else { "DESC" };
        let filter = params.filter.map_or("", AttachmentStatus::as_str);
        let sql = format!(
            "SELECT snapshot FROM file_search_jobs WHERE batch_id=$1 AND ($2='' OR file_id {operator} $2) AND ($3='' OR state=$3 OR ($3='in_progress' AND state IN ('queued','running'))) ORDER BY file_id {order} LIMIT $4"
        );
        let rows: Vec<String> = sqlx::query_scalar(&sql)
            .bind(id)
            .bind(cursor)
            .bind(filter)
            .bind(i64::try_from(params.limit.unwrap_or(20) + 1).unwrap_or(101))
            .fetch_all(self.pool.as_ref())
            .await?;
        rows.iter()
            .map(|s| serde_json::from_str(s).map_err(Into::into))
            .collect()
    }

    pub(crate) async fn cancel_batch(&self, store_id: &str, id: &str) -> Result<FileBatchObject, FileSearchError> {
        let mut tx = self.pool.begin().await?;
        lifecycle::lock_store(&mut tx, store_id).await?;
        let now = database_now(&mut tx).await?;
        lifecycle::require_live_store(&mut tx, store_id, now).await?;
        let found=sqlx::query("UPDATE file_search_batches SET cancelled=1 WHERE id=$1 AND store_id=$2 AND EXISTS (SELECT 1 FROM file_search_jobs WHERE batch_id=$1 AND state IN ('queued','running'))").bind(id).bind(store_id).execute(&mut *tx).await?.rows_affected();
        if found > 0 {
            let rows:Vec<(String,String,String)>=sqlx::query_as("SELECT id,generation,snapshot FROM file_search_jobs WHERE batch_id=$1 AND state IN ('queued','running') ORDER BY id").bind(id).fetch_all(&mut *tx).await?;
            for (job, generation, data) in rows {
                let mut object: VectorStoreFileObject = serde_json::from_str(&data)?;
                object.status = AttachmentStatus::Cancelled;
                terminal(&mut tx, &job, &generation, &object, "cancelled", now).await?;
            }
        }
        tx.commit().await?;
        self.batch(store_id, id).await
    }

    pub(crate) async fn claim_next(
        &self,
        lease: i64,
        per_batch: usize,
        scan: usize,
    ) -> Result<Option<ClaimedFileJob>, FileSearchError> {
        let mut connection = self.pool.acquire().await?;
        let now = database_now(&mut connection).await?;
        drop(connection);
        let candidates:Vec<(String,String)>=sqlx::query_as("SELECT id,store_id FROM file_search_jobs WHERE state='queued' OR (state='running' AND lease_until<=$1) ORDER BY created_at,id LIMIT $2").bind(now).bind(i64::try_from(scan).unwrap_or(1000)).fetch_all(self.pool.as_ref()).await?;
        for (id, store) in candidates {
            let mut tx = self.pool.begin().await?;
            if let Err(error) = lifecycle::lock_store(&mut tx, &store).await {
                if matches!(error, FileSearchError::NotFound(_)) {
                    continue;
                }
                return Err(error);
            }
            sqlx::query("UPDATE file_search_jobs SET id=id WHERE id=$1")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            let now = database_now(&mut tx).await?;
            let token = uuid::Uuid::now_v7().to_string();
            let updated=sqlx::query("UPDATE file_search_jobs SET state='running',claim_token=$2,lease_until=$3,updated_at=$4,attempts=attempts+1 WHERE id=$1 AND (state='queued' OR (state='running' AND lease_until<=$4)) AND (SELECT COUNT(*) FROM file_search_jobs j WHERE j.batch_id=file_search_jobs.batch_id AND j.state='running' AND j.lease_until>$4)<$5")
                .bind(&id).bind(&token).bind(now+lease).bind(now).bind(i64::try_from(per_batch).unwrap_or(32)).execute(&mut *tx).await?.rows_affected();
            if updated == 0 {
                continue;
            }
            let row: Job =
                sqlx::query_as("SELECT id,store_id,generation,options,identity FROM file_search_jobs WHERE id=$1")
                    .bind(&id)
                    .fetch_one(&mut *tx)
                    .await?;
            let options: BatchFileOptions = serde_json::from_str(&row.options)?;
            let job = ClaimedFileJob {
                id: JobId(row.id),
                token: ClaimToken(token),
                generation: AttachmentGeneration(row.generation),
                store_id: row.store_id,
                options: options.request,
                contextual_identity: options.contextual_identity,
                identity: row.identity,
            };
            // Missing generations/expired parents become durable cancellation, never publication.
            if !live(&mut tx, &job, now).await? {
                let data: String = sqlx::query_scalar("SELECT snapshot FROM file_search_jobs WHERE id=$1")
                    .bind(&id)
                    .fetch_one(&mut *tx)
                    .await?;
                let mut object: VectorStoreFileObject = serde_json::from_str(&data)?;
                object.status = AttachmentStatus::Cancelled;
                terminal(&mut tx, &id, &job.generation.0, &object, "cancelled", now).await?;
                tx.commit().await?;
                continue;
            }
            tx.commit().await?;
            return Ok(Some(job));
        }
        Ok(None)
    }

    pub(crate) async fn renew_claim(&self, job: &ClaimedFileJob, lease: i64) -> Result<ClaimOutcome, FileSearchError> {
        let mut tx = self.pool.begin().await?;
        if let Err(error) = lifecycle::lock_store(&mut tx, &job.store_id).await {
            if matches!(error, FileSearchError::NotFound(_)) {
                return Ok(ClaimOutcome::LostClaim);
            }
            return Err(error);
        }
        sqlx::query("UPDATE file_search_jobs SET id=id WHERE id=$1")
            .bind(&job.id.0)
            .execute(&mut *tx)
            .await?;
        let now = database_now(&mut tx).await?;
        if !live(&mut tx, job, now).await? {
            return Ok(ClaimOutcome::LostClaim);
        }
        let n=sqlx::query("UPDATE file_search_jobs SET lease_until=$3,updated_at=$4 WHERE id=$1 AND claim_token=$2 AND state='running' AND lease_until>$4").bind(&job.id.0).bind(&job.token.0).bind(now+lease).bind(now).execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        #[cfg(test)]
        if let Some(hooks) = &self.batch_test_hooks {
            hooks.renewal_started.notify_one();
            tokio::time::sleep(hooks.renewal_delay).await;
        }
        Ok(if n == 1 {
            ClaimOutcome::Applied
        } else {
            ClaimOutcome::LostClaim
        })
    }

    pub(crate) async fn release_claim(&self, job: &ClaimedFileJob) -> Result<(), FileSearchError> {
        // A conditional single statement is sufficient: it does not write attachments or terminal state.
        sqlx::query("UPDATE file_search_jobs SET state='queued',claim_token=NULL,lease_until=NULL WHERE id=$1 AND claim_token=$2 AND state='running'").bind(&job.id.0).bind(&job.token.0).execute(self.pool.as_ref()).await?;
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keeps claim validation and shared publication in one transaction"
    )]
    pub(crate) async fn finish_claim(
        &self,
        job: &ClaimedFileJob,
        result: Result<PreparedAttachment, VectorStoreFileError>,
    ) -> Result<ClaimOutcome, FileSearchError> {
        let mut encoded = if let Ok(prepared) = &result {
            Some(serialize_chunks(&prepared.chunks).await?)
        } else {
            None
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE file_search_files SET id=id WHERE id=$1")
            .bind(&job.options.file_id)
            .execute(&mut *tx)
            .await?;
        if let Err(error) = lifecycle::lock_store(&mut tx, &job.store_id).await {
            if matches!(error, FileSearchError::NotFound(_)) {
                return Ok(ClaimOutcome::LostClaim);
            }
            return Err(error);
        }
        sqlx::query("UPDATE file_search_jobs SET id=id WHERE id=$1")
            .bind(&job.id.0)
            .execute(&mut *tx)
            .await?;
        let now = database_now(&mut tx).await?;
        if !live(&mut tx, job, now).await? {
            return Ok(ClaimOutcome::LostClaim);
        }
        let data:Option<String>=sqlx::query_scalar("SELECT snapshot FROM file_search_jobs WHERE id=$1 AND claim_token=$2 AND state='running' AND lease_until>$3").bind(&job.id.0).bind(&job.token.0).bind(now).fetch_optional(&mut *tx).await?;
        let Some(data) = data else {
            return Ok(ClaimOutcome::LostClaim);
        };
        let mut object: VectorStoreFileObject = serde_json::from_str(&data)?;
        match result {
            Ok(mut prepared) => {
                prepared.object.created_at = object.created_at;
                let current: String =
                    sqlx::query_scalar("SELECT data FROM file_search_attachments WHERE store_id=$1 AND file_id=$2")
                        .bind(&job.store_id)
                        .bind(&job.options.file_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let current: VectorStoreFileObject = serde_json::from_str(&current)?;
                if prepared.object.attributes != current.attributes {
                    prepared.object.attributes = current.attributes;
                    for chunk in &mut prepared.chunks {
                        chunk.attributes.clone_from(&prepared.object.attributes);
                    }
                    encoded = Some(serialize_chunks(&prepared.chunks).await?);
                }
                // Remove only our pending generation, then reuse the one atomic chunk writer.
                sqlx::query("DELETE FROM file_search_attachments WHERE store_id=$1 AND file_id=$2 AND generation=$3")
                    .bind(&job.store_id)
                    .bind(&job.options.file_id)
                    .bind(&job.generation.0)
                    .execute(&mut *tx)
                    .await?;
                let (chunks, bytes) =
                    encoded.ok_or_else(|| FileSearchError::Unavailable("Missing prepared chunks".into()))?;
                publish_attachment(
                    &mut tx,
                    &job.store_id,
                    &job.identity,
                    &prepared,
                    &chunks,
                    bytes,
                    &job.generation.0,
                )
                .await?;
                object = prepared.object;
            }
            Err(error) => {
                object.status = AttachmentStatus::Failed;
                object.last_error = Some(error);
            }
        }
        let now = database_now(&mut tx).await?;
        let valid: Option<String> = sqlx::query_scalar(
            "SELECT id FROM file_search_jobs WHERE id=$1 AND claim_token=$2 AND state='running' AND lease_until>$3",
        )
        .bind(&job.id.0)
        .bind(&job.token.0)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        if valid.is_none() {
            return Ok(ClaimOutcome::LostClaim);
        }
        lifecycle::require_live_store(&mut tx, &job.store_id, now).await?;
        let source: Option<String> = sqlx::query_scalar(
            "SELECT id FROM file_search_files WHERE id=$1 AND (expires_at IS NULL OR expires_at>$2)",
        )
        .bind(&job.options.file_id)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        if source.is_none() {
            return Ok(ClaimOutcome::LostClaim);
        }
        terminal(
            &mut tx,
            &job.id.0,
            &job.generation.0,
            &object,
            object.status.as_str(),
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(ClaimOutcome::Applied)
    }
}

async fn live(tx: &mut DbTransaction<'_>, job: &ClaimedFileJob, now: i64) -> Result<bool, FileSearchError> {
    let found:Option<String>=sqlx::query_scalar("SELECT a.file_id FROM file_search_attachments a JOIN file_search_files f ON f.id=a.file_id JOIN file_search_stores s ON s.id=a.store_id JOIN file_search_jobs j ON j.id=$4 JOIN file_search_batches b ON b.id=j.batch_id WHERE a.store_id=$1 AND a.file_id=$2 AND a.generation=$3 AND a.status='in_progress' AND b.cancelled=0 AND s.lifecycle_status!='expired' AND (s.expires_at IS NULL OR s.expires_at>$5) AND (f.expires_at IS NULL OR f.expires_at>$5)").bind(&job.store_id).bind(&job.options.file_id).bind(&job.generation.0).bind(&job.id.0).bind(now).fetch_optional(&mut **tx).await?;
    Ok(found.is_some())
}
async fn terminal(
    tx: &mut DbTransaction<'_>,
    id: &str,
    generation: &str,
    object: &VectorStoreFileObject,
    state: &str,
    now: i64,
) -> Result<(), FileSearchError> {
    let data = serde_json::to_string(object)?;
    sqlx::query(
        "UPDATE file_search_jobs SET state=$2,snapshot=$3,updated_at=$4,claim_token=NULL,lease_until=NULL WHERE id=$1",
    )
    .bind(id)
    .bind(state)
    .bind(&data)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE file_search_attachments SET data=$3,status=$4 WHERE store_id=$1 AND file_id=$2 AND generation=$5",
    )
    .bind(&object.vector_store_id)
    .bind(&object.id)
    .bind(data)
    .bind(object.status.as_str())
    .bind(generation)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Parent lifecycle writes already own the source or store lock before invalidating unfinished members.
pub(super) async fn invalidate(
    tx: &mut DbTransaction<'_>,
    store: Option<&str>,
    file: Option<&str>,
) -> Result<(), FileSearchError> {
    let rows:Vec<(String,String,String)>=sqlx::query_as("SELECT id,generation,snapshot FROM file_search_jobs WHERE ($1='' OR store_id=$1) AND ($2='' OR file_id=$2) AND state IN ('queued','running') ORDER BY id").bind(store.unwrap_or("")).bind(file.unwrap_or("")).fetch_all(&mut **tx).await?;
    for (id, generation, data) in rows {
        let mut object: VectorStoreFileObject = serde_json::from_str(&data)?;
        object.status = AttachmentStatus::Cancelled;
        let now = database_now(&mut *tx).await?;
        terminal(tx, &id, &generation, &object, "cancelled", now).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::file_search::{FileSearchConfig, VectorStoreFileErrorCode};
    use crate::{storage::create_pool_with_schema, tool::file_search::FileSearchService};
    use serde_json::json;
    use std::sync::Arc;

    #[allow(
        clippy::too_many_lines,
        reason = "keeps competing claims and old-generation publication assertions together"
    )]
    async fn fencing(database: &str) {
        let pool = create_pool_with_schema(Some(database)).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let service = FileSearchService::new(
            pool.clone(),
            Arc::new(reqwest::Client::new()),
            FileSearchConfig {
                files_storage_dir: Some(dir.path().into()),
                ..Default::default()
            },
        )
        .unwrap();
        let storage = FileSearchStorage::new(pool.clone());
        let store = service
            .create_vector_store(serde_json::from_value(json!({})).unwrap())
            .await
            .unwrap();
        let file = service
            .upload_file("fence.txt", "text/plain", "assistants", b"fenced text".to_vec())
            .await
            .unwrap();
        let request = || serde_json::from_value(json!({"file_ids":[file.id]})).unwrap();
        let batch = service.create_file_batch(&store.id, request()).await.unwrap();
        let (one, two) = tokio::join!(storage.claim_next(30, 3, 1000), storage.claim_next(30, 3, 1000));
        let mut claims = [one.unwrap(), two.unwrap()].into_iter().flatten();
        let stale = claims.next().unwrap();
        assert!(claims.next().is_none(), "only one concurrent claimant wins");
        sqlx::query("UPDATE file_search_jobs SET lease_until=1 WHERE id=$1")
            .bind(&stale.id.0)
            .execute(pool.as_ref())
            .await
            .unwrap();
        let current = storage.claim_next(30, 3, 1000).await.unwrap().unwrap();
        assert_ne!(stale.token.0, current.token.0);
        assert_eq!(storage.renew_claim(&stale, 30).await.unwrap(), ClaimOutcome::LostClaim);
        let mut object = service.get_vector_store_file(&store.id, &file.id).await.unwrap();
        object.status = AttachmentStatus::Completed;
        let prepared = || PreparedAttachment {
            object: object.clone(),
            chunks: vec![super::super::StoredChunk {
                file_id: file.id.clone(),
                filename: "fence.txt".into(),
                chunk_index: 0,
                text: "stale".into(),
                embedding_text: None,
                embedding: None,
                attributes: object.attributes.clone(),
            }],
            dimensions: 0,
            parsed_content: "stale".into(),
        };
        assert_eq!(
            storage.finish_claim(&stale, Ok(prepared())).await.unwrap(),
            ClaimOutcome::LostClaim
        );
        assert_eq!(
            storage.finish_claim(&stale, Err(failure())).await.unwrap(),
            ClaimOutcome::LostClaim
        );
        storage.release_claim(&stale).await.unwrap();
        assert_eq!(storage.renew_claim(&current, 30).await.unwrap(), ClaimOutcome::Applied);
        service.cancel_file_batch(&store.id, &batch.id).await.unwrap();
        assert_eq!(
            storage.finish_claim(&current, Err(failure())).await.unwrap(),
            ClaimOutcome::LostClaim
        );
        assert_eq!(
            service
                .get_file_batch(&store.id, &batch.id)
                .await
                .unwrap()
                .file_counts
                .cancelled,
            1
        );
        service.detach_file(&store.id, &file.id).await.unwrap();
        let second = service.create_file_batch(&store.id, request()).await.unwrap();
        let old = storage.claim_next(30, 3, 1000).await.unwrap().unwrap();
        service.detach_file(&store.id, &file.id).await.unwrap();
        service
            .attach_file(
                &store.id,
                AttachFileRequest {
                    file_id: file.id.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            storage.finish_claim(&old, Ok(prepared())).await.unwrap(),
            ClaimOutcome::LostClaim
        );
        assert_eq!(
            storage.finish_claim(&old, Err(failure())).await.unwrap(),
            ClaimOutcome::LostClaim
        );
        storage.release_claim(&old).await.unwrap();
        assert!(storage.claim_next(30, 3, 1000).await.unwrap().is_none());
        assert_eq!(
            service.get_vector_store_file(&store.id, &file.id).await.unwrap().status,
            AttachmentStatus::Completed
        );
        assert_eq!(
            service
                .get_file_batch(&store.id, &second.id)
                .await
                .unwrap()
                .file_counts
                .cancelled,
            1
        );
        let reused = service.create_file_batch(&store.id, request()).await.unwrap();
        assert_eq!(reused.file_counts.completed, 1);
        service.delete_vector_store(&store.id).await.unwrap();
        service.delete_file(&file.id).await.unwrap();
    }
    fn failure() -> VectorStoreFileError {
        VectorStoreFileError {
            code: VectorStoreFileErrorCode::ServerError,
            message: "test failure".into(),
        }
    }
    #[tokio::test]
    async fn sqlite_claim_fencing() {
        fencing("sqlite::memory:").await;
    }
    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL"]
    async fn postgres_claim_fencing() {
        fencing(&std::env::var("TEST_POSTGRES_URL").unwrap()).await;
    }
    #[allow(
        clippy::too_many_lines,
        reason = "verifies identical parent invalidation invariants on both SQL backends"
    )]
    async fn parents(database: &str) {
        let pool = create_pool_with_schema(Some(database)).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let service = FileSearchService::new(
            pool.clone(),
            Arc::new(reqwest::Client::new()),
            FileSearchConfig {
                files_storage_dir: Some(dir.path().into()),
                ..Default::default()
            },
        )
        .unwrap();
        let storage = FileSearchStorage::new(pool.clone());
        for scenario in ["file_expired", "store_expired", "file_deleted", "store_deleted"] {
            let store = service
                .create_vector_store(serde_json::from_value(json!({})).unwrap())
                .await
                .unwrap();
            let file = service
                .upload_file("parent.txt", "text/plain", "assistants", b"parent fencing".to_vec())
                .await
                .unwrap();
            let batch = service
                .create_file_batch(
                    &store.id,
                    serde_json::from_value(json!({"file_ids":[file.id]})).unwrap(),
                )
                .await
                .unwrap();
            let claim = storage.claim_next(30, 3, 1000).await.unwrap().unwrap();
            let mut object = service.get_vector_store_file(&store.id, &file.id).await.unwrap();
            object.status = AttachmentStatus::Completed;
            let prepared = PreparedAttachment {
                object,
                chunks: Vec::new(),
                dimensions: 0,
                parsed_content: "must not publish".into(),
            };
            match scenario {
                "file_expired" => {
                    sqlx::query("UPDATE file_search_files SET expires_at=1 WHERE id=$1")
                        .bind(&file.id)
                        .execute(pool.as_ref())
                        .await
                        .unwrap();
                }
                "store_expired" => {
                    sqlx::query("UPDATE file_search_stores SET expires_at=1 WHERE id=$1")
                        .bind(&store.id)
                        .execute(pool.as_ref())
                        .await
                        .unwrap();
                }
                "file_deleted" => {
                    service.delete_file(&file.id).await.unwrap();
                }
                "store_deleted" => {
                    service.delete_vector_store(&store.id).await.unwrap();
                }
                _ => unreachable!(),
            }
            assert_eq!(
                storage.finish_claim(&claim, Ok(prepared)).await.unwrap(),
                ClaimOutcome::LostClaim,
                "{scenario}"
            );
            assert_eq!(
                storage.renew_claim(&claim, 30).await.unwrap(),
                ClaimOutcome::LostClaim,
                "{scenario}"
            );
            service.cleanup_expired_files(1000).await.unwrap();
            service.cleanup_expired_vector_stores(1000).await.unwrap();
            let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE store_id=$1")
                .bind(&store.id)
                .fetch_one(pool.as_ref())
                .await
                .unwrap();
            assert_eq!(chunks, 0);
            if scenario.starts_with("file_") {
                assert_eq!(
                    service
                        .get_file_batch(&store.id, &batch.id)
                        .await
                        .unwrap()
                        .file_counts
                        .cancelled,
                    1
                );
                assert_eq!(
                    service
                        .list_file_batch_files(&store.id, &batch.id, &ListParams::default())
                        .await
                        .unwrap()
                        .data
                        .len(),
                    1
                );
                service.delete_vector_store(&store.id).await.unwrap();
            } else {
                assert!(
                    service.get_file(&file.id).await.is_ok(),
                    "store deletion preserves uploads"
                );
                service.delete_file(&file.id).await.unwrap();
                if scenario == "store_expired" {
                    service.delete_vector_store(&store.id).await.unwrap();
                }
            }
        }
    }
    #[tokio::test]
    async fn sqlite_parent_fencing() {
        parents("sqlite::memory:").await;
    }
    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL"]
    async fn postgres_parent_fencing() {
        parents(&std::env::var("TEST_POSTGRES_URL").unwrap()).await;
    }
    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL"]
    async fn postgres_job_lock_wait_rechecks_lease_before_publication() {
        let pool = create_pool_with_schema(Some(&std::env::var("TEST_POSTGRES_URL").unwrap()))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let service = FileSearchService::new(
            pool.clone(),
            Arc::new(reqwest::Client::new()),
            FileSearchConfig {
                files_storage_dir: Some(dir.path().into()),
                ..Default::default()
            },
        )
        .unwrap();
        let storage = FileSearchStorage::new(pool.clone());
        let store = service
            .create_vector_store(serde_json::from_value(json!({})).unwrap())
            .await
            .unwrap();
        let file = service
            .upload_file("contended.txt", "text/plain", "assistants", b"no stale lease".to_vec())
            .await
            .unwrap();
        service
            .create_file_batch(
                &store.id,
                serde_json::from_value(json!({"file_ids":[file.id]})).unwrap(),
            )
            .await
            .unwrap();
        let job = storage.claim_next(30, 3, 1000).await.unwrap().unwrap();
        let mut object = service.get_vector_store_file(&store.id, &file.id).await.unwrap();
        object.status = AttachmentStatus::Completed;
        let prepared = PreparedAttachment {
            object,
            chunks: Vec::new(),
            dimensions: 0,
            parsed_content: "stale lease".into(),
        };
        let mut held = pool.begin().await.unwrap();
        let now = database_now(&mut held).await.unwrap();
        sqlx::query("UPDATE file_search_jobs SET lease_until=$2 WHERE id=$1")
            .bind(&job.id.0)
            .bind(now + 1)
            .execute(&mut *held)
            .await
            .unwrap();
        let waiting = tokio::spawn(async move { storage.finish_claim(&job, Ok(prepared)).await });
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert!(!waiting.is_finished(), "publication must wait on the job row");
        held.commit().await.unwrap();
        assert_eq!(waiting.await.unwrap().unwrap(), ClaimOutcome::LostClaim);
        assert_eq!(
            service.get_vector_store_file(&store.id, &file.id).await.unwrap().status,
            AttachmentStatus::InProgress
        );
        service.delete_vector_store(&store.id).await.unwrap();
        service.delete_file(&file.id).await.unwrap();
    }
    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL"]
    async fn postgres_store_delete_waits_for_jobs_before_attachments() {
        let pool = create_pool_with_schema(Some(&std::env::var("TEST_POSTGRES_URL").unwrap()))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let service = FileSearchService::new(
            pool.clone(),
            Arc::new(reqwest::Client::new()),
            FileSearchConfig {
                files_storage_dir: Some(dir.path().into()),
                ..Default::default()
            },
        )
        .unwrap();
        let store = service
            .create_vector_store(serde_json::from_value(json!({})).unwrap())
            .await
            .unwrap();
        let file = service
            .upload_file("order.txt", "text/plain", "assistants", b"consistent deletion".to_vec())
            .await
            .unwrap();
        let batch = service
            .create_file_batch(
                &store.id,
                serde_json::from_value(json!({"file_ids":[file.id]})).unwrap(),
            )
            .await
            .unwrap();
        // Hold the source-deletion prefix: source -> job, before its attachment invalidation.
        let mut source = pool.begin().await.unwrap();
        sqlx::query("UPDATE file_search_files SET id=id WHERE id=$1")
            .bind(&file.id)
            .execute(&mut *source)
            .await
            .unwrap();
        sqlx::query("UPDATE file_search_jobs SET id=id WHERE batch_id=$1")
            .bind(&batch.id)
            .execute(&mut *source)
            .await
            .unwrap();
        let deleting = service.clone();
        let id = store.id.clone();
        let deletion = tokio::spawn(async move { deleting.delete_vector_store(&id).await });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!deletion.is_finished());
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            sqlx::query("UPDATE file_search_attachments SET status=status WHERE store_id=$1 AND file_id=$2")
                .bind(&store.id)
                .bind(&file.id)
                .execute(&mut *source),
        )
        .await
        .expect("store deletion must not own the attachment while waiting for the job")
        .unwrap();
        source.commit().await.unwrap();
        deletion.await.unwrap().unwrap();
        assert!(service.get_file(&file.id).await.is_ok());
        service.delete_file(&file.id).await.unwrap();
    }
}
