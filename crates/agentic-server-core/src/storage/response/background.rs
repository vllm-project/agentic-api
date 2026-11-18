//! Background lifecycle operations on stored responses.

use super::super::models::background::{self, FinishedRow, Writer};
use super::super::models::{conversation, item, response};
use super::super::types::item::encode;
use super::super::types::{
    BackgroundStatus, QueuedTurn, ResponseCheckpoint, ResponseMetadata, StorageError, StoreResult,
};
use super::ResponseStore;
use crate::types::io::InOutItem;
use crate::types::request_response::ResponsePayload;
use crate::utils::common::serialize_to_string;

impl ResponseStore {
    /// Store a background turn as `queued` for `claim`'s owner, as [`Self::persist`] stores a finished turn: the
    /// request's items extend `parent`'s history, so they are listed and continued while the turn runs.
    ///
    /// # Errors
    /// As [`Self::persist`].
    pub(crate) async fn enqueue_background(
        &self,
        parent: Option<&ResponseCheckpoint>,
        turn: QueuedTurn<'_>,
    ) -> StoreResult<Vec<String>> {
        let QueuedTurn {
            response_id,
            items,
            snapshot,
            metadata,
            claim,
        } = turn;
        self.insert(response_id, parent, items, Some(snapshot), metadata, Some(claim))
            .await
    }

    /// Finish the `in_progress` background row `owner` holds with `status`: append `new_items` to the history the
    /// row holds now, so a deletion while the turn ran is kept, and store the turn's snapshot and metadata.
    ///
    /// # Errors
    /// [`StorageError::BackgroundClaimLost`] when the row is no longer owned and in progress, with nothing stored;
    /// otherwise a database or encoding error.
    pub(crate) async fn finish_background(
        &self,
        response_id: &str,
        owner: &str,
        status: BackgroundStatus,
        new_items: Vec<InOutItem>,
        snapshot: &ResponsePayload,
        metadata: &ResponseMetadata,
    ) -> StoreResult<Vec<String>> {
        let new_items = new_items.iter().map(encode).collect::<StoreResult<Vec<_>>>()?;
        let snapshot_json = String::try_from(snapshot)?;
        let metadata_json = String::try_from(metadata)?;
        let lost = || StorageError::BackgroundClaimLost {
            response_id: response_id.to_owned(),
        };

        let mut tx = self.pool()?.begin().await?;
        // Dropping the transaction rolls back anything it wrote.
        if !background::hold_in_tx(&mut tx, response_id, owner).await? {
            return Err(lost());
        }
        let row = response::get(&mut *tx, response_id).await?.ok_or_else(lost)?;
        let mut item_ids = row
            .history_item_ids_vec()
            .map_err(|_| StorageError::InvalidResponseHistory {
                response_id: response_id.to_owned(),
            })?;
        if let Some(conversation_id) = row.conversation_id.as_deref() {
            // Conversation item writers share this lock, so the turn's items get the next sequence numbers.
            conversation::lock_in_tx(&mut tx, conversation_id).await?;
        }
        let created = item::create_in_tx(&mut tx, new_items, row.conversation_id.as_deref()).await?;
        item_ids.extend(created.into_iter().map(|item| item.id));
        let history_item_ids_json = serialize_to_string(&item_ids)?;
        let finished = FinishedRow {
            id: response_id,
            history_item_ids: &history_item_ids_json,
            snapshot: &snapshot_json,
            metadata: &metadata_json,
        };
        background::finish_in_tx(&mut tx, finished, status).await?;
        tx.commit().await?;
        Ok(item_ids)
    }

    /// Move an active background row to `to` with `snapshot`, if `writer` may.
    /// Returns whether the row moved.
    ///
    /// # Errors
    /// Returns an error if the snapshot cannot be encoded or the update fails.
    pub(crate) async fn transition_background(
        &self,
        id: &str,
        from: &[BackgroundStatus],
        writer: Writer<'_>,
        to: BackgroundStatus,
        snapshot: &ResponsePayload,
    ) -> StoreResult<bool> {
        let snapshot = String::try_from(snapshot)?;
        Ok(background::transition(self.pool()?, id, from, writer, to, &snapshot).await?)
    }

    /// Extend an owned active row's lease. Returns `false` once the owner lost it.
    ///
    /// # Errors
    /// Returns an error if the update fails.
    pub(crate) async fn renew_background(&self, id: &str, owner: &str, lease_expires_at: i64) -> StoreResult<bool> {
        Ok(background::renew(self.pool()?, id, owner, lease_expires_at).await?)
    }

    /// The lease expiry of an active background row.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub(crate) async fn background_lease(&self, id: &str) -> StoreResult<Option<i64>> {
        Ok(background::active_lease(self.pool()?, id).await?)
    }

    /// Up to `limit` active background rows whose lease expired before `now`.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub(crate) async fn expired_background(&self, now: i64, limit: i64) -> StoreResult<Vec<String>> {
        Ok(background::expired(self.pool()?, now, limit).await?)
    }
}
