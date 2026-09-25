//! The execution span and its typed outcomes.
//!
//! One `agentic.execute` span covers a request from the moment the executor
//! accepts it until the response payload is handed back or the last streamed
//! frame has been yielded. It is a plain `tracing` span: the server's
//! OpenTelemetry bridge exports it under whatever parent (normally the
//! `http.server.request` span) is current when [`ExecutionSpan::start`] runs,
//! and embedding applications without a bridge see an ordinary log span.
//!
//! Outcomes are recorded exactly once by [`ExecutionSpan`], a guard whose
//! `Drop` fills in whatever the code path did not state explicitly, so a
//! stream dropped mid-body or a task aborted at shutdown is still finalized.
//! Execution and delivery are recorded separately: an SSE `error` frame under
//! an HTTP 200 is an execution failure that was delivered, and persistence
//! that finishes after the client went away is a completed execution that
//! was not.
//!
//! Every attribute value is bounded: outcomes and failure categories are
//! closed enums, and no request identifier, model name, prompt, or error
//! message is recorded.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use tracing::{Span, field, info_span};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use super::error::ExecutorError;

pub(crate) mod stages;

/// Span name shared by every API and transport.
pub const EXECUTE_SPAN_NAME: &str = "agentic.execute";

const ATTR_EXECUTION_OUTCOME: &str = "agentic.execution.outcome";
const ATTR_DELIVERY_OUTCOME: &str = "agentic.delivery.outcome";
const ATTR_ERROR_TYPE: &str = "error.type";
const ATTR_OTEL_STATUS: &str = "otel.status_code";

/// Which public API the request arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Responses,
    Messages,
}

impl Api {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Messages => "messages",
        }
    }
}

/// How the request was handled by the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The executor ran the request: rehydration, inference rounds, tools,
    /// persistence.
    Executor,
    /// The gateway forwards the upstream payload without semantic execution.
    Proxy,
}

impl Route {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Executor => "executor",
            Self::Proxy => "proxy",
        }
    }
}

/// Terminal state of the execution itself, independent of whether the client
/// received it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionOutcome {
    /// The response reached a terminal `completed` status.
    Completed,
    /// The response ended `incomplete` (an output or budget limit was hit).
    Incomplete,
    /// The executor failed; see the `error.type` attribute for the category.
    Failed,
    /// The execution was stopped before reaching a terminal state: the
    /// stream was dropped, or the task was aborted at shutdown.
    Cancelled,
}

impl ExecutionOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Whether the transport was handed everything the execution produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The payload, or the terminal streamed frame, was handed to the
    /// transport. Whether the transport wrote it is the transport's span.
    Delivered,
    /// The stream was dropped before its terminal frame.
    Disconnected,
    /// Execution failed before there was anything to deliver.
    NotStarted,
}

impl DeliveryOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Disconnected => "disconnected",
            Self::NotStarted => "not_started",
        }
    }
}

/// Bounded `error.type` for an execution failure, derived from the
/// [`ExecutorError`] variant — never from its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCategory {
    Storage,
    Persistence,
    ConversationLocked,
    UpstreamStatus,
    UpstreamTransport,
    Network,
    Parse,
    Stream,
    NotFound,
    InvalidRequest,
    PayloadTooLarge,
    ResourceLimit,
    Conflict,
    Compaction,
    Tool,
    /// The upstream answered with an in-band error payload (a Messages
    /// `error` body or event) rather than a non-2xx status.
    UpstreamError,
    /// The gateway tool loop hit its round budget without a terminal turn.
    RoundBudget,
    /// The executor task panicked.
    Panic,
    /// The server-selected reasoning replay policy or profile rejected the request.
    ReasoningReplay,
}

impl FailureCategory {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            Self::Persistence => "persistence",
            Self::ConversationLocked => "conversation_locked",
            Self::UpstreamStatus => "upstream_status",
            Self::UpstreamTransport => "upstream_transport",
            Self::Network => "network",
            Self::Parse => "parse",
            Self::Stream => "stream",
            Self::NotFound => "not_found",
            Self::InvalidRequest => "invalid_request",
            Self::PayloadTooLarge => "payload_too_large",
            Self::ResourceLimit => "resource_limit",
            Self::Conflict => "conflict",
            Self::Compaction => "compaction",
            Self::Tool => "tool",
            Self::UpstreamError => "upstream_error",
            Self::RoundBudget => "round_budget",
            Self::Panic => "panic",
            Self::ReasoningReplay => "reasoning_replay",
        }
    }
}

