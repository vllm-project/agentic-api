//! Errors the gateway raises itself, such as rejected credentials and rate
//! limits, answered in the error envelope of the API the route serves.
//!
//! The response carries the envelope and content type; callers add their own
//! headers, such as `WWW-Authenticate` or retry hints.

use agentic_core::utils::common::{serialize_to_vec_or_default, uuid7_str};
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use serde::Serialize;

use crate::auth::{ANTHROPIC_COUNT_TOKENS_PATH, ANTHROPIC_MESSAGES_PATH};

/// The error envelope a route answers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorFormat {
    OpenAi,
    Anthropic,
}

impl ErrorFormat {
    /// The Messages routes answer in the Anthropic format; every other route
    /// in the `OpenAI` format.
    pub(crate) fn for_path(path: &str) -> Self {
        if matches!(path, ANTHROPIC_MESSAGES_PATH | ANTHROPIC_COUNT_TOKENS_PATH) {
            Self::Anthropic
        } else {
            Self::OpenAi
        }
    }
}

/// One gateway error, with its type in each API's vocabulary.
pub(crate) struct GatewayError<'a> {
    pub(crate) status: StatusCode,
    /// The `OpenAI` `error.type`.
    pub(crate) openai_type: &'static str,
    /// The Anthropic `error.type`.
    pub(crate) anthropic_type: &'static str,
    /// The `OpenAI` `error.code`.
    pub(crate) code: &'static str,
    pub(crate) message: &'a str,
}

#[derive(Serialize)]
struct OpenAiErrorEnvelope<'a> {
    error: OpenAiError<'a>,
}

#[derive(Serialize)]
struct OpenAiError<'a> {
    message: &'a str,
    #[serde(rename = "type")]
    error_type: &'a str,
    /// Always `null`: these errors are not about one request parameter.
    param: Option<&'a str>,
    code: &'a str,
}

#[derive(Serialize)]
#[serde(tag = "type", rename = "error")]
struct AnthropicErrorEnvelope<'a> {
    error: AnthropicError<'a>,
    request_id: &'a str,
}

#[derive(Serialize)]
struct AnthropicError<'a> {
    #[serde(rename = "type")]
    error_type: &'a str,
    message: &'a str,
}

impl GatewayError<'_> {
    /// The error as a JSON response in `format`'s envelope. An Anthropic
    /// response repeats its `request_id` in the `request-id` header.
    pub(crate) fn into_response(self, format: ErrorFormat) -> Response {
        let builder = Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, "application/json");
        let (builder, body) = match format {
            ErrorFormat::OpenAi => {
                let error = OpenAiError {
                    message: self.message,
                    error_type: self.openai_type,
                    param: None,
                    code: self.code,
                };
                (builder, serialize_to_vec_or_default(&OpenAiErrorEnvelope { error }))
            }
            ErrorFormat::Anthropic => {
                let request_id = uuid7_str("req_");
                let error = AnthropicError {
                    error_type: self.anthropic_type,
                    message: self.message,
                };
                let body = serialize_to_vec_or_default(&AnthropicErrorEnvelope {
                    error,
                    request_id: &request_id,
                });
                (builder.header("request-id", request_id), body)
            }
        };
        builder
            .body(Body::from(body))
            .expect("a status code, fixed headers, and a generated request ID form a valid response")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn rate_limited(message: &str) -> GatewayError<'_> {
        GatewayError {
            status: StatusCode::TOO_MANY_REQUESTS,
            openai_type: "rate_limit_error",
            anthropic_type: "rate_limit_error",
            code: "rate_limit_exceeded",
            message,
        }
    }

    async fn body(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn the_openai_envelope_carries_type_code_and_a_dynamic_message() {
        let message = format!("retry after {} seconds", 7);
        let response = rate_limited(&message).into_response(ErrorFormat::OpenAi);

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert!(response.headers().get("request-id").is_none());
        assert_eq!(
            body(response).await,
            json!({"error": {
                "message": "retry after 7 seconds",
                "type": "rate_limit_error",
                "param": null,
                "code": "rate_limit_exceeded"
            }})
        );
    }

    #[tokio::test]
    async fn the_anthropic_envelope_repeats_its_request_id_in_a_header() {
        let response = rate_limited("slow down").into_response(ErrorFormat::Anthropic);

        let request_id = response.headers()["request-id"].to_str().unwrap().to_owned();
        assert!(request_id.starts_with("req_"), "{request_id}");
        assert_eq!(
            body(response).await,
            json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "slow down"},
                "request_id": request_id
            })
        );
    }

    #[test]
    fn messages_routes_answer_in_the_anthropic_format() {
        assert_eq!(ErrorFormat::for_path("/v1/messages"), ErrorFormat::Anthropic);
        assert_eq!(
            ErrorFormat::for_path("/v1/messages/count_tokens"),
            ErrorFormat::Anthropic
        );
        assert_eq!(ErrorFormat::for_path("/v1/responses"), ErrorFormat::OpenAi);
        assert_eq!(ErrorFormat::for_path("/v1/models"), ErrorFormat::OpenAi);
    }
}
