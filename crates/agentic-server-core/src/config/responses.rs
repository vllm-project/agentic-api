//! Responses resource limits and operator-configured upstream capabilities.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::Error;

pub const DEFAULT_MAX_RETAINED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_UPSTREAM_JSON_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_STREAM_EVENT_BYTES: usize = 16 * 1024 * 1024;

pub const MIN_WIRE_HEADROOM_BYTES: usize = 64 * 1024;

pub const MAX_RETAINED_RESPONSE_BYTES_ENV: &str = "AGENTIC_MAX_RETAINED_RESPONSE_BYTES";
pub const MAX_UPSTREAM_JSON_BYTES_ENV: &str = "AGENTIC_MAX_UPSTREAM_JSON_BYTES";
pub const MAX_UPSTREAM_SSE_LINE_BYTES_ENV: &str = "AGENTIC_MAX_UPSTREAM_SSE_LINE_BYTES";
pub const MAX_STREAM_EVENT_BYTES_ENV: &str = "AGENTIC_MAX_STREAM_EVENT_BYTES";

/// Overrides for one exact model ID on the configured Responses upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResponsesModelPolicy {
    /// Whether requests may set `temperature` or `top_p`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_supported: Option<bool>,
    /// Whether requests may set `top_logprobs` or include output-text logprobs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs_supported: Option<bool>,
    /// Accepted explicit reasoning efforts, using the upstream's vocabulary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_efforts: Option<Vec<String>>,
    /// Accepted explicit service tiers for this deployment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tiers: Option<Vec<String>>,
    /// Overrides the deployment-wide cache mode/TTL assertion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_options_supported: Option<bool>,
    /// Whether requests may set the legacy retention field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention_supported: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesConfig {
    /// Fallback assertion for models without a specific cache mode/TTL override.
    pub prompt_cache_options_supported: bool,
    pub model_policies: BTreeMap<String, ResponsesModelPolicy>,
    pub max_retained_bytes: usize,
    pub max_upstream_json_bytes: usize,
    pub max_upstream_sse_line_bytes: usize,
    pub max_stream_event_bytes: usize,
}

impl Default for ResponsesConfig {
    fn default() -> Self {
        Self {
            prompt_cache_options_supported: false,
            model_policies: BTreeMap::new(),
            max_retained_bytes: DEFAULT_MAX_RETAINED_RESPONSE_BYTES,
            max_upstream_json_bytes: DEFAULT_MAX_UPSTREAM_JSON_BYTES,
            max_upstream_sse_line_bytes: DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES,
            max_stream_event_bytes: DEFAULT_MAX_STREAM_EVENT_BYTES,
        }
    }
}

impl ResponsesConfig {
    /// Validates internal consistency between configured limits.
    ///
    /// The wire limits (`max_stream_event_bytes`, `max_upstream_sse_line_bytes`,
    /// and `max_upstream_json_bytes`) must exceed `max_retained_bytes` by proportional
    /// wire headroom (`max(MIN_WIRE_HEADROOM_BYTES, max_retained_bytes / 4)`) to account for
    /// JSON serialization overhead, escaping, and message envelopes.
    ///
    /// # Errors
    /// Returns [`Error::Config`] when streaming delivery, upstream line, or JSON limits
    /// cannot admit the retained response plus wire headroom.
    pub fn validate(&self) -> Result<(), Error> {
        let headroom = MIN_WIRE_HEADROOM_BYTES.max(self.max_retained_bytes / 4);
        let required = self.max_retained_bytes.saturating_add(headroom);
        if self.max_stream_event_bytes < required {
            return Err(Error::Config(format!(
                "max_stream_event_bytes ({}) cannot be smaller than max_retained_bytes ({}) plus headroom ({})",
                self.max_stream_event_bytes, self.max_retained_bytes, required
            )));
        }
        if self.max_upstream_sse_line_bytes < required {
            return Err(Error::Config(format!(
                "max_upstream_sse_line_bytes ({}) cannot be smaller than max_retained_bytes ({}) plus headroom ({})",
                self.max_upstream_sse_line_bytes, self.max_retained_bytes, required
            )));
        }
        if self.max_upstream_json_bytes < required {
            return Err(Error::Config(format!(
                "max_upstream_json_bytes ({}) cannot be smaller than max_retained_bytes ({}) plus headroom ({})",
                self.max_upstream_json_bytes, self.max_retained_bytes, required
            )));
        }
        Ok(())
    }
}
