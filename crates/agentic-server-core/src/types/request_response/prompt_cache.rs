//! The mode/TTL subset of the upstream prompt-cache contract.

use serde::{Deserialize, Deserializer, Serialize};

use super::RequestPayload;

/// Where the upstream selects prompt-cache breakpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheMode {
    Implicit,
    Explicit,
}

/// Minimum cache lifetime requested from a supporting upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum PromptCacheTtl {
    #[serde(rename = "30m")]
    ThirtyMinutes,
}

/// Request-scoped prompt-cache options. The gateway never inserts upstream defaults.
///
/// Typed execution rejects prewarming, diagnostics, and future fields until qualified.
/// Direct HTTP pass-through leaves their interpretation to the upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct PromptCacheOptions {
    #[cfg_attr(feature = "openapi", schema(nullable = false))]
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present_value")]
    pub mode: Option<PromptCacheMode>,
    #[cfg_attr(feature = "openapi", schema(nullable = false))]
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present_value")]
    pub ttl: Option<PromptCacheTtl>,
}

impl<'de> Deserialize<'de> for PromptCacheOptions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            #[serde(default, deserialize_with = "present_value")]
            mode: Option<PromptCacheMode>,
            #[serde(default, deserialize_with = "present_value")]
            ttl: Option<PromptCacheTtl>,
        }
        struct ObjectVisitor;
        impl<'de> serde::de::Visitor<'de> for ObjectVisitor {
            type Value = PromptCacheOptions;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a prompt_cache_options object containing mode and/or ttl")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                let fields = Fields::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(PromptCacheOptions {
                    mode: fields.mode,
                    ttl: fields.ttl,
                })
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

// Omitted members stay omitted; present members must match the non-null SDK contract.
fn present_value<'de, D: Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

/// A syntactically valid cache policy cannot be served by this deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PromptCacheOptionsError {
    #[error(
        "prompt_cache_options is not enabled for this upstream; configure responses.prompt_cache_options_supported only after verifying the mode/TTL contract for every served model"
    )]
    Unsupported,
}

impl<T: ?Sized> RequestPayload<T> {
    /// Validate deployment support for typed execution.
    ///
    /// # Errors
    /// Rejects typed execution against unqualified upstreams. Legacy retention remains independent.
    pub fn validate_prompt_cache_options(&self, supported: bool) -> Result<(), PromptCacheOptionsError> {
        if self.prompt_cache_options.is_none() {
            return Ok(());
        }
        if !supported {
            return Err(PromptCacheOptionsError::Unsupported);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::request_response::{RequestPayload, ResponseTextConfig};
    use serde_json::json;

    #[test]
    fn mapping_and_request_round_trip_preserve_options_and_content_breakpoints() {
        let wire = json!({"model":"test", "store":true,
            "prompt_cache_options":{"mode":"explicit","ttl":"30m"},
            "input":[{"type":"message","role":"developer","content":[{"type":"input_text","text":"stable",
                "prompt_cache_breakpoint":{"mode":"explicit"}}]}]});
        let routing: RequestPayload<serde_json::value::RawValue> = serde_json::from_value(wire.clone()).unwrap();
        let request = routing
            .try_map_text(|text| serde_json::from_str::<ResponseTextConfig>(text.get()).map(Box::new))
            .unwrap();
        let restored: RequestPayload = serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
        for stream in [false, true] {
            let upstream = serde_json::to_value(restored.to_upstream_request(stream).unwrap()).unwrap();
            assert_eq!(upstream["prompt_cache_options"], wire["prompt_cache_options"]);
            assert_eq!(upstream["input"], wire["input"]);
        }
    }

    #[test]
    fn mode_and_ttl_preserve_omission_without_defaults() {
        for wire in [
            json!({}),
            json!({"mode":"implicit"}),
            json!({"ttl":"30m"}),
            json!({"mode":"explicit","ttl":"30m"}),
        ] {
            let options: PromptCacheOptions = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(options).unwrap(), wire);
            let mut request: RequestPayload = RequestPayload {
                prompt_cache_options: Some(options),
                ..Default::default()
            };
            assert!(request.validate_prompt_cache_options(true).is_ok());
            assert_eq!(
                request.validate_prompt_cache_options(false),
                Err(PromptCacheOptionsError::Unsupported)
            );
            request.prompt_cache_retention = Some(super::super::PromptCacheRetention::TwentyFourHours);
            assert_eq!(request.validate_prompt_cache_options(true), Ok(()));
        }
    }

    #[test]
    fn routing_preserves_raw_options_but_execution_requires_the_typed_contract() {
        use serde_json::value::RawValue;
        for value in [json!({"prewarm":true}), json!({"future":1}), json!([])] {
            let wire = json!({"model":"test","input":"hi","prompt_cache_options":value});
            let routing: RequestPayload<RawValue, Box<RawValue>> = serde_json::from_value(wire).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(routing.prompt_cache_options.as_ref().unwrap().get())
                    .unwrap(),
                value
            );
            let typed = routing.try_map_configs(
                |text| serde_json::from_str::<ResponseTextConfig>(text.get()).map(Box::new),
                |options| serde_json::from_str::<PromptCacheOptions>(options.get()),
            );
            assert!(typed.is_err());
        }
    }

    #[test]
    fn unqualified_members_and_invalid_values_are_rejected() {
        for wire in [
            json!({"mode":"auto"}),
            json!({"mode":null}),
            json!({"mode":false}),
            json!({"ttl":"24h"}),
            json!({"ttl":null}),
            json!({"ttl":30}),
            json!({"prewarm":true}),
            json!({"prewarm":false}),
            json!({"comparison_response_id":"resp_1"}),
            json!({"future":1}),
            json!([]),
        ] {
            assert!(
                serde_json::from_value::<PromptCacheOptions>(wire.clone()).is_err(),
                "{wire}"
            );
        }
        assert!(serde_json::from_str::<PromptCacheOptions>(r#"{"mode":"implicit","mode":"explicit"}"#).is_err());
    }
}
