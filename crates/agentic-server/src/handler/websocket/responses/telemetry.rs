//! Trace ownership from WebSocket admission through dispatch or queue disposal.

use std::time::Instant;

use crate::app::AppState;
use crate::telemetry::websocket::WebSocketMetrics;

use agentic_core::executor::telemetry::{Api, ExecutionSpan, FailureCategory, Route};
use opentelemetry::trace::TraceContextExt as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use super::WsError;

pub(super) struct QueuedExecution {
    execution: ExecutionSpan,
    _wait: QueueWait,
}

struct QueueWait {
    span: tracing::Span,
    admitted: Instant,
    metrics: WebSocketMetrics,
}

impl Drop for QueueWait {
    fn drop(&mut self) {
        let waited = self.admitted.elapsed();
        self.span.record("agentic.queue.wait", waited.as_secs_f64());
        self.metrics.record_queue_wait(waited);
    }
}

impl QueuedExecution {
    pub(super) fn new(state: &AppState) -> Self {
        let session = tracing::Span::current().context().span().span_context().clone();
        let metrics = &state.exec_ctx.metrics;
        let execution = ExecutionSpan::start_linked(Api::Responses, Route::Executor, true, metrics, session);
        let wait = QueueWait {
            span: execution.span().clone(),
            admitted: Instant::now(),
            metrics: state.websocket_tracker.metrics().clone(),
        };
        Self { execution, _wait: wait }
    }

    fn dispatch(self) -> ExecutionSpan {
        self.execution
    }
}

/// Start executing an admitted request, ending its queue wait. Every admitted
/// request carries its execution; one that does not is traced from dispatch.
pub(super) fn dispatch(queued: Option<QueuedExecution>, state: &AppState) -> ExecutionSpan {
    queued.unwrap_or_else(|| QueuedExecution::new(state)).dispatch()
}

pub(super) fn finish_local(execution: &mut ExecutionSpan, result: &Result<(), WsError>) {
    match result {
        Ok(()) => {
            execution.completed();
            execution.delivered();
        }
        // Local completion persists before sending; a send failure only changes delivery.
        Err(WsError::SendFailed) => execution.completed(),
        Err(WsError::Executor(error)) => {
            execution.failed(error);
            execution.not_delivered();
        }
        Err(WsError::SerializeJson(_)) => {
            execution.failed_with(FailureCategory::Parse);
            execution.not_delivered();
        }
        Err(WsError::InvalidJson(_) | WsError::UnexpectedType | WsError::BinaryFrame | WsError::TooManyRequests) => {
            execution.failed_with(FailureCategory::InvalidRequest);
            execution.not_delivered();
        }
    }
}
