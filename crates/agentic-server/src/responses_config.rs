//! Resolve independent response resource limits from configuration and environment.

use crate::config_file::ResponsesFileConfig;
use crate::{environment_value, parse_env_nonzero_usize};
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
        reasoning_replay_policy: file.reasoning_replay_policy.unwrap_or_default(),
        reasoning_replay_profile: file.reasoning_replay_profile,
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
        reasoning_replay_policy: None,
        reasoning_replay_profile: None,
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
