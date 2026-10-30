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

/// Insert a `queued` background response owned by `owner`.
///
/// # Errors
/// Returns an error if the insert fails, including for an existing ID.
pub async fn insert_queued(
    pool: &DbPool,
    id: &str,
    metadata: &str,
    created_at: i64,
    owner: &str,
    lease_expires_at: i64,
) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO responses \
         (id, metadata, created_at, background_status, background_owner, background_lease_expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(metadata)
    .bind(created_at)
    .bind(BackgroundStatus::Queued.as_str())
    .bind(owner)
    .bind(lease_expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Move a row in one of the `from` statuses to `to`, replacing its metadata.
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
    metadata: &str,
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
        "UPDATE responses SET background_status = $1, metadata = $2 \
         WHERE id = $3 AND background_status IN ($4, $5){writer_clause}"
    );
    let query = sqlx::query(&sql)
        .bind(to.as_str())
        .bind(metadata)
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

/// The columns a finished turn writes to its `responses` row.
#[derive(Debug, Clone, Copy)]
pub struct TurnRow<'a> {
    pub id: &'a str,
    pub conversation_id: Option<&'a str>,
    pub previous_response_id: Option<&'a str>,
    pub history_item_ids: &'a str,
    pub metadata: &'a str,
}

/// Complete an owned `in_progress` row with its history inside the caller's
/// persistence transaction. Returns whether the row matched; the caller rolls
/// back the transaction's item inserts when it did not.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn finalize_in_tx(
    tx: &mut DbTransaction<'_>,
    row: TurnRow<'_>,
    owner: &str,
    to: BackgroundStatus,
) -> DbResult<bool> {
    let updated = sqlx::query(
        "UPDATE responses SET background_status = $1, conversation_id = $2, previous_response_id = $3, \
         history_item_ids = $4, metadata = $5 \
         WHERE id = $6 AND background_owner = $7 AND background_status = 'in_progress'",
    )
    .bind(to.as_str())
    .bind(row.conversation_id)
    .bind(row.previous_response_id)
    .bind(row.history_item_ids)
    .bind(row.metadata)
    .bind(row.id)
    .bind(owner)
    .execute(&mut **tx)
    .await?;
    Ok(updated.rows_affected() == 1)
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
