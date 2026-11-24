use std::collections::BTreeMap;

use serde_json::{Value, json, value::RawValue};

use super::{RequestPayload, ResponseTextConfig, UnsupportedParameterError, UpstreamRequestError};
use crate::types::io::input::ResolvedFiles;
use crate::types::model_capabilities::{ModelCapabilities, ModelOverride, SupportedReasoningLevels};
use crate::types::{ReasoningEffortLevel, RequestField};

#[test]
fn typed_top_logprobs_survives_mapping_round_trip_and_upstream_serialization() {
    for value in [
        None,
        Some(Value::Null),
        Some(json!(-1)),
        Some(json!(0)),
        Some(json!(21)),
    ] {
        let mut wire = json!({"model": "test", "input": "hello"});
        if let Some(value) = &value {
            wire["top_logprobs"] = value.clone();
        }
        let raw: RequestPayload<RawValue> = serde_json::from_value(wire).unwrap();
        let request = raw
            .try_map_text(|text| serde_json::from_str::<ResponseTextConfig>(text.get()).map(Box::new))
            .unwrap();
        assert!(!request.extra.contains_key("top_logprobs"));
        let restored: RequestPayload = serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        assert_eq!(request.top_logprobs, restored.top_logprobs);
        for stream in [false, true] {
            let body = serde_json::to_string(&restored.to_upstream_request(stream, &ResolvedFiles::default()).unwrap())
                .unwrap();
            assert_eq!(body.matches("\"top_logprobs\":").count(), usize::from(value.is_some()));
            let upstream: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(upstream.get("top_logprobs"), value.as_ref());
        }
    }
}

#[test]
fn programmatic_extension_cannot_bypass_admission_or_duplicate_a_typed_upstream_key() {
    for typed in [RequestField::Missing, RequestField::Null, RequestField::Value(3)] {
        for extension in [Value::Null, json!(10), json!("invalid")] {
            let request = RequestPayload {
                top_logprobs: typed.clone(),
                extra: [("top_logprobs".to_owned(), extension)].into(),
                ..Default::default()
            };
            assert_eq!(
                request.ensure_supported_parameters(None, true),
                Err(UnsupportedParameterError::TopLogprobsExtension)
            );
            assert!(matches!(
                request.to_upstream_request(false, &ResolvedFiles::default()),
                Err(UpstreamRequestError::Parameter(
                    UnsupportedParameterError::TopLogprobsExtension
                ))
            ));
        }
    }
}

#[test]
fn programmatic_tool_calling_is_rejected_before_inference_for_every_transport() {
    // A declared PTC request carries the capability in the wire body, which lands in `extra`.
    // The gateway must reject it rather than forwarding it to an upstream whose PTC contract
    // has not been established. `null` is the surrounding Option and stays allowed.
    let parse = |value: Value| -> RequestPayload {
        serde_json::from_value(json!({
            "model": "test", "input": "hello", "programmatic_tool_calling": value
        }))
        .unwrap()
    };
    for value in [json!(true), json!({}), json!([]), json!("auto")] {
        let request = parse(value);
        assert_eq!(
            request.ensure_supported_parameters(None, true),
            Err(UnsupportedParameterError::ProgrammaticToolCalling)
        );
        for stream in [false, true] {
            assert!(matches!(
                request.to_upstream_request(stream, &ResolvedFiles::default()),
                Err(UpstreamRequestError::Parameter(
                    UnsupportedParameterError::ProgrammaticToolCalling
                ))
            ));
        }
        let error = request.ensure_supported_parameters(None, true).unwrap_err();
        assert_eq!(
            (error.param(), error.code()),
            ("programmatic_tool_calling", "unsupported_parameter")
        );
    }
    assert_eq!(parse(Value::Null).ensure_supported_parameters(None, true), Ok(()));
}

#[test]
fn display_defaults_do_not_restrict_requests_but_explicit_reasoning_levels_do() {
    let caps = ModelCapabilities::new(BTreeMap::from([(
        "configured".to_owned(),
        ModelOverride {
            supported_reasoning_levels: Some(SupportedReasoningLevels::try_new(&[ReasoningEffortLevel::Low]).unwrap()),
            ..Default::default()
        },
    )]));
    for model in ["configured", "unconfigured"] {
        for effort in ["none", "minimal", "low", "max"] {
            let request: RequestPayload = serde_json::from_value(json!({
                "model": model, "input": "hello", "reasoning": {"effort": effort}
            }))
            .unwrap();
            let result = request.ensure_supported_parameters(caps.for_model(model), false);
            if model == "configured" && effort != "low" {
                assert_eq!(result, Err(UnsupportedParameterError::ReasoningEffort));
            } else {
                assert_eq!(result, Ok(()));
            }
        }
    }
    assert!(
        !caps
            .resolve_reasoning_levels("unconfigured")
            .contains(&ReasoningEffortLevel::None)
    );
}

#[test]
fn model_cache_override_takes_precedence_in_both_directions() {
    let request: RequestPayload = serde_json::from_value(json!({
        "model": "test", "input": "hello", "prompt_cache_options": {"mode": "explicit"}
    }))
    .unwrap();
    for deployment in [false, true] {
        for override_value in [None, Some(false), Some(true)] {
            let model = ModelOverride {
                prompt_cache_options_supported: override_value,
                ..Default::default()
            };
            let result = request.ensure_supported_parameters(Some(&model), deployment);
            assert_eq!(result.is_ok(), override_value.unwrap_or(deployment));
            if let Err(error) = result {
                assert_eq!(
                    (error.param(), error.code()),
                    ("prompt_cache_options", "unsupported_parameter")
                );
            }
        }
    }
}

#[test]
fn logprobs_policy_preserves_null_but_rejects_explicit_zero_and_include() {
    let model = ModelOverride {
        logprobs_supported: Some(false),
        ..Default::default()
    };
    for (fields, expected) in [
        (json!({}), None),
        (json!({"top_logprobs": null}), None),
        (json!({"top_logprobs": 0}), Some(UnsupportedParameterError::TopLogprobs)),
        (
            json!({"include": ["message.output_text.logprobs"]}),
            Some(UnsupportedParameterError::LogprobsInclude),
        ),
    ] {
        let mut wire = json!({"model": "test", "input": "hello"});
        wire.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let request: RequestPayload = serde_json::from_value(wire).unwrap();
        assert_eq!(request.ensure_supported_parameters(Some(&model), false).err(), expected);
    }
}
