//! SQLite/Postgres persistence for Messages-native thread checkpoints.
//!
//! A short renewable lease rejects concurrent continuation before inference.
//! Token-fenced transactions consume the parent and publish the child together.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::task::TaskTracker;

use super::models::message_thread::MessageThread;
use super::{DbPool, StorageError};
use crate::types::messages::thread::{MessageThreadCheckpoint, MessagesThreadOwner};

pub const MAX_CHECKPOINT_BYTES: usize = 32 * 1024 * 1024;
const THREAD_TTL_SECONDS: i64 = 300;
const LEASE_SECONDS: i64 = 60;

type ActiveLeases = Mutex<HashMap<String, (MessagesThreadOwner, String)>>;

#[derive(Debug, thiserror::Error)]
pub enum MessageThreadStoreError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("message thread not found")]
    NotFound,
    #[error("message has already been continued")]
    AlreadyContinued,
    #[error("message thread continuation is in flight")]
    InFlight,
    #[error("message thread continuation lease was lost")]
    LeaseLost,
    #[error("message thread checkpoint exceeds 32 MiB")]
    TooLarge,
}

impl From<sqlx::Error> for MessageThreadStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Storage(StorageError::Database(error))
    }
}
impl From<serde_json::Error> for MessageThreadStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(StorageError::Serialization(error))
    }
}

#[derive(Debug, Default)]
struct Inner {
    pool: Option<Arc<DbPool>>,
    active: ActiveLeases,
    cleanup: TaskTracker,
}

/// Durable thread store. `Default` is disabled, for non-threaded core callers.
#[derive(Debug, Clone, Default)]
pub struct MessageThreadStore {
    inner: Arc<Inner>,
}

