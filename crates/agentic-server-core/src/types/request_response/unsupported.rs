//! Request members typed execution recognizes only to reject them.
//!
//! Silently dropping `background` or `prompt` would answer a different request
//! than the client sent, so executor admission rejects them with a typed
//! parameter error instead. Absent and `null` values (and `background: false`)
//! are the defaults the executor already honours, so they stay accepted.
//! Direct HTTP pass-through forwards the original body, and the upstream decides.

use serde::{Deserialize, Deserializer};

use super::RequestPayload;

/// Request parameter name for background execution.
pub const BACKGROUND_PARAM: &str = "background";
/// Request parameter name for a reusable prompt reference.
pub const PROMPT_PARAM: &str = "prompt";

/// A request member recognized only to be rejected: any JSON value
/// deserializes, and none of it is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RejectedValue;

impl<'de> Deserialize<'de> for RejectedValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer).map(|_| Self)
    }
}

/// A recognized request member this gateway cannot honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UnsupportedParameterError {
    #[error("background mode is not supported; omit `background` or set it to false")]
    BackgroundMode,
    #[error("reusable prompt templates are not supported; send `instructions` and `input` instead of `prompt`")]
    PromptTemplate,
}

impl UnsupportedParameterError {
    /// The offending request parameter.
    #[must_use]
    pub const fn param(self) -> &'static str {
        match self {
            Self::BackgroundMode => BACKGROUND_PARAM,
            Self::PromptTemplate => PROMPT_PARAM,
        }
    }

    /// Machine-readable error code for the API error envelope.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::BackgroundMode => "unsupported_value",
            Self::PromptTemplate => "unsupported_parameter",
        }
    }
}

impl RequestPayload {
    /// Rejects request members typed execution recognizes but cannot honour.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedParameterError`] for `background: true` or a
    /// non-null `prompt`.
    pub fn ensure_supported_parameters(&self) -> Result<(), UnsupportedParameterError> {
        if self.background == Some(true) {
            return Err(UnsupportedParameterError::BackgroundMode);
        }
        if self.prompt.is_some() {
            return Err(UnsupportedParameterError::PromptTemplate);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::value::RawValue;
    use serde_json::{Value, json};

    use super::*;

    fn parse(extra: &Value) -> RequestPayload {
        let mut body = json!({"model": "m", "input": "hi"});
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value(body).unwrap()
    }

    #[test]
    fn defaults_and_explicit_defaults_are_supported() {
        for extra in [
            json!({}),
            json!({"background": null}),
            json!({"background": false}),
            json!({"prompt": null}),
        ] {
            assert_eq!(parse(&extra).ensure_supported_parameters(), Ok(()), "{extra}");
        }
    }

    #[test]
    fn background_true_is_an_unsupported_value() {
        let extra = json!({"background": true});
        let error = parse(&extra).ensure_supported_parameters().unwrap_err();
        assert_eq!(error, UnsupportedParameterError::BackgroundMode);
        assert_eq!((error.param(), error.code()), ("background", "unsupported_value"));
    }

    #[test]
    fn background_must_be_a_boolean() {
        let body = json!({"model": "m", "input": "hi", "background": "yes"});
        assert!(serde_json::from_value::<RequestPayload>(body).is_err());
    }

    #[test]
    fn any_prompt_reference_is_an_unsupported_parameter() {
        for prompt in [json!({"id": "pmpt_1", "version": "2"}), json!("x"), json!([])] {
            let extra = json!({"prompt": prompt});
            let error = parse(&extra).ensure_supported_parameters().unwrap_err();
            assert_eq!(error, UnsupportedParameterError::PromptTemplate, "{prompt}");
            assert_eq!((error.param(), error.code()), ("prompt", "unsupported_parameter"));
        }
    }

    #[test]
    fn background_is_checked_before_prompt() {
        let extra = json!({"background": true, "prompt": {"id": "pmpt_1"}});
        assert_eq!(
            parse(&extra).ensure_supported_parameters(),
            Err(UnsupportedParameterError::BackgroundMode)
        );
    }

    /// The HTTP route parses each request before choosing direct pass-through,
    /// which forwards the original body: no value of these members may fail
    /// that parse.
    #[test]
    fn the_routing_payload_parses_every_value_it_passes_through() {
        for extra in [
            json!({"background": true}),
            json!({"background": false}),
            json!({"background": null}),
            json!({"prompt": {"id": "pmpt_1", "version": "2"}}),
            json!({"prompt": "x"}),
            json!({"prompt": []}),
            json!({"prompt": null}),
        ] {
            let mut body = json!({"model": "m", "input": "hi", "text": {"format": {"type": "text"}}});
            body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            let parsed = serde_json::from_str::<RequestPayload<RawValue>>(&body.to_string());
            assert!(parsed.is_ok(), "{extra}: {parsed:?}");
        }
    }

    #[test]
    fn rejected_members_are_never_serialized() {
        let payload = parse(&json!({"background": false, "prompt": {"id": "pmpt_1"}}));
        let stored = serde_json::to_value(&payload).unwrap();
        assert!(stored.get("background").is_none(), "{stored}");
        assert!(stored.get("prompt").is_none(), "{stored}");
    }
}
