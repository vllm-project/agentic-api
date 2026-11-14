//! Responses resource limits, upstream deadlines, and operator-asserted cache support.

use std::num::NonZeroU64;
use std::time::Duration;

use crate::error::Error;

pub const DEFAULT_MAX_RETAINED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_UPSTREAM_JSON_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_STREAM_EVENT_BYTES: usize = 16 * 1024 * 1024;

pub const DEFAULT_UPSTREAM_CONNECT_TIMEOUT_S: NonZeroU64 = NonZeroU64::new(10).expect("nonzero default");
pub const DEFAULT_UPSTREAM_RESPONSE_HEADER_TIMEOUT_S: NonZeroU64 = NonZeroU64::new(600).expect("nonzero default");
pub const DEFAULT_UPSTREAM_TOTAL_TIMEOUT_S: NonZeroU64 = NonZeroU64::new(3600).expect("nonzero default");

pub const MIN_WIRE_HEADROOM_BYTES: usize = 64 * 1024;

pub const MAX_RETAINED_RESPONSE_BYTES_ENV: &str = "AGENTIC_MAX_RETAINED_RESPONSE_BYTES";
pub const MAX_UPSTREAM_JSON_BYTES_ENV: &str = "AGENTIC_MAX_UPSTREAM_JSON_BYTES";
pub const MAX_UPSTREAM_SSE_LINE_BYTES_ENV: &str = "AGENTIC_MAX_UPSTREAM_SSE_LINE_BYTES";
pub const MAX_STREAM_EVENT_BYTES_ENV: &str = "AGENTIC_MAX_STREAM_EVENT_BYTES";
pub const UPSTREAM_CONNECT_TIMEOUT_S_ENV: &str = "AGENTIC_UPSTREAM_CONNECT_TIMEOUT_S";
pub const UPSTREAM_RESPONSE_HEADER_TIMEOUT_S_ENV: &str = "AGENTIC_UPSTREAM_RESPONSE_HEADER_TIMEOUT_S";
pub const UPSTREAM_TOTAL_TIMEOUT_S_ENV: &str = "AGENTIC_UPSTREAM_TOTAL_TIMEOUT_S";

/// Deadlines applied to one typed upstream inference request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UpstreamTimeouts {
    pub response_header: Duration,
    pub total: Duration,
}

impl UpstreamTimeouts {
    #[cfg(test)]
    pub(crate) const fn new(response_header: Duration, total: Duration) -> Self {
        Self { response_header, total }
    }
}

impl Default for UpstreamTimeouts {
    fn default() -> Self {
        Self {
            response_header: Duration::from_secs(DEFAULT_UPSTREAM_RESPONSE_HEADER_TIMEOUT_S.get()),
            total: Duration::from_secs(DEFAULT_UPSTREAM_TOTAL_TIMEOUT_S.get()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponsesConfig {
    /// Operator assertion that typed execution supports the mode/TTL contract for every served model.
    /// Direct HTTP pass-through does not consult this switch.
    pub prompt_cache_options_supported: bool,
    pub max_retained_bytes: usize,
    pub max_upstream_json_bytes: usize,
    pub max_upstream_sse_line_bytes: usize,
    pub max_stream_event_bytes: usize,
    /// Connection deadline for the shared typed-executor HTTP client.
    pub upstream_connect_timeout_s: NonZeroU64,
    /// Deadline for receiving response headers from one inference round.
    pub upstream_response_header_timeout_s: NonZeroU64,
    /// Deadline for one complete inference round, including its response body.
    pub upstream_total_timeout_s: NonZeroU64,
}

impl Default for ResponsesConfig {
    fn default() -> Self {
        Self {
            prompt_cache_options_supported: false,
            max_retained_bytes: DEFAULT_MAX_RETAINED_RESPONSE_BYTES,
            max_upstream_json_bytes: DEFAULT_MAX_UPSTREAM_JSON_BYTES,
            max_upstream_sse_line_bytes: DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES,
            max_stream_event_bytes: DEFAULT_MAX_STREAM_EVENT_BYTES,
            upstream_connect_timeout_s: DEFAULT_UPSTREAM_CONNECT_TIMEOUT_S,
            upstream_response_header_timeout_s: DEFAULT_UPSTREAM_RESPONSE_HEADER_TIMEOUT_S,
            upstream_total_timeout_s: DEFAULT_UPSTREAM_TOTAL_TIMEOUT_S,
        }
    }
}

impl ResponsesConfig {
    /// Validates internal consistency between configured limits and deadlines.
    ///
    /// The wire limits (`max_stream_event_bytes`, `max_upstream_sse_line_bytes`,
    /// and `max_upstream_json_bytes`) must exceed `max_retained_bytes` by proportional
    /// wire headroom (`max(MIN_WIRE_HEADROOM_BYTES, max_retained_bytes / 4)`) to account for
    /// JSON serialization overhead, escaping, and message envelopes.
    ///
    /// # Errors
    /// Returns [`Error::Config`] when deadlines are not ordered or wire limits
    /// cannot admit the retained response plus wire headroom.
    pub fn validate(&self) -> Result<(), Error> {
        if self.upstream_connect_timeout_s > self.upstream_response_header_timeout_s {
            return Err(Error::Config(format!(
                "upstream_connect_timeout_s ({}) cannot exceed upstream_response_header_timeout_s ({})",
                self.upstream_connect_timeout_s, self.upstream_response_header_timeout_s
            )));
        }
        if self.upstream_response_header_timeout_s > self.upstream_total_timeout_s {
            return Err(Error::Config(format!(
                "upstream_response_header_timeout_s ({}) cannot exceed upstream_total_timeout_s ({})",
                self.upstream_response_header_timeout_s, self.upstream_total_timeout_s
            )));
        }
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

    #[must_use]
    pub(crate) const fn upstream_connect_timeout(&self) -> Duration {
        Duration::from_secs(self.upstream_connect_timeout_s.get())
    }

    #[must_use]
    pub(crate) const fn upstream_timeouts(&self) -> UpstreamTimeouts {
        UpstreamTimeouts {
            response_header: Duration::from_secs(self.upstream_response_header_timeout_s.get()),
            total: Duration::from_secs(self.upstream_total_timeout_s.get()),
        }
    }
}