impl MessageThreadStore {
    #[must_use]
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self {
            inner: Arc::new(Inner {
                pool: Some(pool),
                ..Inner::default()
            }),
        }
    }

    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.inner.pool.is_some()
    }

    /// Check the dedicated schema on startup and readiness probes.
    ///
    /// # Errors
    /// Returns a storage error for missing columns or insufficient read access.
    pub async fn verify_ready(&self) -> Result<(), MessageThreadStoreError> {
        let mut connection = self.pool()?.acquire().await?;
        sqlx::query("SELECT message_id, owner_hash, checkpoint, state, lease_token, lease_until, expires_at FROM message_threads LIMIT 0")
            .execute(&mut *connection).await?;
        if super::DatabaseBackend::from_connection(&connection) == super::DatabaseBackend::Postgres {
            let writable: bool = sqlx::query_scalar(
                "SELECT has_table_privilege(current_user, 'message_threads', 'SELECT') \
                    AND has_table_privilege(current_user, 'message_threads', 'INSERT') \
                    AND has_table_privilege(current_user, 'message_threads', 'UPDATE') \
                    AND has_table_privilege(current_user, 'message_threads', 'DELETE')",
            )
            .fetch_one(&mut *connection)
            .await?;
            if !writable {
                return Err(
                    sqlx::Error::Configuration("Messages thread table is missing required privileges".into()).into(),
                );
            }
        }
        Ok(())
    }

    fn pool(&self) -> Result<&DbPool, MessageThreadStoreError> {
        self.inner
            .pool
            .as_deref()
            .ok_or_else(|| StorageError::NotConfigured.into())
    }

    /// Load an unexpired checkpoint without modifying its lease or lifetime.
    ///
    /// # Errors
    /// Returns a storage or thread-state error when the parent cannot be used.
    pub async fn load(
        &self,
        owner: &MessagesThreadOwner,
        id: &str,
    ) -> Result<MessageThreadCheckpoint, MessageThreadStoreError> {
        let row = sqlx::query_as::<_, MessageThread>(
            "SELECT checkpoint, state, lease_until FROM message_threads \
             WHERE message_id = $1 AND owner_hash = $2 AND expires_at > $3",
        )
        .bind(id)
        .bind(owner.as_str())
        .bind(now())
        .fetch_optional(self.pool()?)
        .await?
        .ok_or(MessageThreadStoreError::NotFound)?;
        match row.state.as_str() {
            "consumed" => return Err(MessageThreadStoreError::AlreadyContinued),
            "in_flight" if row.lease_until > now() => return Err(MessageThreadStoreError::InFlight),
            "ready" | "in_flight" => {}
            _ => return Err(MessageThreadStoreError::LeaseLost),
        }
        let checkpoint = row.checkpoint.ok_or(MessageThreadStoreError::NotFound)?;
        Ok(serde_json::from_str(&checkpoint)?)
    }

    /// Atomically reserve a validated parent across all gateway instances.
    ///
    /// # Errors
    /// Returns a state conflict or database error if reservation fails.
    pub async fn reserve(
        &self,
        owner: MessagesThreadOwner,
        id: String,
    ) -> Result<MessageThreadLease, MessageThreadStoreError> {
        let token = uuid::Uuid::now_v7().to_string();
        let changed = sqlx::query(
            "UPDATE message_threads SET state = 'in_flight', lease_token = $1, lease_until = $2 \
             WHERE message_id = $3 AND owner_hash = $4 AND expires_at > $5 \
               AND (state = 'ready' OR (state = 'in_flight' AND lease_until <= $5))",
        )
        .bind(&token)
        .bind(now() + LEASE_SECONDS)
        .bind(&id)
        .bind(owner.as_str())
        .bind(now())
        .execute(self.pool()?)
        .await?
        .rows_affected();
        if changed != 1 {
            // Resolve the same typed state errors as load, without extending expiry.
            self.load(&owner, &id).await?;
            return Err(MessageThreadStoreError::InFlight);
        }
        self.inner
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(token.clone(), (owner.clone(), id.clone()));
        Ok(MessageThreadLease {
            store: self.clone(),
            owner,
            id,
            token,
            completed: false,
        })
    }

    /// Consume the parent and save the completed child in one transaction.
    /// A missing child checkpoint (refusal/fallback) still consumes the parent.
    ///
    /// # Errors
    /// Returns an error before client completion on persistence or fencing failure.
    pub async fn publish(
        &self,
        owner: &MessagesThreadOwner,
        id: &str,
        checkpoint: Option<&MessageThreadCheckpoint>,
        mut lease: Option<MessageThreadLease>,
    ) -> Result<(), MessageThreadStoreError> {
        let encoded = checkpoint.map(serde_json::to_string).transpose()?;
        if encoded.as_ref().is_some_and(|body| body.len() > MAX_CHECKPOINT_BYTES) {
            return Err(MessageThreadStoreError::TooLarge);
        }
        if lease.as_ref().is_some_and(|parent| parent.owner != *owner) {
            return Err(MessageThreadStoreError::LeaseLost);
        }
        let mut tx = self.pool()?.begin().await?;
        if let Some(parent) = &lease {
            let changed = sqlx::query(
                "UPDATE message_threads SET state = 'consumed', checkpoint = NULL, lease_token = NULL, lease_until = 0 \
                 WHERE message_id = $1 AND owner_hash = $2 AND lease_token = $3 AND state = 'in_flight' AND lease_until > $4",
            ).bind(&parent.id).bind(parent.owner.as_str()).bind(&parent.token).bind(now())
                .execute(&mut *tx).await?.rows_affected();
            if changed != 1 {
                return Err(MessageThreadStoreError::LeaseLost);
            }
        }
        if let Some(encoded) = encoded {
            sqlx::query(
                "INSERT INTO message_threads (message_id, owner_hash, checkpoint, state, expires_at) \
                 VALUES ($1, $2, $3, 'ready', $4)",
            )
            .bind(id)
            .bind(owner.as_str())
            .bind(encoded)
            .bind(now() + THREAD_TTL_SECONDS)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        if let Some(parent) = &mut lease {
            parent.completed = true;
        }
        Ok(())
    }

    /// Renew live leases and delete expired checkpoints. Drive every 10 seconds.
    ///
    /// # Errors
    /// Returns a database error; existing leases still fence stale completions.
    pub async fn maintain(&self) -> Result<(), MessageThreadStoreError> {
        let Some(pool) = &self.inner.pool else {
            return Ok(());
        };
        let active: Vec<_> = self
            .inner
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(token, (owner, id))| (token.clone(), owner.clone(), id.clone()))
            .collect();
        for (token, owner, id) in active {
            sqlx::query(
                "UPDATE message_threads SET lease_until = $1 WHERE message_id = $2 AND owner_hash = $3 \
                         AND lease_token = $4 AND state = 'in_flight' AND lease_until > $5",
            )
            .bind(now() + LEASE_SECONDS)
            .bind(id)
            .bind(owner.as_str())
            .bind(token)
            .bind(now())
            .execute(pool.as_ref())
            .await?;
        }
        sqlx::query(
            "DELETE FROM message_threads WHERE expires_at <= $1 AND (state <> 'in_flight' OR lease_until <= $1)",
        )
        .bind(now())
        .execute(pool.as_ref())
        .await?;
        Ok(())
    }

    /// Join every cancellation cleanup operation after request handlers drain.
    pub async fn shutdown(&self) {
        self.inner.cleanup.close();
        self.inner.cleanup.wait().await;
    }
}

/// Cancellation-safe lease; asynchronous release tasks are tracked and joined.
#[derive(Debug)]
pub struct MessageThreadLease {
    store: MessageThreadStore,
    owner: MessagesThreadOwner,
    id: String,
    token: String,
    completed: bool,
}

impl Drop for MessageThreadLease {
    fn drop(&mut self) {
        self.store
            .inner
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.token);
        if self.completed {
            return;
        }
        let Some(pool) = self.store.inner.pool.clone() else {
            return;
        };
        let token = self.token.clone();
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::warn!("message thread lease dropped outside runtime; it will expire");
            return;
        }
        self.store.inner.cleanup.spawn(async move {
            if let Err(error) = sqlx::query(
                "UPDATE message_threads SET state = 'ready', lease_token = NULL, lease_until = 0 \
                 WHERE lease_token = $1 AND state = 'in_flight'",
            )
            .bind(token)
            .execute(pool.as_ref())
            .await
            {
                tracing::error!(%error, "failed to release message thread lease; it will expire");
            }
        });
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
