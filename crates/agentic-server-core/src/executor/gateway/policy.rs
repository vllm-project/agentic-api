//! Concurrency limits shared by gateway tool schedulers.

use crate::config::DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS;
use crate::tool::GatewayBinding;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, SemaphorePermit};

pub(crate) const MAX_CONCURRENT_MATERIALIZATIONS: usize = 16;

/// Request-independent execution policy owned by one [`ExecutionContext`](crate::executor::ExecutionContext).
///
/// The nonzero type prevents a zero-capacity per-request semaphore. Distinct
/// execution contexts retain their own limits instead of sharing process-global state.
#[derive(Debug, Clone)]
pub(crate) struct GatewaySchedulerPolicy {
    max_concurrent_calls: NonZeroUsize,
    materialization_permits: Arc<Semaphore>,
}

/// Held through execution and result conversion. Dropping a call, including
/// while acquiring these permits, releases its capacity without a spawned task.
#[must_use = "keep the permits alive through execution and result conversion"]
pub(crate) struct GatewayExecutionPermits<'a> {
    _self_exclusion: Option<SemaphorePermit<'a>>,
    _execution: SemaphorePermit<'a>,
    _materialization: OwnedSemaphorePermit,
}

impl GatewaySchedulerPolicy {
    #[must_use]
    pub(crate) fn new(max_concurrent_calls: NonZeroUsize) -> Self {
        // Each permit protects one active handler or discovery operation.
        // The policy is cloned with an ExecutionContext, so this bound is
        // shared by Responses, Messages, and WebSocket lanes instead of being
        // recreated per turn. Retained-output limits are enforced separately.
        Self {
            max_concurrent_calls,
            materialization_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_MATERIALIZATIONS)),
        }
    }

    /// A fresh execution window for one inference round. Materialization
    /// capacity remains shared by all users of this policy.
    pub(crate) fn new_round_execution_slots(&self) -> Semaphore {
        Semaphore::new(self.max_concurrent_calls.get())
    }

    /// Shared acquisition order for Responses and Messages. Same-tool waiters
    /// must not occupy the round's execution slots and block unrelated tools.
    /// The execution timeout starts only after all permits have been acquired.
    pub(crate) async fn acquire_execution_permits<'a>(
        &self,
        binding: &'a GatewayBinding,
        execution_slots: &'a Semaphore,
    ) -> GatewayExecutionPermits<'a> {
        let self_exclusion = match &binding.self_exclusion {
            Some(semaphore) => Some(semaphore.acquire().await.expect("semaphore is never closed")),
            None => None,
        };
        let execution = execution_slots.acquire().await.expect("semaphore is never closed");
        let materialization = self.acquire_materialization_permit().await;
        GatewayExecutionPermits {
            _self_exclusion: self_exclusion,
            _execution: execution,
            _materialization: materialization,
        }
    }

    /// Acquires one materialization slot shared by this policy and its clones.
    pub(crate) fn acquire_materialization_permit(
        &self,
    ) -> impl Future<Output = OwnedSemaphorePermit> + Send + 'static + use<> {
        let materialization_permits = Arc::clone(&self.materialization_permits);
        async move {
            materialization_permits
                .acquire_owned()
                .await
                .expect("materialization semaphore is never closed")
        }
    }
}

impl Default for GatewaySchedulerPolicy {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS)
    }
}
