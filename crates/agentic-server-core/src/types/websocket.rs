//! Typed client events for Responses WebSocket sessions.
use serde::{Deserialize, Serialize};

use super::{injection::ResponseInjectRequest, request_response::RequestPayload};

/// Create uses the Responses request fields at the top level of the event.
/// Lane validation and connection admission remain responsibilities of the adapter.
#[derive(Debug, Deserialize, Serialize)]
pub struct ResponseCreateRequest {
    #[serde(flatten)]
    pub payload: RequestPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
}

/// The discriminator belongs to the event envelope, just as it does for injection outcomes.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum ResponseClientEvent {
    #[serde(rename = "response.create")]
    Create(Box<ResponseCreateRequest>),
    #[serde(rename = "response.inject")]
    Inject(ResponseInjectRequest),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_preserves_request_fields_and_session_options() {
        let event: ResponseClientEvent = serde_json::from_str(
            r#"{"type":"response.create","model":"model","input":"hello",
                "previous_response_id":"resp_previous","generate":false,"stream_id":"lane"}"#,
        )
        .unwrap();
        let ResponseClientEvent::Create(request) = &event else {
            panic!("expected create");
        };
        assert_eq!(request.payload.previous_response_id.as_deref(), Some("resp_previous"));
        assert_eq!(request.generate, Some(false));
        assert_eq!(request.stream_id.as_deref(), Some("lane"));
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "response.create");
        assert_eq!(wire["model"], "model");
        assert_eq!(wire["generate"], false);
        assert!(wire.get("payload").is_none());
    }

    #[test]
    fn rejects_invalid_create_options_and_unknown_events() {
        for wire in [
            r#"{"type":"response.create","model":"model","input":"hello","generate":"false"}"#,
            r#"{"type":"response.create","model":"model","input":"hello","stream_id":5}"#,
            r#"{"type":"response.unknown","model":"model"}"#,
        ] {
            assert!(serde_json::from_str::<ResponseClientEvent>(wire).is_err());
        }
    }

    #[test]
    fn injection_uses_the_same_discriminated_envelope() {
        let event: ResponseClientEvent = serde_json::from_str(
            r#"{"type":"response.inject","response_id":"resp_1",
                "input":[{"type":"function_call_output","call_id":"call_1","output":"ok"}]}"#,
        )
        .unwrap();
        assert!(matches!(event, ResponseClientEvent::Inject(_)));
        let wire = serde_json::to_value(event).unwrap();
        assert_eq!(wire["type"], "response.inject");
        assert_eq!(wire["input"][0]["call_id"], "call_1");
    }
}
