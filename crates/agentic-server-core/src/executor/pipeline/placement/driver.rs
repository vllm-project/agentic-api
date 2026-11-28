// Shared test-only placement contracts and correctness fence, not another driver.

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Barrier, Semaphore};
use tokio_util::sync::CancellationToken;

use super::arrival::ArrivalError;
use crate::executor::error::ExecutorError;

#[derive(Debug, thiserror::Error)]
pub(super) enum DriverError {
    #[error("invalid experimental queue limits")]
    InvalidLimits,
    #[error("experimental {0} item exceeds its byte limit")]
    ItemLimit(&'static str),
    #[error("experimental driver cancelled")]
    Cancelled,
    #[error("experimental consumer disconnected")]
    ConsumerClosed,
    #[error("experimental channel closed")]
    ChannelClosed,
    #[error("experimental task did not join")]
    Join,
    #[error(transparent)]
    Ingestion(Box<ExecutorError>),
    #[error("native experimental input transport: {0}")]
    Transport(#[source] Box<ExecutorError>),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Arrival(#[from] ArrivalError),
}

impl From<ExecutorError> for DriverError {
    fn from(error: ExecutorError) -> Self {
        Self::Ingestion(Box::new(error))
    }
}

type Result<T> = std::result::Result<T, DriverError>;

#[derive(Clone, Copy)]
pub(super) enum Placement {
    Inline,
    Worker,
}

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub input_entries: usize,
    pub output_entries: usize,
    pub input_bytes: u32,
    pub output_bytes: u32,
    pub input_item_bytes: u32,
    pub output_item_bytes: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            input_entries: 1,
            output_entries: 1,
            input_bytes: 256 * 1024,
            output_bytes: 256 * 1024,
            input_item_bytes: 64 * 1024,
            output_item_bytes: 64 * 1024,
        }
    }
}

impl Limits {
    pub(super) fn validate(self) -> Result<()> {
        if self.input_entries == 0
            || self.output_entries == 0
            || self.input_entries > Semaphore::MAX_PERMITS
            || self.output_entries > Semaphore::MAX_PERMITS
            || self.input_bytes as usize > Semaphore::MAX_PERMITS
            || self.output_bytes as usize > Semaphore::MAX_PERMITS
            || self.input_item_bytes == 0
            || self.output_item_bytes == 0
            || self.input_bytes < self.input_item_bytes
            || self.output_bytes < self.output_item_bytes
        {
            return Err(DriverError::InvalidLimits);
        }
        Ok(())
    }
}

/// Correctness-only fence: compare native timestamp fields without rewriting them.
/// This fence must not be used inside a timed placement measurement.
#[derive(Clone)]
pub(super) struct FinishFence {
    barrier: Arc<Barrier>,
    ready: Arc<AtomicUsize>,
}

impl FinishFence {
    pub fn new() -> Self {
        Self {
            barrier: Arc::new(Barrier::new(3)),
            ready: Arc::default(),
        }
    }

    pub(super) async fn wait(&self, cancel: &CancellationToken) -> Result<()> {
        self.ready.fetch_add(1, Ordering::SeqCst);
        tokio::select! {
            () = cancel.cancelled() => Err(DriverError::Cancelled),
            _ = self.barrier.wait() => Ok(()),
        }
    }

    pub async fn release(&self, cancel: &CancellationToken) -> Result<()> {
        while self.ready.load(Ordering::SeqCst) < 2 {
            pace(Duration::from_millis(1), cancel).await?;
        }
        let millis = chrono::Utc::now().timestamp_subsec_millis();
        if millis >= 800 {
            pace(Duration::from_millis(u64::from(1050 - millis)), cancel).await?;
        }
        tokio::select! {
            () = cancel.cancelled() => Err(DriverError::Cancelled),
            _ = self.barrier.wait() => Ok(()),
        }
    }
}

async fn pace(delay: Duration, cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(DriverError::Cancelled);
    }
    if !delay.is_zero() {
        tokio::select! {
            () = cancel.cancelled() => return Err(DriverError::Cancelled),
            () = tokio::time::sleep(delay) => {},
        }
    }
    Ok(())
}
