//! Concurrency limits shared by gateway tool schedulers.

use crate::config::DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(crate) const MAX_CONCURRENT_MATERIALIZATIONS: usize = 16;

/// Request-independent execution policy owned by one [`ExecutionContext`](crate::executor::ExecutionContext).
///
/// The nonzero type prevents a zero-capacity per-request semaphore. Distinct
/// execution contexts retain their own limits instead of sharing process-global state.
#[derive(Debug, Clone)]
pub(crate) struct GatewaySchedulerPolicy {
    pub(super) max_concurrent_calls: NonZeroUsize,
    materialization_permits: Arc<Semaphore>,
}

impl GatewaySchedulerPolicy {
    #[must_use]
    pub(crate) fn new(max_concurrent_calls: NonZeroUsize) -> Self {
        // Each permit protects one handler whose output is capped at
        // MAX_GATEWAY_TOOL_OUTPUT_BYTES. The policy is cloned with an
        // ExecutionContext, so this bound is shared by concurrent requests
        // (including WebSocket lanes) instead of being recreated per turn.
        Self {
            max_concurrent_calls,
            materialization_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_MATERIALIZATIONS)),
        }
    }

    /// Acquires one process-wide materialization slot shared by cloned execution contexts.
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
