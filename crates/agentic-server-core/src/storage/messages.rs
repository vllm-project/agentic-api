//! SQLite/Postgres persistence for Messages-native thread checkpoints.
//!
//! A short renewable lease rejects concurrent continuation before inference.
//! Token-fenced transactions consume the parent and publish the child together.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::task::TaskTracker;

use super::models::message_thread;
use super::types::message_thread::MessageThreadState;
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
        let row = message_thread::load(self.pool()?, owner, id, now())
            .await?
            .ok_or(MessageThreadStoreError::NotFound)?;
        match row.state {
            MessageThreadState::Consumed => return Err(MessageThreadStoreError::AlreadyContinued),
            MessageThreadState::InFlight if row.lease_until > now() => return Err(MessageThreadStoreError::InFlight),
            MessageThreadState::Ready | MessageThreadState::InFlight => {}
        }
        Ok(MessageThreadCheckpoint::try_from(row)?)
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
        let changed = message_thread::reserve(self.pool()?, &owner, &id, &token, now(), now() + LEASE_SECONDS).await?;
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
        let encoded = checkpoint.map(String::try_from).transpose()?;
        if encoded.as_ref().is_some_and(|body| body.len() > MAX_CHECKPOINT_BYTES) {
            return Err(MessageThreadStoreError::TooLarge);
        }
        if lease.as_ref().is_some_and(|parent| parent.owner != *owner) {
            return Err(MessageThreadStoreError::LeaseLost);
        }
        let mut tx = self.pool()?.begin().await?;
        if let Some(parent) = &lease {
            let changed =
                message_thread::consume_in_tx(&mut tx, &parent.owner, &parent.id, &parent.token, now()).await?;
            if changed != 1 {
                return Err(MessageThreadStoreError::LeaseLost);
            }
        }
        if let Some(encoded) = encoded {
            message_thread::create_in_tx(&mut tx, owner, id, &encoded, now() + THREAD_TTL_SECONDS).await?;
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
            message_thread::renew(pool.as_ref(), &owner, &id, &token, now(), now() + LEASE_SECONDS).await?;
        }
        message_thread::expire(pool.as_ref(), now()).await?;
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
            if let Err(error) = message_thread::release(pool.as_ref(), &token).await {
                tracing::error!(%error, "failed to release message thread lease; it will expire");
            }
        });
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
