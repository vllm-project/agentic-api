//! Admission, ownership, cancellation, recovery, and shutdown of background jobs.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::warn;

use super::{BackgroundError, expire_if_stale, now_millis, status_of, with_status};
use crate::config::BackgroundConfig;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::modes::ResponseHandler;
use crate::storage::BackgroundStatus;
use crate::storage::models::background::Writer;
use crate::types::request_response::ResponsePayload;
use crate::utils::common::uuid7_str;

/// How long a row stays owned without renewal, and how often its job renews it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LeaseTiming {
    pub lease: Duration,
    pub renew_every: Duration,
}

impl Default for LeaseTiming {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(30),
            renew_every: Duration::from_secs(10),
        }
    }
}

/// Expired rows recovery reads per query.
pub(super) const RECOVERY_BATCH: i64 = 100;

/// Background jobs of one gateway instance.
///
/// Clones share one set of jobs. Admission takes a permit from a semaphore of
/// `max_concurrent + max_queued` permits without waiting, so a full instance
/// answers `503` instead of buffering requests. Each admitted job then waits for
/// one of `max_concurrent` run permits while it is `queued`.
#[derive(Clone)]
pub struct BackgroundJobs {
    inner: Arc<Inner>,
}

struct Inner {
    config: BackgroundConfig,
    timing: LeaseTiming,
    owner: Arc<str>,
    responses: ResponseHandler,
    max_admitted: usize,
    admitted: Arc<Semaphore>,
    running: Arc<Semaphore>,
    /// Cancellation of this instance's own jobs by response ID. Locked only for
    /// map operations, never across an await.
    registry: Mutex<HashMap<String, CancellationToken>>,
    shutdown: CancellationToken,
    tasks: TaskTracker,
}

impl fmt::Debug for BackgroundJobs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackgroundJobs")
            .field("owner", &self.inner.owner)
            .field("config", &self.inner.config)
            .field("available", &self.inner.admitted.available_permits())
            .finish_non_exhaustive()
    }
}

impl BackgroundJobs {
    /// Jobs that store through `responses`, with limits validated by configuration.
    pub(crate) fn new(responses: ResponseHandler, config: BackgroundConfig) -> Self {
        Self::with_timing(responses, config, LeaseTiming::default())
    }

