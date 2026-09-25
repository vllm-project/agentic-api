//! Bounded outbound WebSocket events and stream routing metadata.

use super::WsError;
use crate::app::AppState;
use agentic_core::executor::{ExecutorError, ResourceLimit};
use serde::Deserialize;
use serde_json::Value;

/// Slack reserved beyond the exact `stream_id` member for re-serialization of
/// a parsed executor frame (number formatting, key order) so the executor's
/// own frame check remains a sufficient guard for the WebSocket envelope.
pub(super) const WS_ROUTING_SLACK_BYTES: usize = 256;
pub(super) const WS_MAX_STREAM_ID_CHARS: usize = 256;

/// Maximum serialized size of one outbound WebSocket event, including routing
/// metadata. Derived from the gateway's configured `max_stream_event_bytes` so
/// the socket can deliver every frame the executor is allowed to produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WsEventLimit(pub(super) usize);

impl WsEventLimit {
    pub(super) fn from_state(state: &AppState) -> Self {
        Self(state.exec_ctx.responses_config.max_stream_event_bytes)
    }

    pub(super) fn bytes(self) -> usize {
        self.0
    }

    /// The limit the executor must apply to its own frames so that, once the
    /// `stream_id` routing member is attached, the event still fits this
    /// transport. Validated by the executor before persistence or checkpoint
    /// publication, so an undeliverable terminal frame never leaves a stored
    /// response behind.
    pub(super) fn executor_limit(self, stream_id: Option<&StreamId>) -> usize {
        self.0.saturating_sub(ws_routing_overhead(stream_id))
    }
}

/// Bytes the WebSocket envelope adds to an executor frame: the serialized
/// `"stream_id":<json string>,` member plus re-serialization slack.
pub(super) fn ws_routing_overhead(stream_id: Option<&StreamId>) -> usize {
    let member = stream_id.map_or(0, |stream_id| {
        let value = serde_json::to_string(stream_id.as_str()).map_or(stream_id.as_str().len() * 2 + 2, |s| s.len());
        "\"stream_id\":".len() + value + 1
    });
    member + WS_ROUTING_SLACK_BYTES
}

/// Serialized and size-checked before entering the bounded outbound queue.
pub(super) struct WsOutboundEvent(pub(super) String);

impl WsOutboundEvent {
    pub(super) fn new(value: Value, stream_id: Option<&StreamId>, limit: WsEventLimit) -> Result<Self, WsError> {
        let value = attach_stream_id(value, stream_id)?;
        let text = serde_json::to_string(&value).map_err(WsError::SerializeJson)?;
        if text.len() > limit.bytes() {
            return Err(WsError::from(ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::StreamEvent,
                max_bytes: limit.bytes(),
            }));
        }
        Ok(Self(text))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(try_from = "String")]
pub(in crate::handler::websocket) struct StreamId(String);

impl StreamId {
    pub(in crate::handler::websocket) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for StreamId {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let character_count = value.chars().count();
        if (1..=WS_MAX_STREAM_ID_CHARS).contains(&character_count) {
            Ok(Self(value))
        } else {
            Err(format!(
                "stream_id must contain between 1 and {WS_MAX_STREAM_ID_CHARS} characters"
            ))
        }
    }
}

impl TryFrom<&str> for StreamId {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::try_from(value.to_owned())
    }
}

pub(super) fn attach_stream_id(mut value: Value, stream_id: Option<&StreamId>) -> Result<Value, WsError> {
    let event = value.as_object_mut().ok_or_else(|| {
        WsError::from(ExecutorError::StreamError(
            "upstream WebSocket event must be a JSON object".to_owned(),
        ))
    })?;
    if let Some(stream_id) = stream_id {
        event.insert("stream_id".to_owned(), Value::String(stream_id.as_str().to_owned()));
    } else {
        event.remove("stream_id");
    }
    Ok(value)
}
