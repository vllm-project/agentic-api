use http::StatusCode;
use serde_json::{Value, json};
use thiserror::Error;

use agentic_core::executor::ExecutorError;

#[derive(Debug, Error)]
pub(super) enum WsError {
    #[error(transparent)]
    Executor(#[from] ExecutorError),

    #[error("invalid JSON: {0}")]
    InvalidJson(#[source] serde_json::Error),

    #[error("failed to serialize websocket event: {0}")]
    SerializeJson(#[source] serde_json::Error),

    #[error("websocket message type must be response.create")]
    UnexpectedType,

    #[error("websocket messages must be JSON text frames")]
    BinaryFrame,

    #[error("websocket send failed")]
    SendFailed,

    #[error("websocket client disconnected")]
    ClientDisconnected,

    #[error("websocket shutdown requested")]
    Shutdown,

    #[error("websocket receive failed: {0}")]
    Receive(String),
}

impl WsError {
    pub(super) fn status(&self) -> StatusCode {
        match self {
            Self::Executor(err) => err.http_status(),
            Self::InvalidJson(_) | Self::UnexpectedType | Self::BinaryFrame => StatusCode::BAD_REQUEST,
            Self::SerializeJson(_)
            | Self::SendFailed
            | Self::ClientDisconnected
            | Self::Shutdown
            | Self::Receive(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub(super) fn code(&self) -> &'static str {
        match self {
            Self::Executor(err) => err.error_code(),
            Self::InvalidJson(_) => "invalid_json",
            Self::UnexpectedType | Self::BinaryFrame => "invalid_request_error",
            Self::SerializeJson(_)
            | Self::SendFailed
            | Self::ClientDisconnected
            | Self::Shutdown
            | Self::Receive(_) => "server_error",
        }
    }

    pub(super) fn to_ws_frame(&self) -> Option<Value> {
        if matches!(
            self,
            Self::SerializeJson(_) | Self::SendFailed | Self::ClientDisconnected | Self::Shutdown | Self::Receive(_)
        ) {
            return None;
        }

        let code = self.code();
        Some(json!({
            "type": "error",
            "status": self.status().as_u16(),
            "error": {
                "message": self.to_string(),
                "type": code,
                "code": code
            }
        }))
    }
}
