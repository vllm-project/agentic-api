//! Gateway admission settings from the `[server]` table.
//!
//! Each setting resolves as CLI argument > environment variable >
//! configuration file > default. An explicit CLI argument short-circuits the
//! lower-priority sources, so a stale or malformed value inherited from the
//! environment cannot block startup when the operator names a valid value.

use std::env::VarError;
use std::num::{NonZeroU32, NonZeroUsize};

use agentic_core::error::Error;
use agentic_server::app::DEFAULT_MAX_REQUEST_BODY_SIZE;
use agentic_server::rate_limit::RateLimitConfig;
use clap::Args;

use crate::config_file::ServerFileConfig;

/// Environment override for the serialized request-size ceiling.
pub(crate) const MAX_REQUEST_BODY_SIZE_ENV: &str = "AGENTIC_MAX_REQUEST_BODY_SIZE_BYTES";
/// Environment override for each caller's request rate.
pub(crate) const RATE_LIMIT_REQUESTS_PER_MINUTE_ENV: &str = "AGENTIC_RATE_LIMIT_REQUESTS_PER_MINUTE";
/// Environment override for each caller's burst.
pub(crate) const RATE_LIMIT_BURST_ENV: &str = "AGENTIC_RATE_LIMIT_BURST";

#[derive(Args, Clone, Debug, Default)]
pub(crate) struct RateLimitArgs {
    /// Requests each caller may send per minute; unset disables rate limiting.
    /// Overrides `AGENTIC_RATE_LIMIT_REQUESTS_PER_MINUTE` and
    /// `server.rate_limit_requests_per_minute` in the configuration file.
    #[arg(long, global = true)]
    pub rate_limit_requests_per_minute: Option<NonZeroU32>,

    /// Requests a caller may send at once before the per-minute rate applies;
    /// defaults to the per-minute rate. Overrides `AGENTIC_RATE_LIMIT_BURST` and
    /// `server.rate_limit_burst` in the configuration file.
    #[arg(long, global = true)]
    pub rate_limit_burst: Option<NonZeroU32>,
}

pub(crate) fn resolve_max_request_body_size(
    cli: Option<NonZeroUsize>,
    file: Option<NonZeroUsize>,
) -> Result<NonZeroUsize, Error> {
    resolve_max_request_body_size_value(cli, file, std::env::var(MAX_REQUEST_BODY_SIZE_ENV))
}

pub(crate) fn resolve_max_request_body_size_value(
    cli: Option<NonZeroUsize>,
    file: Option<NonZeroUsize>,
    value: Result<String, VarError>,
) -> Result<NonZeroUsize, Error> {
    if let Some(cli) = cli {
        return Ok(cli);
    }
    crate::parse_env_nonzero_usize_value(
        MAX_REQUEST_BODY_SIZE_ENV,
        value,
        file.unwrap_or(DEFAULT_MAX_REQUEST_BODY_SIZE),
    )
}

/// The per-caller rate limit, or `None` when no rate is configured.
pub(crate) fn resolve_rate_limit(
    cli: &RateLimitArgs,
    file: &ServerFileConfig,
) -> Result<Option<RateLimitConfig>, Error> {
    resolve_rate_limit_values(
        cli,
        file,
        std::env::var(RATE_LIMIT_REQUESTS_PER_MINUTE_ENV),
        std::env::var(RATE_LIMIT_BURST_ENV),
    )
}

pub(crate) fn resolve_rate_limit_values(
    cli: &RateLimitArgs,
    file: &ServerFileConfig,
    requests_per_minute: Result<String, VarError>,
    burst: Result<String, VarError>,
) -> Result<Option<RateLimitConfig>, Error> {
    let requests_per_minute = resolve_nonzero_u32(
        RATE_LIMIT_REQUESTS_PER_MINUTE_ENV,
        cli.rate_limit_requests_per_minute,
        requests_per_minute,
        file.rate_limit_requests_per_minute,
    )?;
    let burst = resolve_nonzero_u32(RATE_LIMIT_BURST_ENV, cli.rate_limit_burst, burst, file.rate_limit_burst)?;
    match (requests_per_minute, burst) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(Error::Config(format!(
            "{RATE_LIMIT_BURST_ENV} requires {RATE_LIMIT_REQUESTS_PER_MINUTE_ENV}"
        ))),
        (Some(requests_per_minute), burst) => RateLimitConfig::new(requests_per_minute, burst)
            .map(Some)
            .map_err(|error| Error::Config(format!("invalid {RATE_LIMIT_BURST_ENV}: {error}"))),
    }
}

fn resolve_nonzero_u32(
    name: &str,
    cli: Option<NonZeroU32>,
    value: Result<String, VarError>,
    file: Option<NonZeroU32>,
) -> Result<Option<NonZeroU32>, Error> {
    if cli.is_some() {
        return Ok(cli);
    }
    match value {
        Ok(value) => value
            .parse::<NonZeroU32>()
            .map(Some)
            .map_err(|error| Error::Config(format!("{name} must be a positive integer: {error}"))),
        Err(VarError::NotPresent) => Ok(file),
        Err(error) => Err(Error::Config(format!("failed to read {name}: {error}"))),
    }
}

