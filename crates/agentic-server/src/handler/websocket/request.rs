//! WebSocket `response.create` envelope parsing and request admission shape.

use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use agentic_core::executor::ExecutorError;
use agentic_core::types::request_response::RequestPayload;

use super::error::WsError;
use super::responses::event::StreamId;
use super::responses::telemetry::QueuedExecution;
use crate::handler::opaque_request::{OpaqueRequestTransport, validate_opaque_request_fields};

pub(super) struct WsRequest {
    pub(super) payload: RequestPayload,
    pub(super) stream_id: Option<StreamId>,
    pub(super) generate: Option<bool>,
    /// Set when the request is queued; dispatch continues its execution span.
    pub(super) execution: Option<QueuedExecution>,
}

#[derive(Debug)]
pub(super) struct WsRequestParseError {
    pub(super) previous_response_id: Option<String>,
    pub(super) error: WsError,
    pub(super) stream_id: Option<StreamId>,
}

pub(super) fn stream_id_from_text(text: &str) -> Option<StreamId> {
    #[derive(Deserialize)]
    struct StreamIdEnvelope {
        stream_id: Option<StreamId>,
    }

    serde_json::from_str::<StreamIdEnvelope>(text).ok()?.stream_id
}

pub(super) fn parse_ws_request(text: &str, opaque_profile_selected: bool) -> Result<WsRequest, WsRequestParseError> {
    let value = serde_json::from_str::<Value>(text).map_err(|error| WsRequestParseError {
        error: WsError::InvalidJson(error),
        previous_response_id: None,
        stream_id: None,
    })?;
    if opaque_profile_selected {
        validate_opaque_request_fields(text.as_bytes(), OpaqueRequestTransport::WebSocket).map_err(|error| {
            WsRequestParseError {
                error: WsError::from(ExecutorError::from(error)),
                previous_response_id: None,
                stream_id: None,
            }
        })?;
    }
    let stream_id = value
        .get("stream_id")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| "stream_id must be a string".to_owned())
                .and_then(StreamId::try_from)
        })
        .transpose()
        .map_err(|error| WsRequestParseError {
            error: WsError::from(ExecutorError::InvalidRequest(error)),
            previous_response_id: None,
            stream_id: None,
        })?;

    if value.get("type").and_then(Value::as_str) != Some("response.create") {
        return Err(WsRequestParseError {
            error: WsError::UnexpectedType,
            previous_response_id: None,
            stream_id,
        });
    }

    // Only valid routing plus response.create may identify a checkpoint for eviction.
    // In particular, an explicit null/invalid stream_id must not target the default lane.
    let previous_response_id = value
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let generate = value.get("generate").and_then(Value::as_bool);
    let mut payload = serde_json::from_value::<RequestPayload>(value).map_err(|error| WsRequestParseError {
        error: WsError::from(ExecutorError::from(error)),
        previous_response_id,
        stream_id: stream_id.clone(),
    })?;
    let requested_stream = payload.stream;
    payload.stream = true;
    debug!(
        requested_stream,
        forced_stream = payload.stream,
        store = payload.store,
        has_previous_response_id = payload.previous_response_id.is_some(),
        has_conversation_id = payload.conversation_id.is_some(),
        stream_id = stream_id.as_ref().map(StreamId::as_str),
        ?generate,
        tools = payload.tools.as_ref().map_or(0, Vec::len),
        "accepted websocket response.create"
    );

    Ok(WsRequest {
        payload,
        stream_id,
        generate,
        execution: None,
    })
}
