//! Background responses: `POST /v1/responses` with `background: true`.
//!
//! Admission stores the prepared request as a `queued` row owned by this gateway
//! instance and returns at once. A supervised job ([`crate::executor::engine`]'s
//! background driver) moves the row to `in_progress`, runs the same blocking
//! engine path as a foreground request, and completes the row inside the
//! engine's own persistence transaction. The database row is authoritative:
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
use crate::storage::models::background::Writer;
use crate::storage::response::ResponseWrite;
use crate::storage::{BackgroundStatus, ResponseMetadata};
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

/// Metadata for an active or failed background row: the snapshot plus the
/// request model. Continuation settings are written only when the turn finishes.
pub(crate) fn row_metadata(snapshot: &ResponsePayload) -> ResponseMetadata {
    ResponseMetadata {
        response_snapshot: Some(Box::new(snapshot.clone())),
        model: snapshot.model.clone(),
        previous_response_id: snapshot.previous_response_id.clone(),
        ..ResponseMetadata::default()
    }
}

/// Reject continuing from a background response that has not finished.
pub(crate) fn ensure_continuable(id: &str, snapshot: &ResponsePayload) -> Result<(), BackgroundError> {
    match status_of(snapshot) {
        Some(status) if !status.is_finished() => Err(BackgroundError::PreviousNotFinished {
            id: id.to_owned(),
            status,
        }),
        _ => Ok(()),
    }
}

/// Reject listing input items for a background response that stored no history.
pub(crate) fn ensure_listable(snapshot: &ResponsePayload) -> Result<(), BackgroundError> {
    match status_of(snapshot) {
        Some(status) if !status.is_finished() => Err(BackgroundError::InputItemsUnavailable(status)),
        _ => Ok(()),
    }
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

/// How a turn persisted by `owner`'s job writes its row: completing the
/// background row with the snapshot's terminal status. Without an owner, the
/// turn inserts a new row.
pub(crate) fn turn_write<'a>(owner: Option<&'a str>, metadata: &ResponseMetadata) -> ResponseWrite<'a> {
    let Some(owner) = owner else {
        return ResponseWrite::Insert;
    };
    let incomplete = metadata
        .response_snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.status == BackgroundStatus::Incomplete.as_str());
    let status = if incomplete {
        BackgroundStatus::Incomplete
    } else {
        BackgroundStatus::Completed
    };
    ResponseWrite::FinalizeBackground { owner, status }
}

/// What the engine needs to finish a turn as a background response.
#[derive(Debug, Clone)]
pub(crate) struct BackgroundRun {
    owner: Arc<str>,
    created_at: i64,
}

impl BackgroundRun {
    pub(crate) fn new(owner: Arc<str>, created_at: i64) -> Self {
        Self { owner, created_at }
    }

    pub(crate) fn owner(&self) -> &Arc<str> {
        &self.owner
    }

    /// Keep the admission identity on the terminal snapshot.
    pub(crate) fn apply(&self, payload: &mut ResponsePayload) {
        payload.created_at = self.created_at;
        payload.standard_fields.background = true;
    }
}

#[cfg(test)]
mod tests;
