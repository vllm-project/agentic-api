//! Resolve independent response resource limits from configuration and environment.

use crate::startup::config_file::ResponsesFileConfig;
use crate::startup::{environment_value, parse_env_nonzero_usize};
use agentic_core::config::{
    DEFAULT_MAX_RETAINED_RESPONSE_BYTES, DEFAULT_MAX_STREAM_EVENT_BYTES, DEFAULT_MAX_UPSTREAM_JSON_BYTES,
    DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES, MAX_RETAINED_RESPONSE_BYTES_ENV, MAX_STREAM_EVENT_BYTES_ENV,
    MAX_UPSTREAM_JSON_BYTES_ENV, MAX_UPSTREAM_SSE_LINE_BYTES_ENV, ResponsesConfig,
};
use agentic_core::error::Error;
use std::num::NonZeroUsize;

pub(crate) fn resolve_responses_config(file: &ResponsesFileConfig) -> Result<ResponsesConfig, Error> {
    let max_retained_bytes_default = file
        .max_retained_bytes
        .unwrap_or_else(|| NonZeroUsize::new(DEFAULT_MAX_RETAINED_RESPONSE_BYTES).expect("nonzero default"));
    let max_retained_bytes =
        parse_env_nonzero_usize(MAX_RETAINED_RESPONSE_BYTES_ENV, max_retained_bytes_default)?.get();
    let max_upstream_json_bytes_default = file
        .max_upstream_json_bytes
        .unwrap_or_else(|| NonZeroUsize::new(DEFAULT_MAX_UPSTREAM_JSON_BYTES).expect("nonzero default"));
    let max_upstream_json_bytes =
        parse_env_nonzero_usize(MAX_UPSTREAM_JSON_BYTES_ENV, max_upstream_json_bytes_default)?.get();
    let max_upstream_sse_line_bytes_default = file
        .max_upstream_sse_line_bytes
        .unwrap_or_else(|| NonZeroUsize::new(DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES).expect("nonzero default"));
    let max_upstream_sse_line_bytes =
        parse_env_nonzero_usize(MAX_UPSTREAM_SSE_LINE_BYTES_ENV, max_upstream_sse_line_bytes_default)?.get();
    let max_stream_event_bytes_default = file
        .max_stream_event_bytes
        .unwrap_or_else(|| NonZeroUsize::new(DEFAULT_MAX_STREAM_EVENT_BYTES).expect("nonzero default"));
    let max_stream_event_bytes =
        parse_env_nonzero_usize(MAX_STREAM_EVENT_BYTES_ENV, max_stream_event_bytes_default)?.get();
    let responses_config = ResponsesConfig {
        prompt_cache_options_supported: file.prompt_cache_options_supported.unwrap_or(false),
        max_retained_bytes,
        max_upstream_json_bytes,
        max_upstream_sse_line_bytes,
        max_stream_event_bytes,
    };
    responses_config.validate()?;
    Ok(responses_config)
}

pub(crate) fn generated_responses_file_config() -> ResponsesFileConfig {
    ResponsesFileConfig {
        prompt_cache_options_supported: None,
        max_retained_bytes: environment_value(MAX_RETAINED_RESPONSE_BYTES_ENV)
            .and_then(|value| value.parse::<NonZeroUsize>().ok()),
        max_upstream_json_bytes: environment_value(MAX_UPSTREAM_JSON_BYTES_ENV)
            .and_then(|value| value.parse::<NonZeroUsize>().ok()),
        max_upstream_sse_line_bytes: environment_value(MAX_UPSTREAM_SSE_LINE_BYTES_ENV)
            .and_then(|value| value.parse::<NonZeroUsize>().ok()),
        max_stream_event_bytes: environment_value(MAX_STREAM_EVENT_BYTES_ENV)
            .and_then(|value| value.parse::<NonZeroUsize>().ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::config_file::FileConfig;
    use agentic_core::types::ServiceTier;

    #[test]
    fn model_capabilities_share_one_configuration_with_the_catalog() {
        let input = r#"
            [responses]
            prompt_cache_options_supported = true

            [models."gpt-6-astra"]
            sampling_supported = false
            logprobs_supported = false
            supported_reasoning_levels = ["none", "minimal", "low", "high"]
            service_tiers = ["default"]
            prompt_cache_options_supported = false
            prompt_cache_retention_supported = false
        "#;
        let file: FileConfig = toml::from_str(input).unwrap();
        let config = resolve_responses_config(&file.responses).unwrap();
        assert!(config.prompt_cache_options_supported);
        let capabilities = file.model_capabilities();
        let policy = capabilities.for_model("gpt-6-astra").unwrap();
        assert_eq!(policy.sampling_supported, Some(false));
        assert_eq!(policy.logprobs_supported, Some(false));
        assert_eq!(policy.supported_reasoning_levels.as_ref().unwrap().as_slice().len(), 4);
        assert_eq!(
            capabilities.resolve_reasoning_levels("gpt-6-astra"),
            policy.supported_reasoning_levels.as_ref().unwrap().as_slice(),
        );
        assert_eq!(policy.service_tiers.as_deref(), Some(&[ServiceTier::Default][..]));
        assert_eq!(policy.prompt_cache_options_supported, Some(false));
        assert_eq!(policy.prompt_cache_retention_supported, Some(false));
        let restored: FileConfig = toml::from_str(&toml::to_string(&file).unwrap()).unwrap();
        assert_eq!(restored.model_capabilities().for_model("gpt-6-astra"), Some(policy));
        assert!(toml::from_str::<FileConfig>("[responses.models.astra]\nlogprobs_supported = false").is_err());
        assert!(toml::from_str::<FileConfig>("[models.astra]\nlogprob_supported = false").is_err());
    }

    #[test]
    fn prompt_cache_support_is_explicit_and_preserved_in_file_config() {
        assert!(
            !resolve_responses_config(&ResponsesFileConfig::default())
                .unwrap()
                .prompt_cache_options_supported
        );
        for supported in [false, true] {
            let file: ResponsesFileConfig =
                toml::from_str(&format!("prompt_cache_options_supported = {supported}")).unwrap();
            assert_eq!(
                resolve_responses_config(&file).unwrap().prompt_cache_options_supported,
                supported
            );
            assert!(
                toml::to_string(&file)
                    .unwrap()
                    .contains(&format!("prompt_cache_options_supported = {supported}"))
            );
        }
        assert!(toml::from_str::<ResponsesFileConfig>("prompt_cache_options_supported = 'yes'").is_err());
    }
}
