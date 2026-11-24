//! Operator configuration for the gateway-executed `web_fetch` tool.

use std::num::NonZeroUsize;
use std::time::Duration;

/// Download ceiling per fetch: a longer body is cut here before extraction.
pub const DEFAULT_WEB_FETCH_MAX_RESPONSE_BYTES: NonZeroUsize =
    NonZeroUsize::new(10 * 1024 * 1024).expect("default is nonzero");
/// Wall-clock ceiling for one fetch, including every redirect hop.
pub const DEFAULT_WEB_FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Redirect hops followed before a fetch fails as not accessible.
pub const DEFAULT_WEB_FETCH_MAX_REDIRECTS: u8 = 5;

/// Settings of the built-in `web_fetch` executor.
///
/// Construct with [`WebFetchConfig::default`] and the `with_*` builders; the
/// struct is non-exhaustive so adding a setting is not a breaking change for
/// downstream crates.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WebFetchConfig {
    /// Whether native `web_fetch` declarations are executed. A disabled
    /// gateway rejects such a declaration with HTTP 400 instead of forwarding
    /// it in a shape the upstream cannot execute.
    pub enabled: bool,
    /// Whether fetches may reach private, loopback, link-local, carrier-grade
    /// NAT, multicast, or otherwise non-public addresses, directly or through
    /// DNS and redirects. Off by default: only deployments that fetch intranet
    /// pages on purpose should enable it. Under the default policy the fetcher
    /// also ignores the environment's proxies and connects directly, so the
    /// address check applies to the real destination; enabling this restores
    /// proxy use.
    pub allow_private_networks: bool,
    /// Bytes read from one response body before the download is cut.
    pub max_response_bytes: NonZeroUsize,
    /// Time allowed for one fetch, including every redirect hop.
    pub timeout: Duration,
    /// Redirect hops followed for one fetch.
    pub max_redirects: u8,
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_private_networks: false,
            max_response_bytes: DEFAULT_WEB_FETCH_MAX_RESPONSE_BYTES,
            timeout: DEFAULT_WEB_FETCH_TIMEOUT,
            max_redirects: DEFAULT_WEB_FETCH_MAX_REDIRECTS,
        }
    }
}

impl WebFetchConfig {
    /// Turns the executor on or off.
    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Allows or refuses non-public destination addresses.
    #[must_use]
    pub const fn with_allow_private_networks(mut self, allow: bool) -> Self {
        self.allow_private_networks = allow;
        self
    }

    /// Overrides the per-fetch download ceiling.
    #[must_use]
    pub const fn with_max_response_bytes(mut self, max_response_bytes: NonZeroUsize) -> Self {
        self.max_response_bytes = max_response_bytes;
        self
    }

    /// Overrides the per-fetch time ceiling.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_enabled_public_only_and_bounded() {
        let config = WebFetchConfig::default();
        assert!(config.enabled);
        assert!(!config.allow_private_networks);
        assert_eq!(config.max_response_bytes.get(), 10 * 1024 * 1024);
        assert_eq!(config.timeout, Duration::from_secs(20));
        assert_eq!(config.max_redirects, 5);
    }

    #[test]
    fn builders_override_each_setting() {
        let config = WebFetchConfig::default()
            .with_enabled(false)
            .with_allow_private_networks(true)
            .with_max_response_bytes(NonZeroUsize::new(1024).expect("nonzero"))
            .with_timeout(Duration::from_secs(3));
        assert!(!config.enabled);
        assert!(config.allow_private_networks);
        assert_eq!(config.max_response_bytes.get(), 1024);
        assert_eq!(config.timeout, Duration::from_secs(3));
    }
}
