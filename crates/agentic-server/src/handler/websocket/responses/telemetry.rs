//! Trace ownership from WebSocket admission through dispatch or queue disposal.

use std::time::Instant;

use agentic_core::executor::telemetry::{Api, ExecutionSpan, FailureCategory, Route};
use opentelemetry::trace::TraceContextExt as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use super::WsError;

pub(in crate::handler::websocket) struct QueuedExecution {
    execution: ExecutionSpan,
    _wait: QueueWait,
}

struct QueueWait {
    span: tracing::Span,
    admitted: Instant,
}

impl Drop for QueueWait {
    fn drop(&mut self) {
        self.span
            .record("agentic.queue.wait", self.admitted.elapsed().as_secs_f64());
    }
}

impl QueuedExecution {
    pub(super) fn new() -> Self {
        let session = tracing::Span::current().context().span().span_context().clone();
        let execution = ExecutionSpan::start_linked(Api::Responses, Route::Executor, true, session);
        let wait = QueueWait {
            span: execution.span().clone(),
            admitted: Instant::now(),
        };
        Self { execution, _wait: wait }
    }

    pub(super) fn dispatch(self) -> ExecutionSpan {
        self.execution
    }
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
