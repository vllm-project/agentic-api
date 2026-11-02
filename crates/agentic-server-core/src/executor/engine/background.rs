//! The background driver: stores a prepared turn as `queued`, then supervises
//! the job that runs it on the blocking engine path.
//!
//! The supervisor owns the job's lifecycle, not its execution: it waits for a
//! run slot, claims the row, runs [`super::run_blocking`] on a tracked task,
//! renews the row's lease, and stops the task on cancellation, shutdown,
//! timeout, or a lost lease. Stopping aborts the task and joins it, which drops
//! its in-flight upstream request and gateway tool calls before the supervisor
//! records the outcome.

use std::str::FromStr;
use std::sync::Arc;

use tokio::task::JoinError;
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tracing::{Instrument as _, warn};

use super::execute::prepare;
use super::run_blocking;
use super::streaming::ResponseShell;
use crate::executor::background::{
    Admission, BackgroundError, BackgroundJob, BackgroundRun, validate_request, with_status,
};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::inference::UpstreamHeaders;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::telemetry::ExecutionSpan;
use crate::storage::models::background::Writer;
use crate::storage::{BackgroundStatus, StorageError};
use crate::tool::ToolSearchState;
use crate::types::request_response::{RequestPayload, ResponsePayload};

/// A `background: true` request as [`super::ExecuteRequest::run_background`] hands it over.
pub(super) struct BackgroundRequest {
    pub payload: RequestPayload,
    pub exec_ctx: Arc<ExecutionContext>,
    pub client_auth: Option<UpstreamHeaders>,
    /// The request is attached to a transient session, which background mode does not continue.
    pub in_session: bool,
}

/// A prepared turn and what its job needs to run it.
struct BackgroundTurn {
    ctx: RequestContext,
    tools: Option<ToolSearchState>,
    exec_ctx: Arc<ExecutionContext>,
    client_auth: Option<UpstreamHeaders>,
}

/// Admit and prepare a background request, store it as `queued`, and start its job.
///
/// # Errors
/// Returns a [`BackgroundError`] for an option background mode does not run or
/// for full capacity, the request's preparation errors, or the storage error
/// that kept the row from being stored. No job starts on any error.
pub(super) async fn run(request: BackgroundRequest, execution: ExecutionSpan) -> ExecutorResult<ResponsePayload> {
    match Box::pin(admit(request)).await {
        Ok((admission, turn)) => enqueue(admission, turn, execution).await,
        Err(error) => Err(not_started(execution, error)),
    }
}

/// Record that the request ends with `error` before any job started.
fn not_started(mut execution: ExecutionSpan, error: ExecutorError) -> ExecutorError {
    execution.failed(&error);
    execution.not_delivered();
    error
}

/// Reserve capacity, then prepare the turn exactly as a foreground request.
async fn admit(request: BackgroundRequest) -> ExecutorResult<(Admission, BackgroundTurn)> {
    if request.in_session {
        return Err(BackgroundError::SessionContinuation.into());
    }
    validate_request(&request.payload)?;
    let admission = request.exec_ctx.background_jobs().admit()?;
    let (ctx, tools) = prepare(request.payload, &request.exec_ctx, None, false).await?;
    // Multi-agent settings can also arrive with the continued response.
    validate_request(&ctx.enriched_request)?;
    let turn = BackgroundTurn {
        ctx,
        tools,
        exec_ctx: request.exec_ctx,
        client_auth: request.client_auth,
    };
    Ok((admission, turn))
}

/// Store the prepared turn as `queued` and spawn its supervisor.
async fn enqueue(
    admission: Admission,
    turn: BackgroundTurn,
    execution: ExecutionSpan,
) -> ExecutorResult<ResponsePayload> {
    let ctx = &turn.ctx;
    let mut queued = ResponseShell::from(ctx).payload(BackgroundStatus::Queued.as_str());
    queued.standard_fields.background = true;
    // The shell's declarations are stripped of request-scoped credentials before this row is stored.
    queued.tool_choice = ctx.public_tool_choice().cloned();
    let job = admission.register(ctx.response_id.clone());
    if let Err(error) = job
        .responses()
        .enqueue_background(&queued, job.owner(), job.lease_deadline())
        .await
    {
        return Err(not_started(execution, error));
    }
    let supervisor = Supervisor {
        snapshot: queued.clone(),
        turn: Some(turn),
        execution,
    };
    job.spawn_supervisor(|job| supervisor.run(job));
    Ok(queued)
}

/// Why a job stopped.
enum Stop {
    /// The task returned or panicked.
    Finished(Box<Result<ExecutorResult<ResponsePayload>, JoinError>>),
    /// A client cancelled or deleted the row, which records the outcome itself.
    Cancelled,
    /// The instance is shutting down.
    Shutdown,
    TimedOut,
    /// Renewal found the row no longer owned and active: another writer finished it.
    Lost,
}

struct Supervisor {
    /// The row's current snapshot, from which every later snapshot is derived.
    snapshot: ResponsePayload,
    turn: Option<BackgroundTurn>,
    execution: ExecutionSpan,
}

impl Supervisor {
    async fn run(mut self, job: BackgroundJob) {
        let span = self.execution.span().clone();
        async move {
            let stop = self.drive(&job).await;
            self.finish(&job, stop).await;
        }
        .instrument(span)
        .await;
    }