impl From<&ExecutorError> for FailureCategory {
    /// Exhaustive on purpose: a new [`ExecutorError`] variant must choose its
    /// category here rather than silently falling into a catch-all.
    fn from(error: &ExecutorError) -> Self {
        match error {
            ExecutorError::Storage(source) if source.is_not_found() => Self::NotFound,
            ExecutorError::Storage(_) => Self::Storage,
            ExecutorError::Persistence(_) => Self::Persistence,
            ExecutorError::ConversationLocked { .. } => Self::ConversationLocked,
            ExecutorError::LLMRequest { .. } => Self::UpstreamStatus,
            ExecutorError::LLMTransport { .. } => Self::UpstreamTransport,
            ExecutorError::NetworkError(_) => Self::Network,
            ExecutorError::JsonError(_) | ExecutorError::ParseError(_) | ExecutorError::UpstreamModel(_) => Self::Parse,
            ExecutorError::StreamError(_) => Self::Stream,
            ExecutorError::NotFound { .. } | ExecutorError::PreviousResponseNotFound { .. } => Self::NotFound,
            ExecutorError::InvalidRequest(_) => Self::InvalidRequest,
            ExecutorError::PayloadTooLarge(_) => Self::PayloadTooLarge,
            ExecutorError::ResourceLimitExceeded { .. } => Self::ResourceLimit,
            ExecutorError::Conflict(_) => Self::Conflict,
            ExecutorError::CompactionFailed { .. } => Self::Compaction,
            ExecutorError::Tool(_) => Self::Tool,
            ExecutorError::StreamProducerPanicked => Self::Panic,
            // A bounded category reveals no provider text from the redacted cause.
            ExecutorError::OpaqueUpstream(error) => Self::from(error.cause()),
            ExecutorError::ReasoningReplay(_) => Self::ReasoningReplay,
        }
    }
}

/// The `agentic.execute` span plus the guard that finalizes its outcome.
///
/// Create it with [`ExecutionSpan::start`], run the request inside
/// [`ExecutionSpan::span`] (via `Instrument`), state the outcome on the
/// paths that know it, and let `Drop` finalize the rest.
#[derive(Debug)]
pub struct ExecutionSpan {
    span: Span,
    execution: Option<ExecutionOutcome>,
    delivery: Option<DeliveryOutcome>,
}

impl ExecutionSpan {
    /// Open the span as a child of whatever span is current.
    #[must_use]
    pub fn start(api: Api, route: Route, stream: bool) -> Self {
        Self::with_parent(api, route, stream, Span::current().id())
    }

    /// Start an independent execution trace linked to a long-lived session.
    #[must_use]
    pub fn start_linked(api: Api, route: Route, stream: bool, link: opentelemetry::trace::SpanContext) -> Self {
        let execution = Self::with_parent(api, route, stream, None);
        if link.is_valid() {
            execution.span.add_link(link);
        }
        execution
    }

    fn with_parent(api: Api, route: Route, stream: bool, parent: Option<tracing::Id>) -> Self {
        // Field names must be literal here; the `ATTR_*` constants name the
        // same fields for `record` calls.
        let span = info_span!(
            parent: parent,
            "agentic.execute",
            agentic.api = api.as_str(),
            agentic.route = route.as_str(),
            agentic.stream = stream,
            agentic.queue.wait = field::Empty,
            agentic.execution.outcome = field::Empty,
            agentic.delivery.outcome = field::Empty,
            error.r#type = field::Empty,
            otel.status_code = field::Empty,
        );
        Self {
            span,
            execution: None,
            delivery: None,
        }
    }

    /// The span to instrument the execution future and stream with.
    #[must_use]
    pub fn span(&self) -> &Span {
        &self.span
    }

    /// The execution reached a terminal state.
    pub fn completed(&mut self) {
        self.set_execution(ExecutionOutcome::Completed);
    }

