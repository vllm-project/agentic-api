//! SQL for the background lifecycle columns of `responses`.
//!
//! Every status change is one conditional `UPDATE`: it names the statuses it
//! may leave and the writer allowed to leave them, and reports whether it
//! matched. Terminal statuses are never left, so concurrent writers (the job,
//! a cancellation, recovery) resolve every race by whichever update matches first.

use super::super::pool::{DbPool, DbResult, DbTransaction};
use super::super::types::BackgroundStatus;

/// The writer of a status change, which decides the rows it may change.
#[derive(Debug, Clone, Copy)]
pub enum Writer<'a> {
    /// The job that owns the row.
    Owner(&'a str),
    /// A client request, which may change an active row whatever its owner.
    Client,
    /// Recovery, for a row whose owner stopped renewing its lease before `now`.
    Recovery { now: i64 },
}

// The active-set literals match `idx_responses_background_active`, so PostgreSQL can use it.

/// The job that owns a `queued` background row, and when its lease expires unless renewed.
#[derive(Debug, Clone, Copy)]
pub struct Claim<'a> {
    pub owner: &'a str,
    pub lease_expires_at: i64,
}

/// Mark the response row just inserted in `tx` as a `queued` background response for `claim`'s owner.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn claim_in_tx(tx: &mut DbTransaction<'_>, id: &str, claim: Claim<'_>) -> DbResult<()> {
    sqlx::query(
        "UPDATE responses SET background_status = $1, background_owner = $2, background_lease_expires_at = $3 \
         WHERE id = $4",
    )
    .bind(BackgroundStatus::Queued.as_str())
    .bind(claim.owner)
    .bind(claim.lease_expires_at)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Move a row in one of the `from` statuses to `to`, replacing its snapshot.
/// Returns whether the row matched.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn transition(
    pool: &DbPool,
    id: &str,
    from: &[BackgroundStatus],
    writer: Writer<'_>,
    to: BackgroundStatus,
    snapshot: &str,
) -> DbResult<bool> {
    let (first, second) = match from {
        [only] => (*only, *only),
        [first, second] => (*first, *second),
        _ => return Err(sqlx::Error::Protocol("a transition leaves one or two statuses".into())),
    };
    let writer_clause = match writer {
        Writer::Owner(_) => " AND background_owner = $6",
        Writer::Client => "",
        Writer::Recovery { .. } => " AND background_lease_expires_at < $6",
    };
    let sql = format!(
        "UPDATE responses SET background_status = $1, snapshot = $2 \
         WHERE id = $3 AND background_status IN ($4, $5){writer_clause}"
    );
    let query = sqlx::query(&sql)
        .bind(to.as_str())
        .bind(snapshot)
        .bind(id)
        .bind(first.as_str())
        .bind(second.as_str());
    let query = match writer {
        Writer::Owner(owner) => query.bind(owner),
        Writer::Client => query,
        Writer::Recovery { now } => query.bind(now),
    };
    Ok(query.execute(pool).await?.rows_affected() == 1)
}

/// Take the `in_progress` row `owner` holds for the rest of `tx`, before reading it. The first statement of the
/// transaction is a write, as in `conversation::lock_in_tx`: on SQLite it takes the database write lock, so a later
/// write cannot fail on a snapshot another connection changed, and on PostgreSQL it locks the row against
/// concurrent writers such as a deletion that rewrites its history. Returns whether the row matched.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn hold_in_tx(tx: &mut DbTransaction<'_>, id: &str, owner: &str) -> DbResult<bool> {
    let held = sqlx::query(
        "UPDATE responses SET background_owner = background_owner \
         WHERE id = $1 AND background_owner = $2 AND background_status = 'in_progress'",
    )
    .bind(id)
    .bind(owner)
    .execute(&mut **tx)
    .await?;
    Ok(held.rows_affected() == 1)
}

/// The columns a finished background turn writes to the row it queued.
#[derive(Debug, Clone, Copy)]
pub struct FinishedRow<'a> {
    pub id: &'a str,
    pub history_item_ids: &'a str,
    pub snapshot: &'a str,
    pub metadata: &'a str,
}

/// Finish a row the caller holds through [`hold_in_tx`] with status `to`.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn finish_in_tx(tx: &mut DbTransaction<'_>, row: FinishedRow<'_>, to: BackgroundStatus) -> DbResult<()> {
    sqlx::query(
        "UPDATE responses SET background_status = $1, history_item_ids = $2, snapshot = $3, metadata = $4 \
         WHERE id = $5",
    )
    .bind(to.as_str())
    .bind(row.history_item_ids)
    .bind(row.snapshot)
    .bind(row.metadata)
    .bind(row.id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Extend the lease of an owned active row. Returns `false` once the row is
/// no longer active and owned by `owner`.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn renew(pool: &DbPool, id: &str, owner: &str, lease_expires_at: i64) -> DbResult<bool> {
    let updated = sqlx::query(
        "UPDATE responses SET background_lease_expires_at = $1 \
         WHERE id = $2 AND background_owner = $3 AND background_status IN ('queued', 'in_progress')",
    )
    .bind(lease_expires_at)
    .bind(id)
    .bind(owner)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// The lease expiry of an active row, or `None` when the row is missing or terminal.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn active_lease(pool: &DbPool, id: &str) -> DbResult<Option<i64>> {
    sqlx::query_scalar(
        "SELECT background_lease_expires_at FROM responses \
         WHERE id = $1 AND background_status IN ('queued', 'in_progress')",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Up to `limit` active rows whose lease expired before `now`, oldest expiry first.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn expired(pool: &DbPool, now: i64, limit: i64) -> DbResult<Vec<String>> {
    sqlx::query_scalar(
        "SELECT id FROM responses \
         WHERE background_status IN ('queued', 'in_progress') AND background_lease_expires_at < $1 \
         ORDER BY background_lease_expires_at, id LIMIT $2",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await
}
