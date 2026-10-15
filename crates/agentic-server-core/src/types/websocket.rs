//! Typed client events and steering outcomes for Responses WebSocket sessions.
use serde::{Deserialize, Serialize};

use super::client_calls::ClientCallId;
use super::io::{InputContent, InputItem, InputMessageContent, ResponsesInput};
use super::tools::ToolSearchExecution;
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
    #[serde(rename = "response.steer")]
    Steer(ResponseSteerRequest),
}

/// Valid steering fields, without the client-event discriminator.
/// Missing targets and invalid-input echoes belong to bounded adapter decoding.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseSteerRequest {
    pub previous_response_id: String,
    pub input: SteeringInput,
}

/// Existing Responses input restricted to nonempty, user-only message arrays
/// or text (including empty text). Content uses the existing `InputContent`.
///
/// Serde invokes `TryFrom<ResponsesInput>` automatically when decoding, so the
/// same validation applies to JSON and programmatic construction.
///
/// Shared input types normalize optional fields and discard unknown message
/// keys. The adapter must check original message fields before decoding and
/// retain bounded original input until commitment or failure for exact echoes.
#[derive(Debug, Deserialize, Serialize)]
#[serde(try_from = "ResponsesInput")]
pub struct SteeringInput(ResponsesInput);

impl AsRef<ResponsesInput> for SteeringInput {
    fn as_ref(&self) -> &ResponsesInput {
        &self.0
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SteeringInputError {
    #[error("steering message input must contain at least one user message")]
    EmptyMessages,
    #[error("steering accepts only user messages without history metadata")]
    InvalidMessage,
    #[error("content type '{0}' is not allowed in steering input")]
    InvalidContent(String),
    #[error("content extension '{0}' conflicts with a modeled field")]
    ConflictingContentField(String),
}

impl TryFrom<ResponsesInput> for SteeringInput {
    type Error = SteeringInputError;

    fn try_from(input: ResponsesInput) -> Result<Self, Self::Error> {
        if let ResponsesInput::Items(items) = &input {
            if items.is_empty() {
                return Err(SteeringInputError::EmptyMessages);
            }
            for item in items {
                let InputItem::Message(message) = item else {
                    return Err(SteeringInputError::InvalidMessage);
                };
                if message.role != "user"
                    || message.id.is_some()
                    || message.agent.is_some()
                    || message.phase.is_some()
                    || message.status.is_some()
                {
                    return Err(SteeringInputError::InvalidMessage);
                }
                Self::validate_content(&message.content)?;
            }
        }
        Ok(Self(input))
    }
}

/// Move accepted input into ordinary continuation preparation without cloning.
impl From<SteeringInput> for ResponsesInput {
    fn from(input: SteeringInput) -> Self {
        input.0
    }
}

impl SteeringInput {
    fn validate_content(content: &InputMessageContent) -> Result<(), SteeringInputError> {
        let InputMessageContent::Parts(parts) = content else {
            return Ok(());
        };
        for part in parts {
            match part {
                InputContent::InputText(text) => Self::validate_extensions(&text.extra, &[("text", true)])?,
                InputContent::InputImage(image) => Self::validate_extensions(
                    &image.extra,
                    &[
                        ("file_id", image.file_id.is_some()),
                        ("image_url", image.image_url.is_some()),
                        ("detail", image.detail.is_some()),
                    ],
                )?,
                InputContent::InputFile(file) => Self::validate_extensions(
                    &file.extra,
                    &[
                        ("file_data", file.file_data.is_some()),
                        ("file_id", file.file_id.is_some()),
                        ("file_url", file.file_url.is_some()),
                        ("filename", file.filename.is_some()),
                        ("detail", file.detail.is_some()),
                    ],
                )?,
                InputContent::OutputText(_) => {
                    return Err(SteeringInputError::InvalidContent("output_text".to_owned()));
                }
                InputContent::Refusal(_) => return Err(SteeringInputError::InvalidContent("refusal".to_owned())),
                InputContent::ReasoningText(_) => {
                    return Err(SteeringInputError::InvalidContent("reasoning_text".to_owned()));
                }
                InputContent::Unknown(kind) => return Err(SteeringInputError::InvalidContent(kind.clone())),
            }
        }
        Ok(())
    }

    fn validate_extensions(
        extra: &serde_json::Map<String, serde_json::Value>,
        fields: &[(&str, bool)],
    ) -> Result<(), SteeringInputError> {
        if extra.contains_key("type") {
            return Err(SteeringInputError::ConflictingContentField("type".to_owned()));
        }
        for (field, populated) in fields {
            if extra.get(*field).is_some_and(|value| *populated || !value.is_null()) {
                return Err(SteeringInputError::ConflictingContentField((*field).to_owned()));
            }
        }
        Ok(())
    }
}

/// Accepted/pending descriptors identify ownership, without echoing input.
#[derive(Debug, Clone, Serialize)]
pub struct AcceptedSteer {
    pub id: String,
    pub previous_response_id: String,
}

/// Typed failure payload. Before delivery, the adapter must substitute the
/// retained original input, including for accepted steering that later fails.
#[derive(Debug, Serialize)]
pub struct FailedSteer {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub previous_response_id: Option<String>,
    pub input: SteeringInputEcho,
}

/// Original submitted JSON for failure echoes, including rejected input shapes.
/// Admission uses `SteeringInput`; an echo is never executable input.
/// Adapters retain this bounded fragment until commitment or failure delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SteeringInputEcho(Box<serde_json::value::RawValue>);

#[derive(Debug, thiserror::Error)]
pub enum SteeringInputEchoError {
    #[error("steering input exceeds the retained-byte limit of {max_bytes}")]
    TooLarge { max_bytes: usize },
    #[error("steering input echo is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl SteeringInputEcho {
    /// Retain a received JSON fragment after checking its byte limit.
    /// # Errors
    /// Rejects oversized fragments before parsing, and invalid JSON.
    pub fn from_json(json: String, max_bytes: usize) -> Result<Self, SteeringInputEchoError> {
        if json.len() > max_bytes {
            return Err(SteeringInputEchoError::TooLarge { max_bytes });
        }
        Ok(Self(serde_json::value::RawValue::from_string(json)?))
    }

    #[must_use]
    pub fn as_json(&self) -> &str {
        self.0.get()
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringPendingReason {
    WaitingForRequiredInput,
}

/// Identifying stubs for qualified client-executed call kinds, without outputs.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum SteeringRequiredInput {
    #[serde(rename = "function_call_output")]
    Function { call_id: ClientCallId, name: String },
    #[serde(rename = "custom_tool_call_output")]
    Custom { call_id: ClientCallId },
    #[serde(rename = "shell_call_output")]
    Shell { call_id: ClientCallId },
    #[serde(rename = "tool_search_output")]
    ToolSearch {
        call_id: ClientCallId,
        execution: ToolSearchExecution,
    },
}

/// Documented codes; support decisions and failure precedence belong to core policy.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringFailureCode {
    ResponseNotFound,
    InvalidInput,
    SteeringNotSupported,
    TooManyPendingSteers,
    ResponseAlreadyCompleted,
    ResponseNotActive,
    SuccessorCreationFailed,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename = "invalid_request_error")]
pub struct SteeringFailure {
    pub code: SteeringFailureCode,
    pub message: String,
}

/// Delivery assigns sequence numbers and lane metadata through its existing envelope.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum SteeringEvent {
    #[serde(rename = "response.steer.accepted")]
    Accepted { steer: AcceptedSteer },
    #[serde(rename = "response.steer.pending")]
    Pending {
        steer: AcceptedSteer,
        reason: SteeringPendingReason,
        required_input: Vec<SteeringRequiredInput>,
    },
    #[serde(rename = "response.steer.failed")]
    Failed { steer: FailedSteer, error: SteeringFailure },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::{InputImageContent, InputMessage, InputTextContent};
    use serde_json::json;

