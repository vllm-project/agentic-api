//! Background lifecycle operations on stored responses.

use super::super::models::background::{self, TurnRow, Writer};
use super::super::models::response;
use super::super::pool::DbTransaction;
use super::super::types::{BackgroundStatus, ResponseMetadata, StorageError, StoreResult};
use super::ResponseStore;

/// How a persisted turn writes its `responses` row.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) enum ResponseWrite<'a> {
    /// Insert a new row: every foreground turn.
    #[default]
    Insert,
    /// Complete the `in_progress` background row that `owner` holds.
    FinalizeBackground { owner: &'a str, status: BackgroundStatus },
}

/// Write a finished turn's row inside its persistence transaction. A background
/// row that is no longer owned and in progress fails the transaction, so the
/// caller's item inserts roll back with it.
///
/// # Errors
/// Returns [`StorageError::BackgroundClaimLost`] for a lost background row, or a database error.
pub(crate) async fn write_turn_in_tx(
    tx: &mut DbTransaction<'_>,
    write: ResponseWrite<'_>,
    row: TurnRow<'_>,
) -> StoreResult<()> {
    match write {
        ResponseWrite::Insert => {
            response::create_in_tx(
                tx,
                row.id,
                row.conversation_id,
                row.previous_response_id,
                Some(row.history_item_ids),
                Some(row.metadata),
            )
            .await?;
            Ok(())
        }
        ResponseWrite::FinalizeBackground { owner, status } => {
            if background::finalize_in_tx(tx, row, owner, status).await? {
                Ok(())
            } else {
                Err(StorageError::BackgroundClaimLost {
                    response_id: row.id.to_owned(),
                })
            }
        }
    }
}

impl ResponseStore {
    /// Store a `queued` background response owned by `owner`.
    pub(crate) async fn enqueue_background(
        &self,
        id: &str,
        metadata: &ResponseMetadata,
        created_at: i64,
        owner: &str,
        lease_expires_at: i64,
    ) -> StoreResult<()> {
        let metadata = String::try_from(metadata)?;
        background::insert_queued(self.pool()?, id, &metadata, created_at, owner, lease_expires_at).await?;
        Ok(())
    }

    /// Move an active background row to `to` with `metadata`, if `writer` may.
    /// Returns whether the row moved.
    pub(crate) async fn transition_background(
        &self,
        id: &str,
        from: &[BackgroundStatus],
        writer: Writer<'_>,
        to: BackgroundStatus,
        metadata: &ResponseMetadata,
    ) -> StoreResult<bool> {
        let metadata = String::try_from(metadata)?;
        Ok(background::transition(self.pool()?, id, from, writer, to, &metadata).await?)
    }

    /// Extend an owned active row's lease. Returns `false` once the owner lost it.
    pub(crate) async fn renew_background(&self, id: &str, owner: &str, lease_expires_at: i64) -> StoreResult<bool> {
        Ok(background::renew(self.pool()?, id, owner, lease_expires_at).await?)
    }

    /// The lease expiry of an active background row.
    pub(crate) async fn background_lease(&self, id: &str) -> StoreResult<Option<i64>> {
        Ok(background::active_lease(self.pool()?, id).await?)
    }

    /// Up to `limit` active background rows whose lease expired before `now`.
    pub(crate) async fn expired_background(&self, now: i64, limit: i64) -> StoreResult<Vec<String>> {
        Ok(background::expired(self.pool()?, now, limit).await?)
    }
}