/// The `[server]` table a generated configuration file records from the environment.
pub(crate) fn generated_server_file_config(environment: impl Fn(&str) -> Option<String>) -> ServerFileConfig {
    ServerFileConfig {
        max_request_body_size_bytes: environment(MAX_REQUEST_BODY_SIZE_ENV).and_then(|value| value.parse().ok()),
        rate_limit_requests_per_minute: environment(RATE_LIMIT_REQUESTS_PER_MINUTE_ENV)
            .and_then(|value| value.parse().ok()),
        rate_limit_burst: environment(RATE_LIMIT_BURST_ENV).and_then(|value| value.parse().ok()),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    const MISSING: Result<String, VarError> = Err(VarError::NotPresent);

    fn rate(value: u32) -> Option<NonZeroU32> {
        NonZeroU32::new(value)
    }

    fn file(requests_per_minute: Option<NonZeroU32>, burst: Option<NonZeroU32>) -> ServerFileConfig {
        ServerFileConfig {
            rate_limit_requests_per_minute: requests_per_minute,
            rate_limit_burst: burst,
            ..ServerFileConfig::default()
        }
    }

    fn resolved(
        cli: &RateLimitArgs,
        file: &ServerFileConfig,
        requests_per_minute: Result<String, VarError>,
        burst: Result<String, VarError>,
    ) -> Option<(u32, u32)> {
        resolve_rate_limit_values(cli, file, requests_per_minute, burst)
            .unwrap()
            .map(|config| (config.requests_per_minute().get(), config.burst().get()))
    }

    #[test]
    fn rate_limiting_is_off_unless_a_rate_is_configured() {
        assert_eq!(
            resolved(
                &RateLimitArgs::default(),
                &ServerFileConfig::default(),
                MISSING,
                MISSING
            ),
            None
        );
    }

    #[test]
    fn each_rate_limit_knob_resolves_cli_then_environment_then_file() {
        let none = RateLimitArgs::default();
        let file = file(rate(60), rate(5));
        assert_eq!(resolved(&none, &file, MISSING, MISSING), Some((60, 5)), "file");
        assert_eq!(
            resolved(&none, &file, Ok("120".to_owned()), Ok("7".to_owned())),
            Some((120, 7)),
            "the environment overrides the file"
        );
        let cli = RateLimitArgs {
            rate_limit_requests_per_minute: rate(240),
            rate_limit_burst: rate(9),
        };
        assert_eq!(
            resolved(&cli, &file, Ok("0".to_owned()), Ok("x".to_owned())),
            Some((240, 9)),
            "the CLI overrides a malformed environment value"
        );
    }

    #[test]
    fn the_burst_defaults_to_the_rate() {
        let file = file(rate(30), None);
        assert_eq!(
            resolved(&RateLimitArgs::default(), &file, MISSING, MISSING),
            Some((30, 30))
        );
    }

    #[test]
    fn invalid_rate_limit_settings_name_the_knob() {
        let none = RateLimitArgs::default();
        let empty = ServerFileConfig::default();
        for (requests_per_minute, burst, knob) in [
            (Ok("0".to_owned()), MISSING, RATE_LIMIT_REQUESTS_PER_MINUTE_ENV),
            (Ok("fast".to_owned()), MISSING, RATE_LIMIT_REQUESTS_PER_MINUTE_ENV),
            (Ok("60".to_owned()), Ok("0".to_owned()), RATE_LIMIT_BURST_ENV),
            (MISSING, Ok("5".to_owned()), RATE_LIMIT_BURST_ENV),
            (Ok("1".to_owned()), Ok("61".to_owned()), RATE_LIMIT_BURST_ENV),
        ] {
            let error = resolve_rate_limit_values(&none, &empty, requests_per_minute, burst).unwrap_err();
            assert!(error.to_string().contains(knob), "{error}");
        }
    }

    #[test]
    fn rate_limit_flags_parse_as_positive_integers() {
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            rate_limit: RateLimitArgs,
        }
        let cli = Cli::try_parse_from([
            "agentic-server",
            "--rate-limit-requests-per-minute",
            "90",
            "--rate-limit-burst",
            "3",
        ])
        .unwrap();
        assert_eq!(cli.rate_limit.rate_limit_requests_per_minute, rate(90));
        assert_eq!(cli.rate_limit.rate_limit_burst, rate(3));
        assert!(Cli::try_parse_from(["agentic-server", "--rate-limit-requests-per-minute", "0"]).is_err());
    }

    #[test]
    fn a_generated_configuration_records_the_environment() {
        let environment = |name: &str| match name {
            RATE_LIMIT_REQUESTS_PER_MINUTE_ENV => Some("60".to_owned()),
            RATE_LIMIT_BURST_ENV => Some("4".to_owned()),
            MAX_REQUEST_BODY_SIZE_ENV => Some("2048".to_owned()),
            _ => None,
        };
        let server = generated_server_file_config(environment);
        assert_eq!(server.rate_limit_requests_per_minute, rate(60));
        assert_eq!(server.rate_limit_burst, rate(4));
        assert_eq!(server.max_request_body_size_bytes, NonZeroUsize::new(2048));
    }
}
