use http::StatusCode;
use thiserror::Error;

use crate::StorageError;
use crate::tool::ToolError;
use crate::types::file_search::FileSearchError;
use crate::types::io::ClientItemError;
use crate::types::io::input::InputValidationError;
use crate::types::io::output::McpListTools;
use crate::types::request_response::{
    MAX_TOOL_CALLS_PARAM, MaxToolCallsError, UnsupportedParameterError, UpstreamRequestError,
};
use crate::utils::common::serialize_to_vec_or_default;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceLimit {
    ResponseBudget,
    UpstreamSseLine,
    UpstreamJsonBody,
    StreamEvent,
}

impl std::fmt::Display for ResourceLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ResponseBudget => write!(f, "executor response budget"),
            Self::UpstreamSseLine => write!(f, "upstream SSE line"),
            Self::UpstreamJsonBody => write!(f, "upstream response"),
            Self::StreamEvent => write!(f, "stream event"),
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error(transparent)]
    FileSearch(#[from] FileSearchError),
    /// Request-owned streaming orchestration panicked. Never forward panic data.
    #[error("stream producer panicked")]
    StreamProducerPanicked,
    /// A storage layer operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Persistence failed after inference completed.
    ///
    /// The source is retained for internal diagnostics while the display
    /// message remains safe to send to API clients.
    #[error("failed to persist response")]
    Persistence(#[source] Box<ExecutorError>),

    /// A persisted conversation changed after its history was read.
    ///
    /// The storage source is retained for internal diagnostics while the
    /// display message remains safe to send to API clients.
    #[error("conversation changed while the response was being generated; retry the request")]
    ConversationLocked {
        #[source]
        source: StorageError,
    },

    /// The LLM backend returned a non-2xx HTTP response.
    #[error("LLM request failed ({status}): {body}")]
    LLMRequest {
        status: StatusCode,
        body: String,
        headers: http::HeaderMap,
    },

    /// The LLM backend could not be reached or timed out before responding.
    #[error("{message}")]
    LLMTransport { status: StatusCode, message: &'static str },

    /// A direct model runtime failed; retain the source without exposing its internals.
    #[error("direct inference failed")]
    DirectInference {
        status: StatusCode,
        param: Option<&'static str>,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A network error occurred reading from the LLM response stream.
    ///
    /// The original `reqwest::Error` is preserved as the error source so
    /// callers can inspect the underlying network failure.
    #[error("network error: {0}")]
    NetworkError(
        #[from]
        #[source]
        reqwest::Error,
    ),

    /// JSON deserialisation failed.
    ///
    /// The original `serde_json::Error` is preserved as the error source so
    /// callers can inspect the exact parse failure location and kind.
    #[error("json error: {0}")]
    JsonError(
        #[from]
        #[source]
        serde_json::Error,
    ),

    /// The upstream supplied invalid typed content. Keep parser diagnostics as
    /// a source without reporting a backend defect as a client request error.
    #[error("upstream returned invalid Messages content")]
    UpstreamProtocol(#[source] serde_json::Error),

    /// A general stream processing error with a human-readable message.
    ///
    /// Used for non-network stream failures (e.g. worker thread panic).
    #[error("stream error: {0}")]
    StreamError(String),

    /// A validation error on the request payload with a human-readable message.
    ///
    /// Used when required fields are missing or structurally invalid.
    #[error("parse error: {0}")]
    ParseError(String),

    #[error("{entity} not found: {id}")]
    NotFound { entity: String, id: String },

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("invalid request: {0}")]
    InvalidInput(InputValidationError),

    /// A client-supplied item `OpenAI` refuses.
    #[error(transparent)]
    ClientItem(#[from] ClientItemError),

    /// The request's `max_tool_calls` is not an integer of at least one.
    #[error(transparent)]
    InvalidMaxToolCalls(#[from] MaxToolCallsError),
    /// A rejected request parameter value, with its public code and parameter name; `None` is reported as a `null`
    /// parameter.
    #[error("{message}")]
    UnsupportedValue {
        param: Option<String>,
        code: Option<&'static str>,
        message: String,
    },
    /// A recognized request member this gateway cannot honour (`background: true`, `prompt`).
    #[error(transparent)]
    UnsupportedParameter(#[from] UnsupportedParameterError),

    /// A `file_id` reference does not resolve to a file the gateway stores.
    #[error("Files [{}] were not found", .ids.join(", "))]
    FileReferencesNotFound { ids: Vec<String> },

    /// The request exceeds a documented transport or component size budget.
    #[error("{0}")]
    PayloadTooLarge(String),

    /// A configured response or transport size boundary was exceeded.
    #[error("{limit} exceeded {max_bytes} bytes")]
    ResourceLimitExceeded { limit: ResourceLimit, max_bytes: usize },

    /// The request conflicts with state already stored.
    #[error("conflict: {0}")]
    Conflict(String),

    #[error("compaction summarization failed with status '{status}': {details}")]
    CompactionFailed { status: String, details: String },

    #[error("tool error: {0}")]
    Tool(#[from] ToolError),

    /// A declared MCP server's tool listing failed. As in the Responses API,
    /// the request fails: HTTP 424 for JSON, and an `error` event followed by
    /// `response.failed` for a stream. The failed listing item is kept for the stream.
    #[error(
        "Error retrieving tool list from MCP server: '{}'. Http status code: 424 (Failed Dependency)",
        .0.server_label
    )]
    McpListToolsFailed(Box<McpListTools>),
}

impl From<UpstreamRequestError> for ExecutorError {
    fn from(error: UpstreamRequestError) -> Self {
        match error {
            UpstreamRequestError::Input(error) => error.into(),
            UpstreamRequestError::Parameter(error) => error.into(),
            UpstreamRequestError::Tool(error) => error.into(),
        }
    }
}

impl From<InputValidationError> for ExecutorError {
    fn from(error: InputValidationError) -> Self {
        // A resolved file whose kind does not match its part uses the recorded gateway envelope:
        // `param: input`, `code: null`, and no message position or gateway prefix.
        match error.resolved_file_reason() {
            Some(reason) => Self::UnsupportedValue {
                param: Some("input".to_owned()),
                code: None,
                message: reason.to_string(),
            },
            None => Self::InvalidInput(error),
        }
    }
}

impl ExecutorError {
    pub(crate) fn is_invalid_upstream_tool_search(&self) -> bool {
        matches!(
            self,
            Self::Tool(ToolError::InvalidUpstreamToolSearch | ToolError::UpstreamWithheldFunctionCall)
        )
    }

    fn client_visible_error(&self) -> &Self {
        match self {
            Self::Persistence(source) if source.contains_conversation_locked() => source.client_visible_error(),
            _ => self,
        }
    }

    fn contains_conversation_locked(&self) -> bool {
        match self {
            Self::ConversationLocked { .. } => true,
            Self::Persistence(source) => source.contains_conversation_locked(),
            _ => false,
        }
    }

    /// HTTP status code that best represents this error to an API caller.
    #[must_use]
    pub fn http_status(&self) -> StatusCode {
        match self.client_visible_error() {
            Self::FileSearch(error) | Self::Tool(ToolError::FileSearch(error)) => {
                StatusCode::from_u16(error.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
            }
            Self::Storage(StorageError::PreviousResponseNotFound { .. })
            | Self::ConversationLocked { .. }
            | Self::Tool(
                ToolError::Config(_)
                | ToolError::MissingOutput { .. }
                | ToolError::UnknownCallOutput { .. }
                | ToolError::UnknownParameter { .. },
            )
            | Self::InvalidRequest(_)
            | Self::InvalidInput(_)
            | Self::ClientItem(_)
            | Self::InvalidMaxToolCalls(_)
            | Self::UnsupportedValue { .. }
            | Self::UnsupportedParameter(_)
            | Self::JsonError(_) => StatusCode::BAD_REQUEST,
            Self::Storage(e) if e.is_not_found() => StatusCode::NOT_FOUND,
            Self::FileReferencesNotFound { .. } => StatusCode::NOT_FOUND,
            Self::Storage(e) if e.is_validation() => StatusCode::BAD_REQUEST,
            Self::LLMRequest { status, .. }
            | Self::LLMTransport { status, .. }
            | Self::DirectInference { status, .. } => *status,
            Self::Tool(
                ToolError::Execution(_)
                | ToolError::InvalidUpstreamToolSearch
                | ToolError::UpstreamWithheldFunctionCall,
            )
            | Self::CompactionFailed { .. } => StatusCode::BAD_GATEWAY,
            Self::Conflict(_) | Self::Storage(StorageError::ItemConflict) => StatusCode::CONFLICT,
            Self::McpListToolsFailed(_) => StatusCode::FAILED_DEPENDENCY,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::ParseError(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::ResourceLimitExceeded { limit, .. } => match limit {
                ResourceLimit::UpstreamSseLine | ResourceLimit::UpstreamJsonBody => StatusCode::BAD_GATEWAY,
                ResourceLimit::ResponseBudget | ResourceLimit::StreamEvent => StatusCode::INTERNAL_SERVER_ERROR,
            },
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Machine-readable error type for the API error envelope.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self.client_visible_error() {
            Self::FileSearch(error) | Self::Tool(ToolError::FileSearch(error)) => match error {
                FileSearchError::FileNotFound { .. } | FileSearchError::FileRejected { .. } => "invalid_request_error",
                _ => match error.status_code() {
                    400 => "invalid_request_error",
                    404 => "not_found",
                    409 => "conflict_error",
                    502 => "upstream_error",
                    _ => "server_error",
                },
            },
            Self::ConversationLocked { .. }
            | Self::Tool(
                ToolError::Config(_)
                | ToolError::MissingOutput { .. }
                | ToolError::UnknownCallOutput { .. }
                | ToolError::UnknownParameter { .. },
            )
            | Self::InvalidRequest(_)
            | Self::InvalidInput(_)
            | Self::ClientItem(_)
            | Self::InvalidMaxToolCalls(_)
            | Self::UnsupportedValue { .. }
            | Self::UnsupportedParameter(_)
            | Self::FileReferencesNotFound { .. }
            | Self::ParseError(_)
            | Self::JsonError(_)
            | Self::PayloadTooLarge(_)
            | Self::Storage(
                StorageError::ItemConflict
                | StorageError::ItemCursorNotFound { .. }
                | StorageError::ResponseNotFound { .. }
                | StorageError::PreviousResponseNotFound { .. }
                | StorageError::ConversationNotFound { .. },
            ) => "invalid_request_error",
            Self::Storage(e) if e.is_not_found() => "not_found",
            Self::Storage(e) if e.is_validation() => "invalid_request_error",
            Self::Conflict(_) => "conflict_error",
            Self::McpListToolsFailed(_) => "external_connector_error",
            Self::DirectInference { status, .. } if status.is_client_error() => "invalid_request_error",
            Self::LLMRequest { .. }
            | Self::LLMTransport { .. }
            | Self::CompactionFailed { .. }
            | Self::DirectInference { .. } => "upstream_error",
            Self::ResourceLimitExceeded { limit, .. } => match limit {
                ResourceLimit::UpstreamSseLine | ResourceLimit::UpstreamJsonBody => "upstream_error",
                ResourceLimit::ResponseBudget | ResourceLimit::StreamEvent => "server_error",
            },
            Self::Tool(
                ToolError::Execution(_)
                | ToolError::InvalidUpstreamToolSearch
                | ToolError::UpstreamWithheldFunctionCall,
            ) => "tool_error",
            _ => "server_error",
        }
    }

    /// Short machine-readable error code for the API error envelope.
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self.client_visible_error() {
            Self::ConversationLocked { .. } => "conversation_locked",
            Self::Storage(StorageError::PreviousResponseNotFound { .. }) => "previous_response_not_found",
            Self::Conflict(_) => "response_already_stored",
            Self::ClientItem(error) => error.code(),
            Self::Storage(StorageError::ItemAlreadyInConversation) => "item_already_in_conversation",
            Self::Storage(StorageError::ItemConflict) => "item_conflict",
            Self::PayloadTooLarge(_) => "body_too_large",
            Self::ResourceLimitExceeded { .. } => "response_resource_limit_exceeded",
            Self::InvalidMaxToolCalls(error) => error.code(),
            Self::InvalidInput(error) => error.error_code().unwrap_or("invalid_request_error"),
            Self::UnsupportedValue { code, .. } => code.unwrap_or("unsupported_value"),
            Self::UnsupportedParameter(error) => error.code(),
            Self::Tool(ToolError::UnknownParameter { .. }) => "unknown_parameter",
            Self::McpListToolsFailed(_) => "http_error",
            other => other.error_type(),
        }
    }

    /// Request parameter associated with the API error, when applicable.
    #[must_use]
    pub fn error_param(&self) -> Option<&str> {
        match self.client_visible_error() {
            Self::DirectInference { param, .. } => *param,
            Self::ClientItem(error) => Some(error.param()),
            Self::FileSearch(error) | Self::Tool(ToolError::FileSearch(error)) => match error {
                FileSearchError::FileNotFound { param, .. } => Some(*param),
                FileSearchError::FileRejected { param, .. } => *param,
                _ => None,
            },
            Self::Tool(ToolError::UnknownParameter { param }) => Some(param),
            Self::UnsupportedValue { param, .. } => param.as_deref(),
            Self::Storage(StorageError::ItemAlreadyInConversation) => Some("items"),
            Self::Storage(StorageError::ItemConflict) => Some("item"),
            Self::Storage(StorageError::ItemCursorNotFound { .. }) => Some("after"),
            Self::ConversationLocked { .. } => Some("conversation"),
            Self::Storage(StorageError::PreviousResponseNotFound { .. }) => Some("previous_response_id"),
            Self::Tool(ToolError::MissingOutput { .. } | ToolError::UnknownCallOutput { .. }) => Some("input"),
            Self::InvalidMaxToolCalls(_) => Some(MAX_TOOL_CALLS_PARAM),
            Self::InvalidInput(error) => error.error_param(),
            Self::UnsupportedParameter(error) => Some(error.param()),
            Self::McpListToolsFailed(_) => Some("tools"),
            _ => None,
        }
    }

    /// Client-safe message for the API error envelope.
    #[must_use]
    pub fn error_message(&self) -> String {
        match self.client_visible_error() {
            Self::DirectInference { status, source, .. } if status.is_client_error() => source.to_string(),
            Self::FileSearch(error) | Self::Tool(ToolError::FileSearch(error)) => error.public_message(),
            Self::Storage(
                error @ (StorageError::ItemAlreadyInConversation
                | StorageError::ItemConflict
                | StorageError::ItemCursorNotFound { .. }
                | StorageError::ResponseNotFound { .. }
                | StorageError::PreviousResponseNotFound { .. }
                | StorageError::ConversationNotFound { .. }),
            ) => error.to_string(),
            Self::Storage(StorageError::Database(_)) => "Internal server error".to_owned(),
            // The recorded mutually exclusive envelope carries no gateway prefix.
            Self::InvalidInput(error) if error.is_mutually_exclusive() => error.to_string(),
            Self::Tool(
                error @ (ToolError::MissingOutput { .. }
                | ToolError::UnknownCallOutput { .. }
                | ToolError::UnknownParameter { .. }),
            ) => error.to_string(),
            other => other.to_string(),
        }
    }

    /// Builds the OpenAI-compatible error object shared by HTTP and SSE.
    pub(crate) fn response_error(&self) -> serde_json::Value {
        let code = if matches!(
            self.client_visible_error(),
            Self::Tool(
                ToolError::MissingOutput { .. }
                    | ToolError::UnknownCallOutput { .. }
                    | ToolError::FileSearch(
                        FileSearchError::FileNotFound { .. } | FileSearchError::FileRejected { .. }
                    )
            ) | Self::Storage(
                StorageError::ItemCursorNotFound { .. }
                    | StorageError::ResponseNotFound { .. }
                    | StorageError::ConversationNotFound { .. }
            ) | Self::UnsupportedValue { code: None, .. }
                | Self::FileReferencesNotFound { .. }
                | Self::FileSearch(FileSearchError::FileNotFound { .. } | FileSearchError::FileRejected { .. })
        ) {
            serde_json::Value::Null
        } else {
            serde_json::json!(self.error_code())
        };
        let mut error = serde_json::Map::new();
        error.insert("message".to_owned(), serde_json::json!(self.error_message()));
        error.insert("type".to_owned(), serde_json::json!(self.error_type()));
        error.insert("code".to_owned(), code);
        if let Some(param) = self.error_param() {
            error.insert("param".to_owned(), serde_json::json!(param));
        } else if matches!(
            self.client_visible_error(),
            Self::Storage(StorageError::ResponseNotFound { .. } | StorageError::ConversationNotFound { .. })
                | Self::UnsupportedValue { param: None, .. }
                | Self::FileReferencesNotFound { .. }
                | Self::FileSearch(FileSearchError::FileRejected { param: None, .. })
                | Self::Tool(ToolError::FileSearch(FileSearchError::FileRejected { param: None, .. }))
        ) {
            error.insert("param".to_owned(), serde_json::Value::Null);
        }
        serde_json::Value::Object(error)
    }

    /// Serialise the error into the HTTP response body bytes.
    ///
    /// `LLMRequest` bodies are forwarded verbatim; all other variants are
    /// wrapped in the standard `{"error": {"message", "type", "code"}}` envelope.
    #[must_use]
    pub fn into_response_body(self) -> Vec<u8> {
        match self {
            Self::LLMRequest { body, .. } => body.into_bytes(),
            other => serialize_to_vec_or_default(&serde_json::json!({ "error": other.response_error() })),
        }
    }
}

pub type ExecutorResult<T> = Result<T, ExecutorError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_search_error_envelopes_preserve_request_failure_categories() {
        use crate::types::file_search::FileSearchError;
        for (source, status, category) in [
            (
                FileSearchError::InvalidRequest("bad query".to_owned()),
                400,
                "invalid_request_error",
            ),
            (FileSearchError::NotFound("missing file".to_owned()), 404, "not_found"),
            (
                FileSearchError::Conflict("already attached".to_owned()),
                409,
                "conflict_error",
            ),
            (FileSearchError::Unavailable("disabled".to_owned()), 503, "server_error"),
            (FileSearchError::ProviderProtocol, 502, "upstream_error"),
        ] {
            let error = ExecutorError::from(ToolError::FileSearch(source));
            assert_eq!(error.http_status().as_u16(), status);
            assert_eq!(error.error_type(), category);
            assert_eq!(error.response_error()["type"], category);
            assert!(std::error::Error::source(&error).is_some());
        }
    }

    #[test]
    fn files_errors_use_the_recorded_envelope() {
        let missing = ExecutorError::from(FileSearchError::FileNotFound {
            id: "file-1".to_owned(),
            param: "after",
        });
        assert_eq!(missing.http_status(), StatusCode::NOT_FOUND);
        assert_eq!(
            missing.response_error(),
            serde_json::json!({
                "message": "No such File object: file-1",
                "type": "invalid_request_error",
                "code": null,
                "param": "after"
            })
        );

        let rejected = ExecutorError::from(FileSearchError::FileRejected {
            message: "File is empty.".to_owned(),
            param: None,
        });
        assert_eq!(rejected.http_status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            rejected.response_error(),
            serde_json::json!({
                "message": "File is empty.",
                "type": "invalid_request_error",
                "code": null,
                "param": null
            })
        );
    }

    #[test]
    fn resolved_file_kind_errors_use_the_recorded_input_envelope() {
        use crate::types::file_search::FileObject;
        use crate::types::io::ResponsesInput;
        use crate::types::io::input::ResolvedFiles;

        let input: ResponsesInput = serde_json::from_value(serde_json::json!([
            {"role":"user", "content":[{"type":"input_image", "file_id":"file-1"}]}
        ]))
        .unwrap();
        let object = FileObject {
            id: "file-1".to_owned(),
            object: "file".to_owned(),
            bytes: 2,
            created_at: 0,
            filename: "notes.txt".to_owned(),
            purpose: "user_data".to_owned(),
            status: "processed".to_owned(),
        };
        let mut files = ResolvedFiles::default();
        files.insert("file-1".to_owned(), (object, b"hi".to_vec()));

        let error = ExecutorError::from(input.normalized_model_input(&[], &files).unwrap_err());
        assert_eq!(error.http_status(), StatusCode::BAD_REQUEST);
        let envelope = error.response_error();
        assert_eq!(envelope["type"], "invalid_request_error");
        assert_eq!(envelope["code"], serde_json::Value::Null);
        assert_eq!(envelope["param"], "input");
        assert_eq!(
            envelope["message"],
            "Invalid input: Expected image type to be a supported format: .jpeg, .jpg, .png, .gif, .webp but got .txt."
        );
    }

    #[test]
    fn test_executor_error_display() {
        let err = ExecutorError::InvalidRequest("test message".into());
        assert!(err.to_string().contains("invalid request"));
        assert!(err.to_string().contains("test message"));
    }

    #[test]
    fn test_executor_error_stream() {
        let err = ExecutorError::StreamError("connection lost".into());
        assert!(err.to_string().contains("stream error"));
    }

    #[test]
    fn test_executor_error_not_found() {
        let err = ExecutorError::NotFound {
            entity: "Conversation".into(),
            id: "conv_123".into(),
        };
        assert!(err.to_string().contains("Conversation"));
        assert!(err.to_string().contains("conv_123"));
    }

    #[test]
    fn test_executor_error_from_storage() {
        let storage_err = StorageError::NotConfigured;
        let exec_err = ExecutorError::from(storage_err);
        assert!(exec_err.to_string().contains("storage error"));
    }

    #[test]
    fn previous_response_error_has_the_continuation_envelope() {
        let error = ExecutorError::Storage(StorageError::PreviousResponseNotFound {
            id: "resp_missing".to_owned(),
        });
        assert_eq!(error.http_status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&error.into_response_body()).unwrap(),
            serde_json::json!({"error": {
                "message": "Previous response with id 'resp_missing' not found.",
                "type": "invalid_request_error", "code": "previous_response_not_found",
                "param": "previous_response_id"
            }})
        );
    }

    #[test]
    fn tool_search_configuration_errors_are_bad_requests() {
        let error = ExecutorError::from(ToolError::Config("invalid tool_search request".to_owned()));

        assert_eq!(error.http_status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.error_type(), "invalid_request_error");
    }

    #[test]
    fn test_executor_error_json_preserves_source() {
        use std::error::Error;
        let json_err: serde_json::Error = serde_json::from_str::<serde_json::Value>("{bad}").unwrap_err();
        let exec_err = ExecutorError::from(json_err);
        assert!(exec_err.source().is_some(), "source should be chained");
        assert!(exec_err.to_string().contains("json error"));
    }

    #[test]
    fn conversation_locked_response_preserves_conflict_through_persistence() {
        use std::error::Error;

        let error = ExecutorError::Persistence(Box::new(ExecutorError::ConversationLocked {
            source: StorageError::ConversationConflict {
                conversation_id: "conv_internal".to_owned(),
            },
        }));

        let conversation_locked = error.source().expect("persistence source must be retained");
        let conflict = conversation_locked
            .source()
            .expect("conversation conflict source must be retained");
        assert!(matches!(
            conflict.downcast_ref::<StorageError>(),
            Some(StorageError::ConversationConflict { conversation_id })
                if conversation_id == "conv_internal"
        ));

        assert_eq!(error.http_status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&error.into_response_body())
                .expect("valid error response JSON"),
            serde_json::json!({
                "error": {
                    "message": "conversation changed while the response was being generated; retry the request",
                    "type": "invalid_request_error",
                    "code": "conversation_locked",
                    "param": "conversation"
                }
            })
        );
    }

    #[test]
    fn non_conflict_response_omits_param() {
        let body = ExecutorError::InvalidRequest("invalid input".to_owned()).into_response_body();
        let value: serde_json::Value = serde_json::from_slice(&body).expect("valid error response JSON");

        assert!(!value["error"].as_object().expect("error object").contains_key("param"));
    }

    #[test]
    fn invalid_max_tool_calls_matches_the_openai_parameter_envelope() {
        let below = crate::types::request_response::OutOfRangeInteger::Unsigned(0);
        let error = ExecutorError::from(MaxToolCallsError::BelowMinimum(below));

        assert_eq!(error.http_status(), StatusCode::BAD_REQUEST);
        let value: serde_json::Value = serde_json::from_slice(&error.into_response_body()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"error": {
                "message": "Invalid 'max_tool_calls': integer below minimum value. Expected a value >= 1, but got 0 instead.",
                "type": "invalid_request_error",
                "code": "integer_below_min_value",
                "param": "max_tool_calls"
            }})
        );
    }

    #[test]
    fn response_id_conflict_has_a_machine_readable_conflict_envelope() {
        let error = ExecutorError::Conflict("a turn is already stored under 'resp_1'".to_owned());

        assert_eq!(error.http_status(), StatusCode::CONFLICT);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&error.into_response_body())
                .expect("valid error response JSON"),
            serde_json::json!({
                "error": {
                    "message": "conflict: a turn is already stored under 'resp_1'",
                    "type": "conflict_error",
                    "code": "response_already_stored"
                }
            })
        );
    }

    #[test]
    fn missing_tool_output_matches_openai_error_envelope() {
        let error = ExecutorError::Tool(ToolError::MissingOutput {
            call_id: "call_test".to_owned(),
        });

        assert_eq!(error.http_status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&error.into_response_body())
                .expect("valid error response JSON"),
            serde_json::json!({
                "error": {
                    "message": "No tool output found for function call call_test.",
                    "type": "invalid_request_error",
                    "param": "input",
                    "code": null
                }
            })
        );
    }

    fn envelope(error: ExecutorError) -> serde_json::Value {
        assert_eq!(error.http_status(), StatusCode::BAD_REQUEST);
        serde_json::from_slice(&error.into_response_body()).expect("valid error response JSON")
    }

    // The three async-tool errors below match the bodies recorded from OpenAI in
    // tests/cassettes/async_tools (edge-cases, client-tool-types, multi-agent-parallel).

    #[test]
    fn unknown_call_output_matches_openai_error_envelope() {
        let error = ExecutorError::Tool(ToolError::UnknownCallOutput {
            call_id: "call_async_unknown_probe".to_owned(),
        });
        assert_eq!(
            envelope(error),
            serde_json::json!({
                "error": {
                    "message": "No tool call found for function call output with call_id call_async_unknown_probe.",
                    "type": "invalid_request_error",
                    "param": "input",
                    "code": null
                }
            })
        );
    }

    #[test]
    fn unknown_tool_parameter_matches_openai_error_envelope() {
        let error = ExecutorError::Tool(ToolError::UnknownParameter {
            param: "tools[0].async".to_owned(),
        });
        assert_eq!(
            envelope(error),
            serde_json::json!({
                "error": {
                    "message": "Unknown parameter: 'tools[0].async'.",
                    "type": "invalid_request_error",
                    "param": "tools[0].async",
                    "code": "unknown_parameter"
                }
            })
        );
    }

    #[test]
    fn unsupported_value_matches_openai_error_envelope() {
        let error = ExecutorError::UnsupportedValue {
            param: Some("multi_agent".into()),
            code: Some("unsupported_value"),
            message: "Async parallel tool calls are not yet supported in multi-agent mode.".into(),
        };
        assert_eq!(
            envelope(error),
            serde_json::json!({
                "error": {
                    "message": "Async parallel tool calls are not yet supported in multi-agent mode.",
                    "type": "invalid_request_error",
                    "param": "multi_agent",
                    "code": "unsupported_value"
                }
            })
        );
    }

    #[test]
    fn unsupported_parameters_match_openai_error_envelopes() {
        let background = ExecutorError::from(UnsupportedParameterError::BackgroundMode);
        assert_eq!(background.http_status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            envelope(background),
            serde_json::json!({"error": {
                "message": "background mode is not supported; omit `background` or set it to false",
                "type": "invalid_request_error",
                "code": "unsupported_value",
                "param": "background"
            }})
        );
        assert_eq!(
            envelope(ExecutorError::from(UnsupportedParameterError::PromptTemplate)),
            serde_json::json!({"error": {
                "message": "reusable prompt templates are not supported; send `instructions` and `input` instead of `prompt`",
                "type": "invalid_request_error",
                "code": "unsupported_parameter",
                "param": "prompt"
            }})
        );
        assert_eq!(
            envelope(ExecutorError::from(UnsupportedParameterError::BackgroundType(
                crate::types::request_response::JsonKind::Object
            ))),
            serde_json::json!({"error": {
                "message": "Invalid type for 'background': expected a boolean, but got an object instead.",
                "type": "invalid_request_error",
                "code": "invalid_type",
                "param": "background"
            }})
        );
    }
}

#[cfg(test)]
mod storage_error_mapping_tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn database_details_remain_in_the_source_but_not_the_response() {
        let error = ExecutorError::Storage(StorageError::Database(sqlx::Error::Protocol(
            "UNIQUE constraint failed: items.id".to_owned(),
        )));
        assert!(error.source().unwrap().to_string().contains("items.id"));
        assert_eq!(error.http_status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body: serde_json::Value = serde_json::from_slice(&error.into_response_body()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"error": {
                "message": "Internal server error", "type": "server_error", "code": "server_error"
            }})
        );
    }
}
