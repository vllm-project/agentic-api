//! Decode response creation and routing fields at the WebSocket boundary.
use agentic_core::executor::ExecutorError;
use agentic_core::types::request_response::RequestPayload;
use agentic_core::types::websocket::ResponseCreateRequest;
use agentic_core::utils::common::deserialize_from_str;
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use super::super::error::WsError;
use super::control::SteeringTarget;
use super::{StreamId, telemetry};
use std::sync::Arc;

pub(super) struct WsRequest {
    pub(super) payload: RequestPayload,
    pub(super) stream_id: Option<StreamId>,
    pub(super) generate: Option<bool>,
    pub(super) execution: Option<telemetry::QueuedExecution>,
    pub(super) steering: Option<Arc<SteeringTarget>>,
    pub(super) automatic: bool,
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

/// Inspect only the discriminator; response.create is decoded by its lane-aware
/// parser so a large creation payload is not allocated and discarded twice.
pub(super) fn is_injection(text: &str) -> bool {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(rename = "type")]
        kind: Option<String>,
    }

    serde_json::from_str::<Envelope>(text).is_ok_and(|event| event.kind.as_deref() == Some("response.inject"))
}

pub(super) fn is_steering(text: &str) -> bool {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(rename = "type")]
        kind: Option<String>,
    }
    deserialize_from_str::<Envelope>(text).is_ok_and(|event| event.kind.as_deref() == Some("response.steer"))
}

pub(super) fn parse_ws_request(text: &str) -> Result<WsRequest, WsRequestParseError> {
    let mut value = serde_json::from_str::<Value>(text).map_err(|error| WsRequestParseError {
        error: WsError::InvalidJson(error),
        previous_response_id: None,
        stream_id: None,
    })?;
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
    // The event discriminator is transport metadata, not a provider extension.
    if let Some(fields) = value.as_object_mut() {
        fields.remove("type");
    }
    let create = serde_json::from_value::<ResponseCreateRequest>(value).map_err(|error| WsRequestParseError {
        error: WsError::from(ExecutorError::from(error)),
        previous_response_id,
        stream_id: stream_id.clone(),
    })?;
    let ResponseCreateRequest {
        mut payload, generate, ..
    } = create;
    let requested_stream = payload.stream;
    payload.stream = true;
    debug!(
        requested_stream,
        forced_stream = payload.stream,
        store = payload.store,
        has_previous_response_id = payload.previous_response_id.is_some(),
        has_conversation_id = payload.conversation.is_some(),
        stream_id = stream_id.as_ref().map(StreamId::as_str),
        ?generate,
        tools = payload.tools.as_ref().map_or(0, Vec::len),
        "accepted websocket response.create"
    );

    Ok(WsRequest {
        execution: None,
        steering: None,
        automatic: false,
        payload,
        stream_id,
        generate,
    })
}

/// The boundary retains original input separately from its validated model input.
/// Shared message deserialization intentionally drops unknown fields, so validation
/// checks the original shape before invoking the existing typed conversion.
pub(super) struct SteeringSubmission {
    pub(super) previous_response_id: Option<String>,
    pub(super) echo: agentic_core::types::websocket::SteeringInputEcho,
    pub(super) input: Option<agentic_core::types::websocket::SteeringInput>,
}

impl SteeringSubmission {
    pub(super) fn parse(text: &str, max_bytes: usize) -> Result<Self, WsError> {
        use agentic_core::{
            types::websocket::{SteeringInput, SteeringInputEcho},
            utils::common::{deserialize_from_value, serialize_to_string},
        };
        let mut envelope =
            deserialize_from_str::<serde_json::Map<String, Value>>(text).map_err(WsError::InvalidJson)?;
        let previous_response_id = envelope
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let invalid_envelope = envelope
            .keys()
            .any(|key| !matches!(key.as_str(), "type" | "previous_response_id" | "input"));
        let original = envelope.remove("input").unwrap_or(Value::Null);
        let echo = SteeringInputEcho::from_json(
            serialize_to_string(&original).map_err(WsError::SerializeJson)?,
            max_bytes,
        )
        .map_err(|error| WsError::from(ExecutorError::InvalidRequest(error.to_string())))?;
        let input = if !invalid_envelope && Self::valid_original_messages(&original) {
            deserialize_from_value::<SteeringInput>(original).ok()
        } else {
            None
        };
        Ok(Self {
            previous_response_id,
            echo,
            input,
        })
    }

    fn valid_original_messages(value: &Value) -> bool {
        if value.is_string() {
            return true;
        }
        let Some(items) = value.as_array() else {
            return false;
        };
        !items.is_empty()
            && items.iter().all(|item| {
                let Some(message) = item.as_object() else {
                    return false;
                };
                message
                    .keys()
                    .all(|key| matches!(key.as_str(), "type" | "role" | "content"))
                    && message.get("type").is_none_or(|kind| kind.as_str() == Some("message"))
                    && message.get("role").and_then(Value::as_str) == Some("user")
            })
    }
}

#[cfg(test)]
mod steering_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn original_message_metadata_is_rejected_before_shared_deserialization() {
        for metadata in ["id", "status", "phase", "agent", "unknown"] {
            let mut message = json!({"type":"message", "role":"user", "content":"hello"});
            message[metadata] = Value::Null;
            let original = json!([message]);
            let text =
                json!({"type":"response.steer", "previous_response_id":"resp_parent", "input":original}).to_string();
            let submission = SteeringSubmission::parse(&text, 4096).unwrap();
            assert!(submission.input.is_none());
            assert_eq!(
                deserialize_from_str::<Value>(submission.echo.as_json()).unwrap(),
                original
            );
        }
    }

    #[test]
    fn steering_reuses_shared_input_and_preserves_media_nulls_in_failure_echo() {
        let original = json!([{"type":"message", "role":"user", "content":[{"type":"input_image", "image_url":"https://example.org/a.png", "detail":null}]}]);
        let text = json!({"type":"response.steer", "previous_response_id":"resp_parent", "input":original}).to_string();
        let submission = SteeringSubmission::parse(&text, 4096).unwrap();
        assert!(submission.input.is_some());
        assert_eq!(
            deserialize_from_str::<Value>(submission.echo.as_json()).unwrap(),
            original
        );
        assert!(SteeringSubmission::parse(&text, 8).is_err());
    }

    #[test]
    fn steering_forbids_lane_fields_and_tool_outputs_and_accepts_empty_text() {
        for input in [
            Value::Null,
            json!([]),
            json!([{"type":"function_call_output", "call_id":"call_1", "output":"done"}]),
        ] {
            let text =
                json!({"type":"response.steer", "previous_response_id":"resp_parent", "input":input}).to_string();
            assert!(SteeringSubmission::parse(&text, 4096).unwrap().input.is_none());
        }
        let text =
            json!({"type":"response.steer", "previous_response_id":"resp_parent", "input":"", "stream_id":"lane"})
                .to_string();
        assert!(SteeringSubmission::parse(&text, 4096).unwrap().input.is_none());
        let text = json!({"type":"response.steer", "previous_response_id":"resp_parent", "input":""}).to_string();
        assert!(SteeringSubmission::parse(&text, 4096).unwrap().input.is_some());
    }
}
