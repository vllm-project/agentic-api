//! Resolve independent response resource limits from configuration and environment.

use crate::config_file::ResponsesFileConfig;
use crate::web_fetch_config::env_parsed;
use crate::{environment_value, parse_env_nonzero_usize};
use agentic_core::config::{
    BACKGROUND_MAX_CONCURRENT_ENV, BACKGROUND_MAX_QUEUED_ENV, BACKGROUND_TIMEOUT_SECONDS_ENV, BackgroundConfig,
    DEFAULT_MAX_RETAINED_RESPONSE_BYTES, DEFAULT_MAX_STREAM_EVENT_BYTES, DEFAULT_MAX_UPSTREAM_JSON_BYTES,
    DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES, MAX_RETAINED_RESPONSE_BYTES_ENV, MAX_STREAM_EVENT_BYTES_ENV,
    MAX_UPSTREAM_JSON_BYTES_ENV, MAX_UPSTREAM_SSE_LINE_BYTES_ENV, ResponsesConfig,
};
use agentic_core::error::Error;
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

/// Resolves the background limits as environment variable > configuration file > default.
fn resolve_background_config(
    file: &ResponsesFileConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Result<BackgroundConfig, Error> {
    let defaults = BackgroundConfig::default();
    let config = BackgroundConfig {
        max_concurrent: env_parsed::<NonZeroUsize>(&env, BACKGROUND_MAX_CONCURRENT_ENV)?
            .or(file.background_max_concurrent)
            .unwrap_or(defaults.max_concurrent),
        max_queued: env_parsed::<usize>(&env, BACKGROUND_MAX_QUEUED_ENV)?
            .or(file.background_max_queued)
            .unwrap_or(defaults.max_queued),
        timeout: env_parsed::<NonZeroU64>(&env, BACKGROUND_TIMEOUT_SECONDS_ENV)?
            .or(file.background_timeout_seconds)
            .map_or(defaults.timeout, |seconds| Duration::from_secs(seconds.get())),
    };
    config.validate()?;
    Ok(config)
}

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
        background: resolve_background_config(file, environment_value)?,
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
        background_max_concurrent: environment_value(BACKGROUND_MAX_CONCURRENT_ENV)
            .and_then(|value| value.parse::<NonZeroUsize>().ok()),
        background_max_queued: environment_value(BACKGROUND_MAX_QUEUED_ENV)
            .and_then(|value| value.parse::<usize>().ok()),
        background_timeout_seconds: environment_value(BACKGROUND_TIMEOUT_SECONDS_ENV)
            .and_then(|value| value.parse::<NonZeroU64>().ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_file::FileConfig;
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

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn background_limits_default_then_file_then_environment() {
        assert_eq!(
            resolve_background_config(&ResponsesFileConfig::default(), no_env).unwrap(),
            BackgroundConfig::default()
        );
        let file: ResponsesFileConfig =
            toml::from_str("background_max_concurrent = 2\nbackground_max_queued = 0\nbackground_timeout_seconds = 1")
                .unwrap();
        let from_file = resolve_background_config(&file, no_env).unwrap();
        assert_eq!(from_file.max_concurrent.get(), 2);
        assert_eq!(from_file.max_queued, 0);
        assert_eq!(from_file.timeout, Duration::from_secs(1));
        let rendered = toml::to_string(&file).unwrap();
        for key in [
            "background_max_concurrent = 2",
            "background_max_queued = 0",
            "background_timeout_seconds = 1",
        ] {
            assert!(rendered.contains(key), "{rendered}");
        }

        let env = |name: &str| match name {
            BACKGROUND_MAX_CONCURRENT_ENV => Some("3".to_owned()),
            BACKGROUND_MAX_QUEUED_ENV => Some("5".to_owned()),
            BACKGROUND_TIMEOUT_SECONDS_ENV => Some("7".to_owned()),
            _ => None,
        };
        let from_env = resolve_background_config(&file, env).unwrap();
        assert_eq!(from_env.max_concurrent.get(), 3);
        assert_eq!(from_env.max_queued, 5);
        assert_eq!(from_env.timeout, Duration::from_secs(7));
    }

    #[test]
    fn background_limits_reject_values_below_their_minimum() {
        for (name, below, minimum) in [
            (BACKGROUND_MAX_CONCURRENT_ENV, "0", "1"),
            (BACKGROUND_MAX_QUEUED_ENV, "-1", "0"),
            (BACKGROUND_TIMEOUT_SECONDS_ENV, "0", "1"),
        ] {
            let at = |value: &'static str| move |key: &str| (key == name).then(|| value.to_owned());
            let error = resolve_background_config(&ResponsesFileConfig::default(), at(below)).unwrap_err();
            assert!(error.to_string().contains(name), "{name}={below}: {error}");
            assert!(
                resolve_background_config(&ResponsesFileConfig::default(), at(minimum)).is_ok(),
                "{name}={minimum}"
            );
        }
        for invalid in [
            "background_max_concurrent = 0",
            "background_max_queued = -1",
            "background_timeout_seconds = 0",
            "background_unknown = 1",
        ] {
            assert!(toml::from_str::<ResponsesFileConfig>(invalid).is_err(), "{invalid}");
        }
    }
}
