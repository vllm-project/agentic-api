//! Database operations for Messages checkpoints and token-fenced leases.

use sqlx::{Any, Transaction};

use crate::storage::DbPool;
use crate::storage::types::message_thread::MessageThreadState;
use crate::types::messages::thread::MessagesThreadOwner;

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct MessageThread {
    pub checkpoint: Option<String>,
    #[sqlx(try_from = "String")]
    pub state: MessageThreadState,
    pub lease_until: i64,
}

pub(crate) async fn load(
    pool: &DbPool,
    owner: &MessagesThreadOwner,
    id: &str,
    now: i64,
) -> Result<Option<MessageThread>, sqlx::Error> {
    sqlx::query_as(
        "SELECT checkpoint, state, lease_until FROM message_threads \
        WHERE message_id = $1 AND owner_hash = $2 AND expires_at > $3",
    )
    .bind(id)
    .bind(owner.as_str())
    .bind(now)
    .fetch_optional(pool)
    .await
}

pub(crate) async fn reserve(
    pool: &DbPool,
    owner: &MessagesThreadOwner,
    id: &str,
    token: &str,
    now: i64,
    until: i64,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE message_threads SET state = 'in_flight', lease_token = $1, lease_until = $2 \
        WHERE message_id = $3 AND owner_hash = $4 AND expires_at > $5 \
        AND (state = 'ready' OR (state = 'in_flight' AND lease_until <= $5))",
    )
    .bind(token)
    .bind(until)
    .bind(id)
    .bind(owner.as_str())
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected())
}

pub(crate) async fn consume_in_tx(
    tx: &mut Transaction<'_, Any>,
    owner: &MessagesThreadOwner,
    id: &str,
    token: &str,
    now: i64,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE message_threads SET state = 'consumed', checkpoint = NULL, lease_token = NULL, lease_until = 0 \
        WHERE message_id = $1 AND owner_hash = $2 AND lease_token = $3 AND state = 'in_flight' AND lease_until > $4",
    )
    .bind(id)
    .bind(owner.as_str())
    .bind(token)
    .bind(now)
    .execute(&mut **tx)
    .await?
    .rows_affected())
}

pub(crate) async fn create_in_tx(
    tx: &mut Transaction<'_, Any>,
    owner: &MessagesThreadOwner,
    id: &str,
    checkpoint: &str,
    expires: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO message_threads (message_id, owner_hash, checkpoint, state, expires_at) \
        VALUES ($1, $2, $3, 'ready', $4)",
    )
    .bind(id)
    .bind(owner.as_str())
    .bind(checkpoint)
    .bind(expires)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn renew(
    pool: &DbPool,
    owner: &MessagesThreadOwner,
    id: &str,
    token: &str,
    now: i64,
    until: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE message_threads SET lease_until = $1 WHERE message_id = $2 AND owner_hash = $3 \
        AND lease_token = $4 AND state = 'in_flight' AND lease_until > $5",
    )
    .bind(until)
    .bind(id)
    .bind(owner.as_str())
    .bind(token)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn expire(pool: &DbPool, now: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM message_threads WHERE expires_at <= $1 AND (state <> 'in_flight' OR lease_until <= $1)")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn release(pool: &DbPool, token: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE message_threads SET state = 'ready', lease_token = NULL, lease_until = 0 \
        WHERE lease_token = $1 AND state = 'in_flight'",
    )
    .bind(token)
    .execute(pool)
    .await?;
    Ok(())
}
