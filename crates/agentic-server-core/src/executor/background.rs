//! Background responses: `POST /v1/responses` with `background: true`.
//!
//! Admission stores the prepared turn as a `queued` row owned by this gateway
//! instance, with the request's items as a finished turn stores them, and returns
//! at once. A supervised job ([`crate::executor::engine`]'s background driver)
//! moves the row to `in_progress`, runs the same blocking engine path as a
//! foreground request, and finishes the row through the engine's own persistence
//! path, appending the turn's output. The database row is authoritative:
//! every status change is a conditional update ([`crate::storage::BackgroundStatus`]
//! documents the states), so a cancellation, the job, and recovery after a lost
//! owner race safely across gateway instances that share one database.
//!
//! This module owns admission, ownership leases, cancellation, recovery, and
//! shutdown. It never runs inference itself.

mod error;
mod jobs;

use std::str::FromStr;
use std::sync::Arc;

pub use error::BackgroundError;
pub use jobs::BackgroundJobs;
#[cfg(test)]
pub(crate) use jobs::LeaseTiming;
pub(crate) use jobs::{Admission, BackgroundJob};

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::modes::ResponseHandler;
use crate::storage::BackgroundStatus;
use crate::storage::models::background::Writer;
use crate::types::request_response::{RequestPayload, ResponsePayload};

/// Wall-clock milliseconds, the unit of stored lease expiries. Instances that
/// share a database compare these, so their clocks must agree to well within a lease.
pub(crate) fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Reject request options background mode does not run in this version.
pub(crate) fn validate_request(request: &RequestPayload) -> Result<(), BackgroundError> {
    if !request.store {
        return Err(BackgroundError::RequiresStore);
    }
    if request.stream {
        return Err(BackgroundError::Streaming);
    }
    if request.multi_agent.as_ref().is_some_and(|config| config.enabled) {
        return Err(BackgroundError::MultiAgent);
    }
    Ok(())
}

/// The lifecycle status of a stored background snapshot; `None` for a foreground response.
pub(crate) fn status_of(snapshot: &ResponsePayload) -> Option<BackgroundStatus> {
    snapshot
        .standard_fields
        .background
        .then(|| BackgroundStatus::from_str(&snapshot.status).ok())
        .flatten()
}

/// A background snapshot moved to `status`, keeping its identity and request fields.
pub(crate) fn with_status(
    mut snapshot: ResponsePayload,
    status: BackgroundStatus,
    error: Option<&ExecutorError>,
) -> ResponsePayload {
    status.as_str().clone_into(&mut snapshot.status);
    snapshot.error = error.map(ExecutorError::failed_response_error);
    snapshot
}

/// Fail an active background row whose owner stopped renewing its lease
/// before `now`. Returns the failed snapshot when this call failed the row.
pub(crate) async fn expire_if_stale(
    responses: &ResponseHandler,
    id: &str,
    snapshot: &ResponsePayload,
    now: i64,
) -> ExecutorResult<Option<ResponsePayload>> {
    if !status_of(snapshot).is_some_and(BackgroundStatus::is_active) {
        return Ok(None);
    }
    if responses
        .background_lease(id)
        .await?
        .is_none_or(|expires_at| expires_at >= now)
    {
        return Ok(None);
    }
    let failed = with_status(
        snapshot.clone(),
        BackgroundStatus::Failed,
        Some(&BackgroundError::Interrupted.into()),
    );
    let moved = responses
        .transition_background(
            id,
            (&BackgroundStatus::ACTIVE, BackgroundStatus::Failed),
            Writer::Recovery { now },
            &failed,
        )
        .await?;
    Ok(moved.then_some(failed))
}

/// What the engine needs to finish a turn as a background response.
#[derive(Debug, Clone)]
pub(crate) struct BackgroundRun {
    owner: Arc<str>,
    created_at: i64,
    queued_items: usize,
}

impl BackgroundRun {
    /// `queued_items` counts the request items the turn had when it was queued, which its row already holds.
    pub(crate) fn new(owner: Arc<str>, created_at: i64, queued_items: usize) -> Self {
        Self {
            owner,
            created_at,
            queued_items,
        }
    }

    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }

    pub(crate) fn queued_items(&self) -> usize {
        self.queued_items
    }

    /// Keep the admission identity on the terminal snapshot.
    pub(crate) fn apply(&self, payload: &mut ResponsePayload) {
        payload.created_at = self.created_at;
        payload.standard_fields.background = true;
    }
}

#[cfg(test)]
mod tests;
