//! Backend contract for gateway-executed code interpreter calls.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::tool::ToolError;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ExecutionStatus {
    Completed,
    Failed,
    Incomplete,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct ExecutionOutput {
    pub(super) status: ExecutionStatus,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

/// Cancellation is installed after backend startup; a request can be dropped
/// before then, so an early cancel must be remembered.
#[derive(Default)]
pub(super) struct ExecutionCancellation {
    inner: Mutex<CancellationState>,
}

#[derive(Default)]
struct CancellationState {
    cancel: Option<Box<dyn Fn() + Send + Sync>>,
    cancelled: bool,
}

impl ExecutionCancellation {
    #[cfg_attr(not(feature = "embedded-code-interpreter"), allow(dead_code))]
    pub(super) fn install(&self, cancel: impl Fn() + Send + Sync + 'static) {
        let mut state = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cancelled {
            cancel();
        }
        state.cancel = Some(Box::new(cancel));
    }

    pub(super) fn cancel(&self) {
        let mut state = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled = true;
        if let Some(cancel) = &state.cancel {
            cancel();
        }
    }
}

/// A provider executes a guest and returns text for public projection.
/// The common executor bounds its retained output, owns admission and
/// cancellation supervision, and constructs wire items. A provider must still
/// control its own host output and memory while executing.
#[cfg_attr(not(any(feature = "embedded-code-interpreter", test)), allow(dead_code))]
pub(super) trait CodeInterpreterProvider: Send + Sync {
    fn check_ready(&self) -> Result<(), ToolError>;

    fn max_concurrency(&self) -> NonZeroUsize;

    fn execute(
        &self,
        code: String,
        cancellation: Arc<ExecutionCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<ExecutionOutput, ToolError>> + Send + '_>>;
}
