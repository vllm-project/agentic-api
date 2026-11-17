//! Typed gateway parameter checks and opt-in model capability restrictions.
//!
//! Wire decoding validates field shapes. Admission rejects unsupported gateway features and
//! explicitly configured model restrictions before history lookup or inference. Unconfigured
//! model capabilities remain upstream-owned, independently of catalog display defaults.

use std::fmt;

use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::{JsonKind, RequestPayload};
use crate::types::io::ItemInclude;
use crate::types::model_capabilities::ModelOverride;

/// Request parameter name for background execution.
pub const BACKGROUND_PARAM: &str = "background";
/// Request parameter name for a reusable prompt reference.
pub const PROMPT_PARAM: &str = "prompt";
/// Request parameter name for the tool selector.
pub const TOOL_CHOICE_PARAM: &str = "tool_choice";

/// A request member recognized only to be rejected: any JSON value
/// deserializes, and none of it is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RejectedValue;

impl<'de> Deserialize<'de> for RejectedValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer).map(|_| Self)
    }
}

/// `background` exactly as the client sent it; `null` is the surrounding `Option`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Background(WireBackground);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireBackground {
    Boolean(bool),
    Other(JsonKind),
}

impl<'de> Deserialize<'de> for Background {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(BackgroundVisitor).map(Self)
    }
}

struct BackgroundVisitor;

impl<'de> Visitor<'de> for BackgroundVisitor {
    type Value = WireBackground;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<WireBackground, E> {
        Ok(WireBackground::Boolean(value))
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<WireBackground, E> {
        Ok(WireBackground::Other(JsonKind::Integer))
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<WireBackground, E> {
        Ok(WireBackground::Other(JsonKind::Integer))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<WireBackground, E> {
        Ok(WireBackground::Other(JsonKind::Decimal))
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<WireBackground, E> {
        Ok(WireBackground::Other(JsonKind::String))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<WireBackground, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(WireBackground::Other(JsonKind::Object))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<WireBackground, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(WireBackground::Other(JsonKind::Array))
    }
}

/// A recognized request member this gateway cannot honour, or a `background`
/// that is not a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UnsupportedParameterError {
    #[error("background mode is not supported; omit `background` or set it to false")]
    BackgroundMode,
    #[error("Invalid type for 'background': expected a boolean, but got {0} instead.")]
    BackgroundType(JsonKind),
    #[error("reusable prompt templates are not supported; send `instructions` and `input` instead of `prompt`")]
    PromptTemplate,
    #[error("MCP tool_choice is not supported; use `auto`, `required`, or a function `tool_choice` instead")]
    McpToolChoice,
    #[error("temperature is not supported by the configured model policy")]
    Temperature,
    #[error("top_p is not supported by the configured model policy")]
    TopP,
    #[error("top_logprobs is not supported by the configured model policy")]
    TopLogprobs,
    #[error("message.output_text.logprobs in include is not supported by the configured model policy")]
    LogprobsInclude,
    #[error("reasoning.effort is not supported by the configured model policy")]
    ReasoningEffort,
    #[error("service_tier is not supported by the configured model policy")]
    ServiceTier,
    #[error("prompt_cache_retention is not supported by the configured model policy")]
    PromptCacheRetention,
    #[error(
        "prompt_cache_options is not enabled for this model; verify the upstream mode/TTL contract before enabling models.<id>.prompt_cache_options_supported or responses.prompt_cache_options_supported"
    )]
    PromptCacheOptions,
    #[error("top_logprobs must use its typed request field, not extra")]
    TopLogprobsExtension,
}

impl UnsupportedParameterError {
    /// The offending request parameter.
    #[must_use]
    pub const fn param(self) -> &'static str {
        match self {
            Self::BackgroundMode | Self::BackgroundType(_) => BACKGROUND_PARAM,
            Self::PromptTemplate => PROMPT_PARAM,
            Self::McpToolChoice => TOOL_CHOICE_PARAM,
            Self::Temperature => "temperature",
            Self::TopP => "top_p",
            Self::TopLogprobs | Self::TopLogprobsExtension => "top_logprobs",
            Self::LogprobsInclude => "include",
            Self::ReasoningEffort => "reasoning.effort",
            Self::ServiceTier => "service_tier",
            Self::PromptCacheRetention => "prompt_cache_retention",
            Self::PromptCacheOptions => "prompt_cache_options",
        }
    }

    /// Machine-readable error code for the API error envelope.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::BackgroundMode
            | Self::McpToolChoice
            | Self::Temperature
            | Self::TopP
            | Self::TopLogprobs
            | Self::LogprobsInclude
            | Self::ReasoningEffort
            | Self::ServiceTier
            | Self::PromptCacheRetention
            | Self::TopLogprobsExtension => "unsupported_value",
            Self::BackgroundType(_) => "invalid_type",
            Self::PromptTemplate | Self::PromptCacheOptions => "unsupported_parameter",
        }
    }
}

impl RequestPayload {
    pub(super) fn validate_modeled_extensions(&self) -> Result<(), UnsupportedParameterError> {
        if self.extra.contains_key("top_logprobs") {
            return Err(UnsupportedParameterError::TopLogprobsExtension);
        }
        Ok(())
    }