    #[test]
    fn failed_input_echo_preserves_rejected_shape_and_checks_byte_limit() {
        let original = r#"[{"role":"user","content":null,"id":null,"unknown":1}]"#;
        let echo = SteeringInputEcho::from_json(original.to_owned(), original.len()).unwrap();
        assert_eq!(echo.as_json(), original);
        assert_eq!(
            crate::utils::common::serialize_to_value(&echo).unwrap(),
            json!([
                {"role":"user","content":null,"id":null,"unknown":1}
            ])
        );
        assert!(matches!(
            SteeringInputEcho::from_json(original.to_owned(), original.len() - 1),
            Err(SteeringInputEchoError::TooLarge { .. })
        ));
        assert!(matches!(
            SteeringInputEcho::from_json("[".to_owned(), 4096),
            Err(SteeringInputEchoError::Json(_))
        ));
    }

    #[test]
    fn steering_preserves_shared_strings_messages_media_and_extensions() {
        for input in [
            json!(""),
            json!("Keep the scope small."),
            json!([{"type":"message","role":"user","content":"first"},{"type":"message","role":"user","content":"second"}]),
            json!([{"type":"message","role":"user","content":[
                {"type":"input_text","text":"read this","prompt_cache_breakpoint":{"mode":"explicit"}},
                {"type":"input_image","image_url":"https://example.com/image.png","detail":"auto"},
                {"type":"input_file","file_id":"file_1","filename":"notes.txt"}
            ]}]),
        ] {
            let wire = json!({"previous_response_id":"resp_1","input":input});
            let request: ResponseSteerRequest = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(request).unwrap(), wire);
        }
    }

    #[test]
    fn steering_validates_shared_input_automatically_during_deserialization() {
        for input in [
            json!([]),
            json!(null),
            json!([{"role":"assistant","content":"hello"}]),
            json!([{"role":"system","content":"hello"}]),
            json!([{"type":"function_call_output","call_id":"call_1","output":"ok"}]),
            json!([{"type":"other","role":"user","content":"hello"}]),
            json!([{"role":"user","content":"hello","id":"message_1"}]),
            json!([{"role":"user","content":"hello","status":"completed"}]),
            json!([{"role":"user","content":[{"type":"output_text","text":"hello"}]}]),
            json!([{"role":"user","content":[{"type":"unknown","text":"hello"}]}]),
        ] {
            assert!(serde_json::from_value::<SteeringInput>(input).is_err());
        }
        for extra in ["stream_id", "instructions", "model"] {
            let mut wire = json!({"previous_response_id":"resp_1","input":"hello"});
            wire[extra] = json!("unexpected");
            assert!(serde_json::from_value::<ResponseSteerRequest>(wire).is_err());
        }
    }

    #[test]
    fn steering_programmatic_input_uses_existing_types_and_enforces_validation() {
        assert!(SteeringInput::try_from(ResponsesInput::Items(Vec::new())).is_err());
        for role in ["assistant", "system"] {
            let message = InputMessage {
                role: role.to_owned(),
                ..InputMessage::default()
            };
            assert!(SteeringInput::try_from(ResponsesInput::Items(vec![InputItem::Message(message)])).is_err());
        }
        let message = InputMessage {
            role: "user".to_owned(),
            content: InputMessageContent::Parts(vec![InputContent::OutputText(InputTextContent::new("no"))]),
            ..InputMessage::default()
        };
        assert!(SteeringInput::try_from(ResponsesInput::Items(vec![InputItem::Message(message)])).is_err());
        let mut image = InputImageContent {
            image_url: Some("https://example.com/image.png".to_owned()),
            ..InputImageContent::default()
        };
        image.extra.insert("image_url".to_owned(), json!(null));
        let message = InputMessage {
            role: "user".to_owned(),
            content: InputMessageContent::Parts(vec![InputContent::InputImage(image)]),
            ..InputMessage::default()
        };
        assert!(SteeringInput::try_from(ResponsesInput::Items(vec![InputItem::Message(message)])).is_err());
        let input: SteeringInput = serde_json::from_value(json!([
            {"role":"user","content":"first"}, {"role":"user","content":[{"type":"input_text","text":"second"}]}
        ]))
        .unwrap();
        assert!(matches!(input.as_ref(), ResponsesInput::Items(items) if items.len() == 2));
        let input = ResponsesInput::from(input);
        assert!(matches!(&input, ResponsesInput::Items(items) if items.len() == 2));
        let wire = serde_json::to_value(&input).unwrap();
        assert_eq!(wire[0]["type"], "message");
        assert_eq!(wire[0]["content"], "first");
        assert_eq!(wire[1]["content"][0]["text"], "second");
        let input = SteeringInput::try_from(input).unwrap();
        assert_eq!(serde_json::to_value(input).unwrap(), wire);
    }

    #[test]
    fn steering_outcomes_match_descriptors_with_typed_input() {
        let accepted = AcceptedSteer {
            id: "steer_1".to_owned(),
            previous_response_id: "resp_1".to_owned(),
        };
        let wire = serde_json::to_value(SteeringEvent::Accepted {
            steer: accepted.clone(),
        })
        .unwrap();
        assert_eq!(
            wire,
            json!({"type":"response.steer.accepted","steer":{"id":"steer_1","previous_response_id":"resp_1"}})
        );
        let pending = SteeringEvent::Pending {
            steer: accepted,
            reason: SteeringPendingReason::WaitingForRequiredInput,
            required_input: vec![
                SteeringRequiredInput::Function {
                    call_id: "call_1".to_owned().try_into().unwrap(),
                    name: "lookup".to_owned(),
                },
                SteeringRequiredInput::Custom {
                    call_id: "call_2".to_owned().try_into().unwrap(),
                },
            ],
        };
        let wire = serde_json::to_value(pending).unwrap();
        assert_eq!(
            wire["required_input"],
            json!([
                {"type":"function_call_output","call_id":"call_1","name":"lookup"},
                {"type":"custom_tool_call_output","call_id":"call_2"}
            ])
        );
        assert_eq!(wire["reason"], "waiting_for_required_input");
        assert!(wire["steer"].get("input").is_none());
        assert!(wire.get("sequence_number").is_none());
        assert!(wire.get("stream_id").is_none());
        let input = json!([{"type":"message","role":"user","content":[{"type":"input_image","image_url":"https://example.com/image.png"}]}]);
        for id in [None, Some("steer_1".to_owned())] {
            let expected_id = id.as_ref().map(|id| json!(id));
            let event = SteeringEvent::Failed {
                steer: FailedSteer {
                    id,
                    previous_response_id: None,
                    input: SteeringInputEcho::from_json(
                        crate::utils::common::serialize_to_string(&input).unwrap(),
                        4096,
                    )
                    .unwrap(),
                },
                error: SteeringFailure {
                    code: SteeringFailureCode::ResponseNotFound,
                    message: "Unavailable target.".to_owned(),
                },
            };
            let wire = serde_json::to_value(event).unwrap();
            assert_eq!(wire["steer"].get("id"), expected_id.as_ref());
            assert_eq!(wire["steer"]["previous_response_id"], json!(null));
            assert_eq!(wire["steer"]["input"], input);
            assert_eq!(
                wire["error"],
                json!({"type":"invalid_request_error","code":"response_not_found","message":"Unavailable target."})
            );
        }
    }

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
