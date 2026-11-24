//! Deployment configuration for the gateway-executed `web_fetch` tool.

use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use agentic_core::config::WebFetchConfig;
use agentic_core::error::Error;

use crate::config_file::WebFetchFileConfig;

/// Environment override for whether native `web_fetch` declarations are executed.
const WEB_FETCH_ENABLED_ENV: &str = "AGENTIC_WEB_FETCH_ENABLED";
/// Environment override allowing fetches of non-public addresses.
const WEB_FETCH_ALLOW_PRIVATE_NETWORKS_ENV: &str = "AGENTIC_WEB_FETCH_ALLOW_PRIVATE_NETWORKS";
/// Environment override for the per-fetch download ceiling.
const WEB_FETCH_MAX_RESPONSE_BYTES_ENV: &str = "AGENTIC_WEB_FETCH_MAX_RESPONSE_BYTES";
/// Environment override for the per-fetch time ceiling, in seconds.
const WEB_FETCH_TIMEOUT_SECS_ENV: &str = "AGENTIC_WEB_FETCH_TIMEOUT_SECS";

/// Resolves the `web_fetch` settings as environment variable > configuration
/// file > default. Every setting is independent; an unset one keeps the
/// built-in default.
pub(crate) fn resolve_web_fetch_config(
    file: &WebFetchFileConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Result<WebFetchConfig, Error> {
    let defaults = WebFetchConfig::default();
    let enabled = env_bool(&env, WEB_FETCH_ENABLED_ENV)?
        .or(file.enabled)
        .unwrap_or(defaults.enabled);
    let allow_private_networks = env_bool(&env, WEB_FETCH_ALLOW_PRIVATE_NETWORKS_ENV)?
        .or(file.allow_private_networks)
        .unwrap_or(defaults.allow_private_networks);
    let max_response_bytes = env_parsed::<NonZeroUsize>(&env, WEB_FETCH_MAX_RESPONSE_BYTES_ENV)?
        .or(file.max_response_bytes)
        .unwrap_or(defaults.max_response_bytes);
    let timeout = env_parsed::<NonZeroU64>(&env, WEB_FETCH_TIMEOUT_SECS_ENV)?
        .or(file.timeout_secs)
        .map_or(defaults.timeout, |secs| Duration::from_secs(secs.get()));
    Ok(defaults
        .with_enabled(enabled)
        .with_allow_private_networks(allow_private_networks)
        .with_max_response_bytes(max_response_bytes)
        .with_timeout(timeout))
}

fn env_bool(env: &impl Fn(&str) -> Option<String>, name: &str) -> Result<Option<bool>, Error> {
    let Some(value) = env(name) else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        other => Err(Error::Config(format!("{name} must be true or false, got {other:?}"))),
    }
}

fn env_parsed<T: std::str::FromStr>(env: &impl Fn(&str) -> Option<String>, name: &str) -> Result<Option<T>, Error>
where
    T::Err: std::fmt::Display,
{
    let Some(value) = env(name) else {
        return Ok(None);
    };
    value
        .trim()
        .parse::<T>()
        .map(Some)
        .map_err(|error| Error::Config(format!("{name} must be a positive integer: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment lookup over a fixed set of variables, mirroring `environment_value`.
    fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        }
    }

    #[test]
    fn web_fetch_config_defaults_when_nothing_is_set() {
        let config = resolve_web_fetch_config(&WebFetchFileConfig::default(), env_from(&[])).expect("defaults");
        assert_eq!(config, WebFetchConfig::default());
        assert!(config.enabled);
        assert!(!config.allow_private_networks);
    }

    #[test]
    fn web_fetch_config_reads_the_file_and_lets_the_environment_win() {
        let file = WebFetchFileConfig {
            enabled: Some(false),
            allow_private_networks: Some(true),
            max_response_bytes: NonZeroUsize::new(4096),
            timeout_secs: NonZeroU64::new(7),
        };
        let config = resolve_web_fetch_config(&file, env_from(&[])).expect("file settings");
        assert!(!config.enabled);
        assert!(config.allow_private_networks);
        assert_eq!(config.max_response_bytes.get(), 4096);
        assert_eq!(config.timeout, Duration::from_secs(7));

        let config = resolve_web_fetch_config(
            &file,
            env_from(&[
                ("AGENTIC_WEB_FETCH_ENABLED", "true"),
                ("AGENTIC_WEB_FETCH_ALLOW_PRIVATE_NETWORKS", "no"),
                ("AGENTIC_WEB_FETCH_MAX_RESPONSE_BYTES", "1024"),
                ("AGENTIC_WEB_FETCH_TIMEOUT_SECS", "3"),
            ]),
        )
        .expect("environment overrides");
        assert!(config.enabled);
        assert!(!config.allow_private_networks);
        assert_eq!(config.max_response_bytes.get(), 1024);
        assert_eq!(config.timeout, Duration::from_secs(3));
    }

    #[test]
    fn web_fetch_config_rejects_invalid_environment_values() {
        for (name, value, expected) in [
            (
                "AGENTIC_WEB_FETCH_ENABLED",
                "maybe",
                "AGENTIC_WEB_FETCH_ENABLED must be true or false",
            ),
            (
                "AGENTIC_WEB_FETCH_ALLOW_PRIVATE_NETWORKS",
                "2",
                "AGENTIC_WEB_FETCH_ALLOW_PRIVATE_NETWORKS must be true or false",
            ),
            (
                "AGENTIC_WEB_FETCH_MAX_RESPONSE_BYTES",
                "0",
                "AGENTIC_WEB_FETCH_MAX_RESPONSE_BYTES must be a positive integer",
            ),
            (
                "AGENTIC_WEB_FETCH_TIMEOUT_SECS",
                "ten",
                "AGENTIC_WEB_FETCH_TIMEOUT_SECS must be a positive integer",
            ),
        ] {
            let error =
                resolve_web_fetch_config(&WebFetchFileConfig::default(), env_from(&[(name, value)])).expect_err(name);
            assert!(error.to_string().starts_with(expected), "{name}: {error}");
        }
    }
}
