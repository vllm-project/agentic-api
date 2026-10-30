//! Background lifecycle operations on stored responses, for `executor/background`.

use super::ResponseHandler;
use crate::executor::background::row_metadata;
use crate::executor::error::ExecutorResult;
use crate::storage::BackgroundStatus;
use crate::storage::models::background::Writer;
use crate::types::request_response::ResponsePayload;

impl ResponseHandler {
    /// The stored snapshot of `response_id`, or `None` for a record without one.
    pub(crate) async fn snapshot(&self, response_id: &str) -> ExecutorResult<Option<ResponsePayload>> {
        let stored = self.store.get(response_id).await?;
        Ok(stored.metadata.response_snapshot.map(|snapshot| *snapshot))
    }

    /// Store `snapshot` as a `queued` background response owned by `owner`.
    pub(crate) async fn enqueue_background(
        &self,
        snapshot: &ResponsePayload,
        owner: &str,
        lease_expires_at: i64,
    ) -> ExecutorResult<()> {
        let metadata = row_metadata(snapshot);
        self.store
            .enqueue_background(&snapshot.id, &metadata, snapshot.created_at, owner, lease_expires_at)
            .await?;
        Ok(())
    }

    /// Move the active background row `response_id` to `to` with `snapshot`, if
    /// `writer` may. Returns whether the row moved.
    pub(crate) async fn transition_background(
        &self,
        response_id: &str,
        transition: (&[BackgroundStatus], BackgroundStatus),
        writer: Writer<'_>,
        snapshot: &ResponsePayload,
    ) -> ExecutorResult<bool> {
        let (from, to) = transition;
        let metadata = row_metadata(snapshot);
        Ok(self
            .store
            .transition_background(response_id, from, writer, to, &metadata)
            .await?)
    }

    /// Extend an owned active row's lease. Returns `false` once the owner lost it.
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
    pub(crate) async fn background_lease(&self, response_id: &str) -> ExecutorResult<Option<i64>> {
        Ok(self.store.background_lease(response_id).await?)
    }

    /// Up to `limit` active background rows whose lease expired before `now`.
    pub(crate) async fn expired_background(&self, now: i64, limit: i64) -> ExecutorResult<Vec<String>> {
        Ok(self.store.expired_background(now, limit).await?)
    }
}
