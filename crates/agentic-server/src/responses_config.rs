//! Resolve response resource limits and upstream deadlines from configuration and environment.

use crate::config_file::ResponsesFileConfig;
use crate::{environment_value, parse_env_nonzero_u64_value, parse_env_nonzero_usize};
use agentic_core::config::{
    DEFAULT_MAX_RETAINED_RESPONSE_BYTES, DEFAULT_MAX_STREAM_EVENT_BYTES, DEFAULT_MAX_UPSTREAM_JSON_BYTES,
    DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES, DEFAULT_UPSTREAM_CONNECT_TIMEOUT_S,
    DEFAULT_UPSTREAM_RESPONSE_HEADER_TIMEOUT_S, DEFAULT_UPSTREAM_TOTAL_TIMEOUT_S, MAX_RETAINED_RESPONSE_BYTES_ENV,
    MAX_STREAM_EVENT_BYTES_ENV, MAX_UPSTREAM_JSON_BYTES_ENV, MAX_UPSTREAM_SSE_LINE_BYTES_ENV, ResponsesConfig,
    UPSTREAM_CONNECT_TIMEOUT_S_ENV, UPSTREAM_RESPONSE_HEADER_TIMEOUT_S_ENV, UPSTREAM_TOTAL_TIMEOUT_S_ENV,
};
use agentic_core::error::Error;
use std::num::{NonZeroU64, NonZeroUsize};

fn resolve_timeout(name: &str, file_value: Option<NonZeroU64>, default: NonZeroU64) -> Result<NonZeroU64, Error> {
    parse_env_nonzero_u64_value(name, std::env::var(name), file_value.unwrap_or(default))
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
    let upstream_connect_timeout_s = resolve_timeout(
        UPSTREAM_CONNECT_TIMEOUT_S_ENV,
        file.upstream_connect_timeout_s,
        DEFAULT_UPSTREAM_CONNECT_TIMEOUT_S,
    )?;
    let upstream_response_header_timeout_s = resolve_timeout(
        UPSTREAM_RESPONSE_HEADER_TIMEOUT_S_ENV,
        file.upstream_response_header_timeout_s,
        DEFAULT_UPSTREAM_RESPONSE_HEADER_TIMEOUT_S,
    )?;
    let upstream_total_timeout_s = resolve_timeout(
        UPSTREAM_TOTAL_TIMEOUT_S_ENV,
        file.upstream_total_timeout_s,
        DEFAULT_UPSTREAM_TOTAL_TIMEOUT_S,
    )?;
    let responses_config = ResponsesConfig {
        prompt_cache_options_supported: file.prompt_cache_options_supported.unwrap_or(false),
        max_retained_bytes,
        max_upstream_json_bytes,
        max_upstream_sse_line_bytes,
        max_stream_event_bytes,
        upstream_connect_timeout_s,
        upstream_response_header_timeout_s,
        upstream_total_timeout_s,
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
        upstream_connect_timeout_s: environment_value(UPSTREAM_CONNECT_TIMEOUT_S_ENV)
            .and_then(|value| value.parse::<NonZeroU64>().ok()),
        upstream_response_header_timeout_s: environment_value(UPSTREAM_RESPONSE_HEADER_TIMEOUT_S_ENV)
            .and_then(|value| value.parse::<NonZeroU64>().ok()),
        upstream_total_timeout_s: environment_value(UPSTREAM_TOTAL_TIMEOUT_S_ENV)
            .and_then(|value| value.parse::<NonZeroU64>().ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