    pub(crate) fn with_timing(responses: ResponseHandler, config: BackgroundConfig, timing: LeaseTiming) -> Self {
        // Configuration validation rejects these bounds; clamp so an unvalidated one cannot panic.
        let running = config.max_concurrent.get().min(Semaphore::MAX_PERMITS);
        let admitted = config.max_admitted().unwrap_or(running);
        Self {
            inner: Arc::new(Inner {
                config,
                timing,
                owner: uuid7_str("bgo_").into(),
                responses,
                max_admitted: admitted,
                admitted: Arc::new(Semaphore::new(admitted)),
                running: Arc::new(Semaphore::new(running)),
                registry: Mutex::new(HashMap::new()),
                shutdown: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        }
    }

    /// Reserve capacity for one job, or reject without waiting.
    pub(crate) fn admit(&self) -> Result<Admission, BackgroundError> {
        match Arc::clone(&self.inner.admitted).try_acquire_owned() {
            Ok(permit) => Ok(Admission {
                jobs: self.clone(),
                permit,
            }),
            Err(TryAcquireError::NoPermits) => Err(BackgroundError::CapacityExceeded),
            Err(TryAcquireError::Closed) => Err(BackgroundError::ShuttingDown),
        }
    }

    /// Cancel a background response on any instance. Cancelling a cancelled
    /// response returns it again.
    ///
    /// # Errors
    /// Returns not found for an unknown ID, [`BackgroundError::NotBackground`]
    /// for a foreground response, [`BackgroundError::NotCancellable`] for any
    /// other terminal status, or a storage error.
    pub async fn cancel(&self, response_id: &str) -> ExecutorResult<ResponsePayload> {
        // A failed update means another writer finished the row first; the next read reports it.
        for _ in 0..2 {
            let snapshot = self
                .inner
                .responses
                .snapshot(response_id)
                .await?
                .ok_or(BackgroundError::NotBackground)?;
            let status = status_of(&snapshot).ok_or(BackgroundError::NotBackground)?;
            match status {
                BackgroundStatus::Cancelled => return Ok(snapshot),
                BackgroundStatus::Queued | BackgroundStatus::InProgress => {
                    let cancelled = with_status(snapshot, BackgroundStatus::Cancelled, None);
                    let moved = self
                        .inner
                        .responses
                        .transition_background(
                            response_id,
                            (&BackgroundStatus::ACTIVE, BackgroundStatus::Cancelled),
                            Writer::Client,
                            &cancelled,
                        )
                        .await?;
                    if moved {
                        self.stop_local(response_id);
                        return Ok(cancelled);
                    }
                }
                finished => return Err(BackgroundError::NotCancellable(finished).into()),
            }
        }
        Err(ExecutorError::Conflict(format!(
            "background response '{response_id}' changed while it was being cancelled"
        )))
    }

    /// Stop this instance's job for `response_id`, if it runs one. The caller
    /// has already changed or removed the row; the job writes nothing more.
    pub fn stop_local(&self, response_id: &str) {
        let token = self
            .inner
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(response_id)
            .cloned();
        if let Some(token) = token {
            token.cancel();
        }
    }

    /// Fail every active row whose owner stopped renewing its lease, as at startup.
    /// Rows of live instances, including this one, keep running.
    ///
    /// # Errors
    /// Returns a storage error when the expired rows cannot be read or failed.
    pub async fn recover(&self) -> ExecutorResult<usize> {
        self.recover_at(now_millis()).await
    }

    pub(crate) async fn recover_at(&self, now: i64) -> ExecutorResult<usize> {
        let responses = &self.inner.responses;
        let mut failed = 0;
        loop {
            let ids = responses.expired_background(now, RECOVERY_BATCH).await?;
            let mut progressed = false;
            for id in &ids {
                let snapshot = match responses.snapshot(id).await {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        warn!(response_id = %id, error = %error, "cannot read an expired background response");
                        continue;
                    }
                };
                let Some(snapshot) = snapshot else { continue };
                if expire_if_stale(responses, id, &snapshot, now).await?.is_some() {
                    failed += 1;
                    progressed = true;
                }
            }
            // A full batch that failed nothing would repeat itself.
            if !progressed || i64::try_from(ids.len()).is_ok_and(|read| read < RECOVERY_BATCH) {
                return Ok(failed);
            }
        }
    }

    /// Stop admitting and cancel every job. Each job records its row `failed`.
    pub fn begin_shutdown(&self) {
        self.inner.admitted.close();
        self.inner.running.close();
        self.inner.shutdown.cancel();
        self.inner.tasks.close();
    }

    /// [`Self::begin_shutdown`], then wait until every job has recorded its outcome.
    pub async fn shutdown(&self) {
        self.begin_shutdown();
        self.inner.tasks.wait().await;
    }

    /// Jobs admitted and not yet finished, running or queued.
    #[must_use]
    pub fn active(&self) -> usize {
        self.inner
            .max_admitted
            .saturating_sub(self.inner.admitted.available_permits())
    }
}

/// Capacity reserved for one job before its row exists.
pub(crate) struct Admission {
    jobs: BackgroundJobs,
    permit: OwnedSemaphorePermit,
}

impl Admission {
    /// Register the job for `response_id`, so cancellation and shutdown reach it.
    pub(crate) fn register(self, response_id: String) -> BackgroundJob {
        let token = self.jobs.inner.shutdown.child_token();
        self.jobs
            .inner
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(response_id.clone(), token.clone());
        BackgroundJob {
            jobs: self.jobs,
            response_id,
            token,
            _admitted: self.permit,
        }
    }
}

/// One registered job. Dropping it releases its capacity and registration.
pub(crate) struct BackgroundJob {
    jobs: BackgroundJobs,
    response_id: String,
    token: CancellationToken,
    _admitted: OwnedSemaphorePermit,
}

impl BackgroundJob {
    pub(crate) fn response_id(&self) -> &str {
        &self.response_id
    }

    pub(crate) fn owner(&self) -> &Arc<str> {
        &self.jobs.inner.owner
    }

    pub(crate) fn responses(&self) -> &ResponseHandler {
        &self.jobs.inner.responses
    }

    pub(crate) fn token(&self) -> &CancellationToken {
        &self.token
    }

    pub(crate) fn timing(&self) -> LeaseTiming {
        self.jobs.inner.timing
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.jobs.inner.config.timeout
    }

    /// Whether the instance is shutting down rather than this job being cancelled.
    pub(crate) fn shutting_down(&self) -> bool {
        self.jobs.inner.shutdown.is_cancelled()
    }

    /// The lease expiry for a renewal made now.
    pub(crate) fn lease_deadline(&self) -> i64 {
        let lease = i64::try_from(self.timing().lease.as_millis()).unwrap_or(i64::MAX);
        now_millis().saturating_add(lease)
    }

    /// Wait for a run slot. Returns `None` once the instance shuts down.
    pub(crate) async fn run_slot(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.jobs.inner.running).acquire_owned().await.ok()
    }

    /// Run `supervise` for this job on a task the instance waits for at shutdown.
    pub(crate) fn spawn_supervisor<F, S>(self, supervise: S)
    where
        S: FnOnce(Self) -> F,
        F: Future<Output = ()> + Send + 'static,
    {
        let tasks = self.jobs.inner.tasks.clone();
        tasks.spawn(supervise(self));
    }

    /// Spawn a task the instance waits for at shutdown.
    pub(crate) fn spawn<F>(&self, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.jobs.inner.tasks.spawn(task)
    }
}

impl Drop for BackgroundJob {
    fn drop(&mut self) {
        // Response IDs are unique, so the entry under this ID is this job's.
        self.jobs
            .inner
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.response_id);
    }
}