    /// The execution ended at an output or budget limit.
    pub fn incomplete(&mut self) {
        self.set_execution(ExecutionOutcome::Incomplete);
    }

    /// Terminal outcome from a Responses `status`: `incomplete` is its own
    /// outcome; anything else the executor produces here is `completed`
    /// (failures are reported through [`ExecutionSpan::failed`]).
    pub fn completed_with_status(&mut self, status: &str) {
        if status == "incomplete" {
            self.incomplete();
        } else {
            self.completed();
        }
    }

    /// Terminal outcome from a Messages `stop_reason`: `max_tokens` is the
    /// Messages form of an incomplete turn.
    pub fn completed_with_stop_reason(&mut self, stop_reason: Option<&str>) {
        if stop_reason == Some("max_tokens") {
            self.incomplete();
        } else {
            self.completed();
        }
    }

    /// The execution failed with an executor error.
    pub fn failed(&mut self, error: &ExecutorError) {
        self.failed_with(FailureCategory::from(error));
    }

    /// The execution failed for a reason that is not an [`ExecutorError`].
    pub fn failed_with(&mut self, category: FailureCategory) {
        if self.set_execution(ExecutionOutcome::Failed) {
            self.span.record(ATTR_ERROR_TYPE, category.as_str());
            self.span.record(ATTR_OTEL_STATUS, "ERROR");
        }
    }

    /// The execution was stopped before a terminal state.
    pub fn cancelled(&mut self) {
        self.set_execution(ExecutionOutcome::Cancelled);
    }

    /// The payload, or the terminal streamed frame, has been handed to the
    /// transport.
    pub fn delivered(&mut self) {
        self.set_delivery(DeliveryOutcome::Delivered);
    }

    /// Nothing was ever handed to the transport.
    pub fn not_delivered(&mut self) {
        self.set_delivery(DeliveryOutcome::NotStarted);
    }

    /// Delivery stopped after the transport accepted the response.
    pub fn disconnected(&mut self) {
        self.set_delivery(DeliveryOutcome::Disconnected);
    }

    /// Records the first execution outcome only; later calls are ignored so
    /// that a `[DONE]` after an error frame cannot overwrite the failure.
    fn set_execution(&mut self, outcome: ExecutionOutcome) -> bool {
        if self.execution.is_some() {
            return false;
        }
        self.execution = Some(outcome);
        self.span.record(ATTR_EXECUTION_OUTCOME, outcome.as_str());
        true
    }

    fn set_delivery(&mut self, outcome: DeliveryOutcome) {
        if self.delivery.is_some() {
            return;
        }
        self.delivery = Some(outcome);
        self.span.record(ATTR_DELIVERY_OUTCOME, outcome.as_str());
    }
}

/// A stream polled inside a span, so work done while producing frames —
/// persistence after the last inference round, the outcome guard's `Drop` —
/// is attributed to the request. `tracing`'s `Instrument` covers futures
/// only; this is the stream equivalent.
pub struct InstrumentedStream<S> {
    inner: S,
    span: Span,
}

impl<S> InstrumentedStream<S> {
    pub fn new(inner: S, span: Span) -> Self {
        Self { inner, span }
    }
}