    async fn drive(&mut self, job: &BackgroundJob) -> Stop {
        let renew_every = job.timing().renew_every;
        let mut renewals = tokio::time::interval_at(Instant::now() + renew_every, renew_every);
        renewals.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let slot = match guarded(job, &mut renewals, job.run_slot()).await {
            Ok(Some(slot)) => slot,
            Ok(None) => return Stop::Shutdown,
            Err(stop) => return stop,
        };
        let started = with_status(self.snapshot.clone(), BackgroundStatus::InProgress, None);
        let claimed = job
            .responses()
            .transition_background(
                job.response_id(),
                (&[BackgroundStatus::Queued], BackgroundStatus::InProgress),
                Writer::Owner(job.owner()),
                &started,
            )
            .await;
        match claimed {
            Ok(true) => self.snapshot = started,
            // Cancelled while queued: the model is never called.
            Ok(false) => return Stop::Lost,
            Err(error) => return Stop::Finished(Box::new(Ok(Err(error)))),
        }
        let Some(BackgroundTurn {
            ctx,
            tools,
            exec_ctx,
            client_auth,
        }) = self.turn.take()
        else {
            return Stop::Lost;
        };
        let run = BackgroundRun::new(Arc::clone(job.owner()), self.snapshot.created_at);
        let mut task = job.spawn(
            async move { Box::pin(run_blocking(ctx, tools, &exec_ctx, client_auth.as_ref(), Some(&run))).await }
                .in_current_span(),
        );
        let stop = match guarded(job, &mut renewals, tokio::time::timeout(job.timeout(), &mut task)).await {
            Ok(Ok(joined)) => return Stop::Finished(Box::new(joined)),
            Ok(Err(_elapsed)) => Stop::TimedOut,
            Err(stop) => stop,
        };
        task.abort();
        // Join the aborted task so its upstream request and tool calls are gone before the row changes.
        let _ = task.await;
        drop(slot);
        stop
    }

    async fn finish(mut self, job: &BackgroundJob, stop: Stop) {
        let failure = match stop {
            Stop::Finished(finished) => match *finished {
                Ok(Ok(payload)) => {
                    if BackgroundStatus::from_str(&payload.status).is_ok_and(BackgroundStatus::is_finished) {
                        // The engine stored the terminal row in its own persistence transaction.
                        self.execution.completed_with_status(&payload.status);
                        self.execution.delivered();
                        return;
                    }
                    // A status the engine does not store, such as an upstream `failed` response.
                    self.execution.completed_with_status(&payload.status);
                    let mut failed = with_status(self.snapshot.clone(), BackgroundStatus::Failed, None);
                    failed.error = payload.error;
                    return self.record(job, failed).await;
                }
                Ok(Err(ExecutorError::Storage(StorageError::BackgroundClaimLost { .. }))) => {
                    self.execution.cancelled();
                    self.execution.not_delivered();
                    return;
                }
                Ok(Err(error)) => {
                    self.execution.failed(&error);
                    error
                }
                Err(_join_error) => {
                    let error = BackgroundError::Panicked.into();
                    self.execution.failed(&error);
                    error
                }
            },
            Stop::Cancelled | Stop::Lost => {
                self.execution.cancelled();
                self.execution.not_delivered();
                return;
            }
            Stop::Shutdown => {
                self.execution.cancelled();
                BackgroundError::Interrupted.into()
            }
            Stop::TimedOut => {
                let error = BackgroundError::TimedOut(job.timeout().as_secs()).into();
                self.execution.failed(&error);
                error
            }
        };
        let failed = with_status(self.snapshot.clone(), BackgroundStatus::Failed, Some(&failure));
        self.record(job, failed).await;
    }

    /// Record a `failed` snapshot unless another writer finished the row first.
    async fn record(&mut self, job: &BackgroundJob, failed: ResponsePayload) {
        let recorded = job
            .responses()
            .transition_background(
                job.response_id(),
                (&BackgroundStatus::ACTIVE, BackgroundStatus::Failed),
                Writer::Owner(job.owner()),
                &failed,
            )
            .await;
        match recorded {
            Ok(true) => self.execution.delivered(),
            Ok(false) => self.execution.not_delivered(),
            Err(error) => {
                warn!(response_id = job.response_id(), error = %error, "cannot record a failed background response");
                self.execution.not_delivered();
            }
        }
    }
}

/// Await `future` while renewing the job's lease, stopping on cancellation or a lost lease.
async fn guarded<F: Future>(job: &BackgroundJob, renewals: &mut Interval, future: F) -> Result<F::Output, Stop> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            output = &mut future => return Ok(output),
            () = job.token().cancelled() => {
                return Err(if job.shutting_down() { Stop::Shutdown } else { Stop::Cancelled });
            }
            _ = renewals.tick() => {
                match job.responses().renew_background(job.response_id(), job.owner(), job.lease_deadline()).await {
                    Ok(true) => {}
                    Ok(false) => return Err(Stop::Lost),
                    // Keep running: the next renewal retries before the lease expires.
                    Err(error) => warn!(response_id = job.response_id(), error = %error, "cannot renew a background lease"),
                }
            }
        }
    }
}
