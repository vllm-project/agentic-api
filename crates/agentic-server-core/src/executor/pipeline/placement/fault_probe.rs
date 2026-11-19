// Deterministic correctness-only synchronization; never enable in a timed run.

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub(super) struct EarlyFailureProbe {
    entered: Arc<AtomicBool>,
}

impl EarlyFailureProbe {
    pub async fn wait_until_paused(&self, cancel: &CancellationToken) -> bool {
        while !self.entered.load(Ordering::SeqCst) {
            tokio::select! {
                () = cancel.cancelled() => return false,
                () = tokio::time::sleep(Duration::from_millis(1)) => {},
            }
        }
        true
    }

    /// Model a long paced input wait after the first admitted line. Cancellation
    /// must wake it; the test must never wait for the timer to expire.
    pub async fn pause_before_second_input(&self, cancel: &CancellationToken) {
        self.entered.store(true, Ordering::SeqCst);
        tokio::select! {
            () = cancel.cancelled() => {},
            () = tokio::time::sleep(Duration::from_secs(3600)) => {
                panic!("the correctness probe was not cancelled before its timer expired");
            },
        }
    }
}
