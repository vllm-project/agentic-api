//! Typed errors for background requests and background response operations.

use http::StatusCode;

use crate::executor::telemetry::FailureCategory;
use crate::storage::BackgroundStatus;

/// A background request or operation the gateway rejects, or the reason a job failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackgroundError {
    #[error("background mode requires store: true; store: false background responses are not supported yet")]
    RequiresStore,
    #[error("background mode does not support stream: true yet; poll GET /v1/responses/{{response_id}} instead")]
    Streaming,
    #[error("background mode does not support multi_agent yet")]
    MultiAgent,
    #[error("background mode does not support transient session continuation")]
    SessionContinuation,
    #[error("streaming retrieval is not supported yet; omit `{param}`")]
    StreamingRetrieval { param: &'static str },
    #[error("background capacity is full; retry the request later")]
    CapacityExceeded,
    #[error("the gateway is shutting down; retry the request on another instance")]
    ShuttingDown,
    #[error("Only responses created with background=true can be cancelled.")]
    NotBackground,
    #[error("Cannot cancel a response with status '{0}'.")]
    NotCancellable(BackgroundStatus),
    #[error(
        "Previous response with id '{id}' has status '{status}'; only a completed or incomplete response can be continued."
    )]
    PreviousNotFinished { id: String, status: BackgroundStatus },
    #[error("Input items are not available for a background response with status '{0}'.")]
    InputItemsUnavailable(BackgroundStatus),
    #[error("the background response exceeded its {0}-second timeout")]
    TimedOut(u64),
    #[error("the gateway instance running this background response stopped before it finished")]
    Interrupted,
    #[error("the background job stopped on an internal error")]
    Panicked,
}

impl BackgroundError {
    /// The bounded `error.type` an execution failing with this error records.
    pub(crate) fn failure_category(&self) -> FailureCategory {
        match self {
            Self::CapacityExceeded | Self::ShuttingDown | Self::TimedOut(_) => FailureCategory::ResourceLimit,
            // A shutdown records `cancelled`; an interruption reaches `failed` only through a panic.
            Self::Interrupted | Self::Panicked => FailureCategory::Panic,
            Self::RequiresStore
            | Self::Streaming
            | Self::MultiAgent
            | Self::SessionContinuation
            | Self::StreamingRetrieval { .. }
            | Self::NotBackground
            | Self::NotCancellable(_)
            | Self::PreviousNotFinished { .. }
            | Self::InputItemsUnavailable(_) => FailureCategory::InvalidRequest,
        }
    }

    #[must_use]
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::RequiresStore
            | Self::Streaming
            | Self::MultiAgent
            | Self::SessionContinuation
            | Self::StreamingRetrieval { .. }
            | Self::NotBackground
            | Self::NotCancellable(_)
            | Self::PreviousNotFinished { .. } => StatusCode::BAD_REQUEST,
            Self::InputItemsUnavailable(_) => StatusCode::CONFLICT,
            Self::CapacityExceeded | Self::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
            Self::TimedOut(_) | Self::Interrupted | Self::Panicked => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self.http_status() {
            StatusCode::BAD_REQUEST => "invalid_request_error",
            StatusCode::CONFLICT => "conflict_error",
            _ => "server_error",
        }
    }

    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RequiresStore | Self::Streaming | Self::MultiAgent | Self::SessionContinuation => "unsupported_value",
            Self::StreamingRetrieval { .. } => "unsupported_parameter",
            Self::CapacityExceeded => "background_capacity_exceeded",
            Self::ShuttingDown => "server_shutting_down",
            Self::NotBackground | Self::NotCancellable(_) => "response_not_cancellable",
            Self::PreviousNotFinished { .. } => "previous_response_not_completed",
            Self::InputItemsUnavailable(_) => "response_not_completed",
            Self::TimedOut(_) | Self::Interrupted | Self::Panicked => "server_error",
        }
    }

    #[must_use]
    pub fn param(&self) -> Option<&'static str> {
        match self {
            Self::RequiresStore => Some("store"),
            Self::Streaming => Some("stream"),
            Self::MultiAgent => Some("multi_agent"),
            Self::StreamingRetrieval { param } => Some(param),
            Self::NotBackground | Self::NotCancellable(_) => Some("response_id"),
            Self::SessionContinuation | Self::PreviousNotFinished { .. } => Some("previous_response_id"),
            Self::CapacityExceeded
            | Self::ShuttingDown
            | Self::InputItemsUnavailable(_)
            | Self::TimedOut(_)
            | Self::Interrupted
            | Self::Panicked => None,
        }
    }
}