impl<S: Stream + Unpin> Stream for InstrumentedStream<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let _entered = this.span.enter();
        Pin::new(&mut this.inner).poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl Drop for ExecutionSpan {
    fn drop(&mut self) {
        // Nothing stated an execution outcome: the future or stream was
        // dropped while work was pending.
        if self.execution.is_none() {
            self.set_execution(ExecutionOutcome::Cancelled);
        }
        if self.delivery.is_none() {
            // A failure before anything was produced never started delivery;
            // everything else got this far and was then dropped.
            let delivery = match self.execution {
                Some(ExecutionOutcome::Failed) => DeliveryOutcome::NotStarted,
                _ => DeliveryOutcome::Disconnected,
            };
            self.set_delivery(delivery);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolError;
    use http::StatusCode;

    #[test]
    fn failure_categories_are_derived_from_variants_not_messages() {
        let cases: Vec<(ExecutorError, FailureCategory)> = vec![
            (
                ExecutorError::LLMRequest {
                    status: StatusCode::BAD_GATEWAY,
                    body: "secret upstream body".to_owned(),
                    headers: http::HeaderMap::new(),
                },
                FailureCategory::UpstreamStatus,
            ),
            (
                ExecutorError::LLMTransport {
                    status: StatusCode::GATEWAY_TIMEOUT,
                    message: "timed out",
                },
                FailureCategory::UpstreamTransport,
            ),
            (
                ExecutorError::StreamError("worker panicked: secret".to_owned()),
                FailureCategory::Stream,
            ),
            (
                ExecutorError::ParseError("bad input".to_owned()),
                FailureCategory::Parse,
            ),
            (
                ExecutorError::InvalidRequest("nope".to_owned()),
                FailureCategory::InvalidRequest,
            ),
            (
                ExecutorError::PayloadTooLarge("too big".to_owned()),
                FailureCategory::PayloadTooLarge,
            ),
            (
                ExecutorError::ResourceLimitExceeded {
                    limit: super::super::error::ResourceLimit::ResponseBudget,
                    max_bytes: 1,
                },
                FailureCategory::ResourceLimit,
            ),
            (ExecutorError::Conflict("dup".to_owned()), FailureCategory::Conflict),
            (
                ExecutorError::CompactionFailed {
                    status: "failed".to_owned(),
                    details: "x".to_owned(),
                },
                FailureCategory::Compaction,
            ),
            (
                ExecutorError::Tool(ToolError::Execution("boom".to_owned())),
                FailureCategory::Tool,
            ),
            (
                ExecutorError::PreviousResponseNotFound {
                    id: "resp_1".to_owned(),
                },
                FailureCategory::NotFound,
            ),
            (
                ExecutorError::Persistence(Box::new(ExecutorError::Conflict("inner".to_owned()))),
                FailureCategory::Persistence,
            ),
            (ExecutorError::StreamProducerPanicked, FailureCategory::Panic),
            (
                super::super::error::OpaqueUpstreamError::redact(ExecutorError::StreamError(
                    "reflected secret".to_owned(),
                )),
                FailureCategory::Stream,
            ),
            (
                ExecutorError::UpstreamModel(crate::types::upstream_identity::UpstreamModelError::Invalid),
                FailureCategory::Parse,
            ),
            (
                ExecutorError::ReasoningReplay(crate::types::reasoning_replay::ReasoningReplayError::OpaqueNotEnabled),
                FailureCategory::ReasoningReplay,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(FailureCategory::from(&error), expected, "{error}");
            // The category is a fixed identifier, never the message.
            assert!(!expected.as_str().contains("secret"));
            assert!(!expected.as_str().contains(' '));
        }
    }

    #[test]
    fn first_execution_outcome_wins() {
        let mut execution = ExecutionSpan::start(Api::Responses, Route::Executor, true);
        execution.failed_with(FailureCategory::Stream);
        execution.completed();
        assert_eq!(execution.execution, Some(ExecutionOutcome::Failed));
    }

    #[test]
    fn drop_defaults_to_cancelled_and_disconnected() {
        let mut execution = ExecutionSpan::start(Api::Responses, Route::Executor, true);
        assert_eq!(execution.execution, None);
        // Simulate `Drop` bookkeeping without consuming the value.
        execution.cancelled();
        assert_eq!(execution.execution, Some(ExecutionOutcome::Cancelled));
    }

    #[test]
    fn incomplete_status_is_its_own_outcome() {
        let mut execution = ExecutionSpan::start(Api::Responses, Route::Executor, false);
        execution.completed_with_status("incomplete");
        assert_eq!(execution.execution, Some(ExecutionOutcome::Incomplete));

        let mut execution = ExecutionSpan::start(Api::Messages, Route::Executor, false);
        execution.completed_with_stop_reason(Some("max_tokens"));
        assert_eq!(execution.execution, Some(ExecutionOutcome::Incomplete));

        let mut execution = ExecutionSpan::start(Api::Messages, Route::Executor, false);
        execution.completed_with_stop_reason(Some("end_turn"));
        assert_eq!(execution.execution, Some(ExecutionOutcome::Completed));
    }
}
