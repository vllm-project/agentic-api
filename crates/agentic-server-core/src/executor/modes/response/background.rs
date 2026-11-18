//! Background lifecycle operations on stored responses, for `executor/background`.

use super::ResponseHandler;
use crate::executor::error::ExecutorResult;
use crate::executor::request::RequestContext;
use crate::storage::models::background::Writer;
use crate::storage::{BackgroundStatus, FinishedTurn, QueuedTurn};
use crate::types::request_response::ResponsePayload;

impl ResponseHandler {
    /// The stored snapshot of `response_id`, or `None` for a record without one.
    ///
    /// # Errors
    /// Returns not found for an unknown ID, or a storage error.
    pub(crate) async fn snapshot(&self, response_id: &str) -> ExecutorResult<Option<ResponsePayload>> {
        Ok(self.store.get_snapshot(response_id).await?)
    }

    /// Stores a background turn as `queued`, extending the history rehydration captured in `ctx.checkpoint`.
    ///
    /// # Errors
    /// Returns a storage error when the turn cannot be stored.
    pub(crate) async fn enqueue_background(&self, ctx: &RequestContext, turn: QueuedTurn<'_>) -> ExecutorResult<()> {
        let parent = ctx.checkpoint.as_ref().filter(|checkpoint| checkpoint.durable);
        self.store.enqueue_background(parent, turn).await?;
        Ok(())
    }

    /// Finishes the `in_progress` row `owner` holds with `turn`, appending its items to the row's history.
    ///
    /// # Errors
    /// [`crate::storage::StorageError::BackgroundClaimLost`] when another writer finished the row first, or a storage
    /// error.
    pub(crate) async fn finish_background(
        &self,
        ctx: &RequestContext,
        owner: &str,
        turn: FinishedTurn<'_>,
    ) -> ExecutorResult<Vec<String>> {
        Ok(self.store.finish_background(&ctx.response_id, owner, turn).await?)
    }

    /// Moves the active background row `response_id` to `to` with `snapshot`, if `writer` may. Returns whether the
    /// row moved.
    ///
    /// # Errors
    /// Returns a storage error.
    pub(crate) async fn transition_background(
        &self,
        response_id: &str,
        transition: (&[BackgroundStatus], BackgroundStatus),
        writer: Writer<'_>,
        snapshot: &ResponsePayload,
    ) -> ExecutorResult<bool> {
        let (from, to) = transition;
        Ok(self
            .store
            .transition_background(response_id, from, writer, to, snapshot)
            .await?)
    }

    /// Extends an owned active row's lease. Returns `false` once the owner lost it.
    ///
    /// # Errors
    /// Returns a storage error.
    pub(crate) async fn renew_background(
        &self,
        response_id: &str,
        owner: &str,
        lease_expires_at: i64,
    ) -> ExecutorResult<bool> {
        Ok(self
            .store
            .renew_background(response_id, owner, lease_expires_at)
            .await?)
    }

    /// The lease expiry of an active background row.
    ///
    /// # Errors
    /// Returns a storage error.
    pub(crate) async fn background_lease(&self, response_id: &str) -> ExecutorResult<Option<i64>> {
        Ok(self.store.background_lease(response_id).await?)
    }

    /// Up to `limit` active background rows whose lease expired before `now`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub(crate) async fn expired_background(&self, now: i64, limit: i64) -> ExecutorResult<Vec<String>> {
        Ok(self.store.expired_background(now, limit).await?)
    }
}