    /// Rejects request members typed execution recognizes but cannot honour.
    ///
    /// # Errors
    ///
    /// Returns a typed error for unsupported gateway parameters or explicit model restrictions.
    /// Only the configured override restricts model parameters; catalog display defaults do not.
    /// The deployment cache-options setting is used when the model has no override for it.
    pub fn ensure_supported_parameters(
        &self,
        model: Option<&ModelOverride>,
        prompt_cache_options_supported: bool,
    ) -> Result<(), UnsupportedParameterError> {
        self.validate_modeled_extensions()?;
        let cache_supported = model
            .and_then(|model| model.prompt_cache_options_supported)
            .unwrap_or(prompt_cache_options_supported);
        if self.prompt_cache_options.is_some() && !cache_supported {
            return Err(UnsupportedParameterError::PromptCacheOptions);
        }
        if let Some(model) = model {
            if model.sampling_supported == Some(false) {
                if self.temperature.is_some() {
                    return Err(UnsupportedParameterError::Temperature);
                }
                if self.top_p.is_some() {
                    return Err(UnsupportedParameterError::TopP);
                }
            }
            if model.logprobs_supported == Some(false) {
                if self.top_logprobs.as_ref().is_some() {
                    return Err(UnsupportedParameterError::TopLogprobs);
                }
                if self
                    .include
                    .iter()
                    .flatten()
                    .any(|field| matches!(field.parse(), Ok(ItemInclude::MessageOutputTextLogprobs)))
                {
                    return Err(UnsupportedParameterError::LogprobsInclude);
                }
            }
            if let Some((effort, allowed)) = self
                .reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.effort)
                .zip(model.supported_reasoning_levels.as_ref())
                && !allowed.as_slice().contains(&effort)
            {
                return Err(UnsupportedParameterError::ReasoningEffort);
            }
            if let Some((tier, allowed)) = self.service_tier.zip(model.service_tiers.as_ref())
                && !allowed.contains(&tier)
            {
                return Err(UnsupportedParameterError::ServiceTier);
            }
            if model.prompt_cache_retention_supported == Some(false) && self.prompt_cache_retention.is_some() {
                return Err(UnsupportedParameterError::PromptCacheRetention);
            }
        }
        match self.background {
            Some(Background(WireBackground::Boolean(true))) => {
                return Err(UnsupportedParameterError::BackgroundMode);
            }
            Some(Background(WireBackground::Other(kind))) => {
                return Err(UnsupportedParameterError::BackgroundType(kind));
            }
            Some(Background(WireBackground::Boolean(false))) | None => {}
        }
        if self.prompt.is_some() {
            return Err(UnsupportedParameterError::PromptTemplate);
        }
        match &self.tool_choice {
            Some(choice) if !choice.is_supported() => Err(UnsupportedParameterError::McpToolChoice),
            _ => Ok(()),
        }
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
            assert_eq!(
                parse(&extra).ensure_supported_parameters(None, false),
                Ok(()),
                "{extra}"
            );
        }
    }

    #[test]
    fn background_true_is_an_unsupported_value() {
        let extra = json!({"background": true});
        let error = parse(&extra).ensure_supported_parameters(None, false).unwrap_err();
        assert_eq!(error, UnsupportedParameterError::BackgroundMode);
        assert_eq!((error.param(), error.code()), ("background", "unsupported_value"));
    }

    #[test]
    fn a_non_boolean_background_is_an_invalid_type() {
        for (value, kind) in [
            (json!(0), JsonKind::Integer),
            (json!(-1), JsonKind::Integer),
            (json!(0.5), JsonKind::Decimal),
            (json!("false"), JsonKind::String),
            (json!({}), JsonKind::Object),
            (json!([true]), JsonKind::Array),
        ] {
            let error = parse(&json!({"background": value}))
                .ensure_supported_parameters(None, false)
                .unwrap_err();
            assert_eq!(error, UnsupportedParameterError::BackgroundType(kind), "{value}");
            assert_eq!((error.param(), error.code()), ("background", "invalid_type"));
        }
        assert_eq!(
            UnsupportedParameterError::BackgroundType(JsonKind::String).to_string(),
            "Invalid type for 'background': expected a boolean, but got a string instead."
        );
    }

    #[test]
    fn any_prompt_reference_is_an_unsupported_parameter() {
        for prompt in [json!({"id": "pmpt_1", "version": "2"}), json!("x"), json!([])] {
            let extra = json!({"prompt": prompt});
            let error = parse(&extra).ensure_supported_parameters(None, false).unwrap_err();
            assert_eq!(error, UnsupportedParameterError::PromptTemplate, "{prompt}");
            assert_eq!((error.param(), error.code()), ("prompt", "unsupported_parameter"));
        }
    }

    #[test]
    fn background_is_checked_before_prompt() {
        let extra = json!({"background": true, "prompt": {"id": "pmpt_1"}});
        assert_eq!(
            parse(&extra).ensure_supported_parameters(None, false),
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
            json!({"background": 0}),
            json!({"background": "false"}),
            json!({"background": {}}),
            json!({"background": [1]}),
            json!({"background": 0.5}),
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
