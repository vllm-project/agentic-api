//! Typed parsing of the standard `OTEL_*` environment variables the gateway
//! interprets itself.
//!
//! Only the variables the SDK or exporter would otherwise fall back on
//! silently, or reject with an unhelpful message, are validated here
//! (exporter selection, protocol, compression, service name, and the global
//! kill switch). Endpoint, header, timeout, sampler, and batch settings are
//! read by the SDK and OTLP exporter directly, which already implement the
//! signal-specific → generic → default precedence rules.

use std::fmt;
use std::time::Duration;

const DEFAULT_SERVICE_NAME: &str = "agentic-api";

pub const OTEL_SDK_DISABLED: &str = "OTEL_SDK_DISABLED";
pub const OTEL_SERVICE_NAME: &str = "OTEL_SERVICE_NAME";
pub const OTEL_TRACES_EXPORTER: &str = "OTEL_TRACES_EXPORTER";
pub const OTEL_METRICS_EXPORTER: &str = "OTEL_METRICS_EXPORTER";
pub const OTEL_EXPORTER_OTLP_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";
pub const OTEL_EXPORTER_OTLP_TRACES_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL";
pub const OTEL_EXPORTER_OTLP_METRICS_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL";
pub const OTEL_EXPORTER_OTLP_COMPRESSION: &str = "OTEL_EXPORTER_OTLP_COMPRESSION";
pub const OTEL_EXPORTER_OTLP_TRACES_COMPRESSION: &str = "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION";
pub const OTEL_EXPORTER_OTLP_METRICS_COMPRESSION: &str = "OTEL_EXPORTER_OTLP_METRICS_COMPRESSION";

/// Which exporter a signal is delivered to.
///
/// The OpenTelemetry specification defaults exporter selection to `otlp`;
/// this gateway deliberately defaults to `None` so that telemetry is opt-in
/// and no exporter is ever constructed unless an operator asked for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExporterSelection {
    #[default]
    None,
    Otlp,
}

impl ExporterSelection {
    fn parse(var: &'static str, value: &str) -> Result<Self, TelemetryConfigError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "otlp" => Ok(Self::Otlp),
            _ => Err(TelemetryConfigError::UnknownExporter {
                var,
                value: value.to_owned(),
            }),
        }
    }
}

/// OTLP wire protocol.
///
/// Only `http/protobuf` is offered: the gateway is built without the gRPC
/// exporter and its `tonic` stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OtlpProtocol {
    #[default]
    HttpProtobuf,
}

impl OtlpProtocol {
    fn parse(var: &'static str, value: &str) -> Result<Self, TelemetryConfigError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "http/protobuf" => Ok(Self::HttpProtobuf),
            _ => Err(TelemetryConfigError::UnsupportedProtocol {
                var,
                value: value.to_owned(),
            }),
        }
    }
}

impl fmt::Display for OtlpProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpProtobuf => f.write_str("http/protobuf"),
        }
    }
}

/// Compression applied to OTLP export request bodies.
///
/// Only `gzip` is compiled in (`zstd` would add a C dependency). The
/// exporter's own parser rejects the specification's `none` value, so the
/// only way to disable compression is to leave the variable unset — which
/// this type mirrors rather than papering over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OtlpCompression {
    #[default]
    None,
    Gzip,
}

impl OtlpCompression {
    fn parse(var: &'static str, value: &str) -> Result<Self, TelemetryConfigError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "gzip" => Ok(Self::Gzip),
            _ => Err(TelemetryConfigError::UnsupportedCompression {
                var,
                value: value.to_owned(),
            }),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TelemetryConfigError {
    #[error("{var} must be `true` or `false`, got `{value}`")]
    InvalidBool { var: &'static str, value: String },
    #[error("{var} must be `none` or `otlp`, got `{value}`")]
    UnknownExporter { var: &'static str, value: String },
    #[error("{var} must be `http/protobuf` (gRPC is unavailable below Rust 1.88), got `{value}`")]
    UnsupportedProtocol { var: &'static str, value: String },
    #[error("{var} must be `gzip` or unset (zstd is not compiled in), got `{value}`")]
    UnsupportedCompression { var: &'static str, value: String },
}

/// Validated telemetry settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryConfig {
    traces: ExporterSelection,
    metrics: ExporterSelection,
    protocol: OtlpProtocol,
    /// Per-signal compression, already resolved with the signal-specific →
    /// generic → unset precedence so the exporter never has to read the
    /// compression variables itself.
    traces_compression: OtlpCompression,
    metrics_compression: OtlpCompression,
    service_name: String,
    /// Programmatic OTLP endpoint. Overrides every endpoint environment
    /// variable, so it is only set by embedding code and tests, never from
    /// the environment.
    otlp_endpoint: Option<String>,
    /// Programmatic per-export timeout; same override semantics as the
    /// endpoint.
    otlp_timeout: Option<Duration>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

impl TelemetryConfig {
    /// Telemetry fully off: no providers, no exporters, no subscriber layer.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            traces: ExporterSelection::None,
            metrics: ExporterSelection::None,
            protocol: OtlpProtocol::HttpProtobuf,
            traces_compression: OtlpCompression::None,
            metrics_compression: OtlpCompression::None,
            service_name: DEFAULT_SERVICE_NAME.to_owned(),
            otlp_endpoint: None,
            otlp_timeout: None,
        }
    }

    /// Parse from the process environment.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryConfigError`] for any recognised variable whose
    /// value is invalid; unset variables take their documented defaults.
    pub fn from_env() -> Result<Self, TelemetryConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Parse from an arbitrary lookup function.
    ///
    /// Kept separate from [`Self::from_env`] so tests never mutate the
    /// process environment.
    ///
    /// # Errors
    ///
    /// See [`Self::from_env`].
    pub fn from_lookup<F>(lookup: F) -> Result<Self, TelemetryConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        if parse_bool(OTEL_SDK_DISABLED, lookup(OTEL_SDK_DISABLED).as_deref())? {
            return Ok(Self::disabled());
        }

        let traces = parse_exporter(OTEL_TRACES_EXPORTER, lookup(OTEL_TRACES_EXPORTER).as_deref())?;
        let metrics = parse_exporter(OTEL_METRICS_EXPORTER, lookup(OTEL_METRICS_EXPORTER).as_deref())?;
        let protocol = parse_protocol(
            OTEL_EXPORTER_OTLP_PROTOCOL,
            lookup(OTEL_EXPORTER_OTLP_PROTOCOL).as_deref(),
        )?;
        // The exporter honours the signal-specific variables over the generic
        // one, so each must be validated even though only one protocol exists.
        for var in [OTEL_EXPORTER_OTLP_TRACES_PROTOCOL, OTEL_EXPORTER_OTLP_METRICS_PROTOCOL] {
            parse_protocol(var, lookup(var).as_deref())?;
        }
        let compression = parse_compression(
            OTEL_EXPORTER_OTLP_COMPRESSION,
            lookup(OTEL_EXPORTER_OTLP_COMPRESSION).as_deref(),
        )?;
        let traces_compression = parse_compression(
            OTEL_EXPORTER_OTLP_TRACES_COMPRESSION,
            lookup(OTEL_EXPORTER_OTLP_TRACES_COMPRESSION).as_deref(),
        )?
        .or(compression)
        .unwrap_or_default();
        let metrics_compression = parse_compression(
            OTEL_EXPORTER_OTLP_METRICS_COMPRESSION,
            lookup(OTEL_EXPORTER_OTLP_METRICS_COMPRESSION).as_deref(),
        )?
        .or(compression)
        .unwrap_or_default();
        let service_name = lookup(OTEL_SERVICE_NAME)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_owned());

        Ok(Self {
            traces,
            metrics,
            protocol,
            traces_compression,
            metrics_compression,
            service_name,
            otlp_endpoint: None,
            otlp_timeout: None,
        })
    }

    /// Route both signals to the given OTLP/HTTP base endpoint, ignoring the
    /// endpoint environment variables.
    #[must_use]
    pub fn with_otlp_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.otlp_endpoint = Some(endpoint.into());
        self
    }

    /// Bound every OTLP export request by `timeout`, ignoring the timeout
    /// environment variables.
    #[must_use]
    pub fn with_otlp_timeout(mut self, timeout: Duration) -> Self {
        self.otlp_timeout = Some(timeout);
        self
    }

    #[must_use]
    pub fn traces(&self) -> ExporterSelection {
        self.traces
    }

    #[must_use]
    pub fn metrics(&self) -> ExporterSelection {
        self.metrics
    }

    #[must_use]
    pub fn protocol(&self) -> OtlpProtocol {
        self.protocol
    }

    #[must_use]
    pub fn traces_compression(&self) -> OtlpCompression {
        self.traces_compression
    }

    #[must_use]
    pub fn metrics_compression(&self) -> OtlpCompression {
        self.metrics_compression
    }

    #[must_use]
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    #[must_use]
    pub fn otlp_endpoint(&self) -> Option<&str> {
        self.otlp_endpoint.as_deref()
    }

    #[must_use]
    pub fn otlp_timeout(&self) -> Option<Duration> {
        self.otlp_timeout
    }

    /// `true` when at least one signal has an exporter selected.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.traces != ExporterSelection::None || self.metrics != ExporterSelection::None
    }
}

fn parse_bool(var: &'static str, value: Option<&str>) -> Result<bool, TelemetryConfigError> {
    match value.map(str::trim) {
        None | Some("") => Ok(false),
        Some(value) if value.eq_ignore_ascii_case("true") => Ok(true),
        Some(value) if value.eq_ignore_ascii_case("false") => Ok(false),
        Some(value) => Err(TelemetryConfigError::InvalidBool {
            var,
            value: value.to_owned(),
        }),
    }
}

fn parse_exporter(var: &'static str, value: Option<&str>) -> Result<ExporterSelection, TelemetryConfigError> {
    match value.map(str::trim) {
        None | Some("") => Ok(ExporterSelection::None),
        Some(value) => ExporterSelection::parse(var, value),
    }
}

fn parse_protocol(var: &'static str, value: Option<&str>) -> Result<OtlpProtocol, TelemetryConfigError> {
    match value.map(str::trim) {
        None | Some("") => Ok(OtlpProtocol::HttpProtobuf),
        Some(value) => OtlpProtocol::parse(var, value),
    }
}

/// `None` when the variable is unset, so a signal-specific variable can fall
/// back to the generic one.
fn parse_compression(var: &'static str, value: Option<&str>) -> Result<Option<OtlpCompression>, TelemetryConfigError> {
    match value.map(str::trim) {
        None | Some("") => Ok(None),
        Some(value) => OtlpCompression::parse(var, value).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config_from(pairs: &[(&str, &str)]) -> Result<TelemetryConfig, TelemetryConfigError> {
        let vars: HashMap<&str, &str> = pairs.iter().copied().collect();
        TelemetryConfig::from_lookup(|name| vars.get(name).map(|value| (*value).to_owned()))
    }

    #[test]
    fn unset_environment_is_disabled() {
        let config = config_from(&[]).unwrap();
        assert!(!config.is_enabled());
        assert_eq!(config, TelemetryConfig::disabled());
        assert_eq!(config.service_name(), "agentic-api");
    }

    #[test]
    fn otlp_selection_enables_each_signal_independently() {
        let traces_only = config_from(&[("OTEL_TRACES_EXPORTER", "otlp")]).unwrap();
        assert!(traces_only.is_enabled());
        assert_eq!(traces_only.traces(), ExporterSelection::Otlp);
        assert_eq!(traces_only.metrics(), ExporterSelection::None);

        let metrics_only = config_from(&[("OTEL_METRICS_EXPORTER", "OTLP")]).unwrap();
        assert!(metrics_only.is_enabled());
        assert_eq!(metrics_only.traces(), ExporterSelection::None);
        assert_eq!(metrics_only.metrics(), ExporterSelection::Otlp);
    }

    #[test]
    fn sdk_disabled_overrides_exporter_selection() {
        let config = config_from(&[
            ("OTEL_SDK_DISABLED", "TRUE"),
            ("OTEL_TRACES_EXPORTER", "otlp"),
            ("OTEL_METRICS_EXPORTER", "otlp"),
            ("OTEL_SERVICE_NAME", "custom"),
        ])
        .unwrap();
        assert!(!config.is_enabled());
        assert_eq!(config, TelemetryConfig::disabled());
    }

    #[test]
    fn sdk_disabled_rejects_non_boolean_values() {
        let error = config_from(&[("OTEL_SDK_DISABLED", "1")]).unwrap_err();
        assert_eq!(
            error,
            TelemetryConfigError::InvalidBool {
                var: "OTEL_SDK_DISABLED",
                value: "1".to_owned(),
            }
        );
    }

    #[test]
    fn unknown_exporter_values_are_rejected() {
        let error = config_from(&[("OTEL_TRACES_EXPORTER", "otlp,console")]).unwrap_err();
        assert_eq!(
            error,
            TelemetryConfigError::UnknownExporter {
                var: "OTEL_TRACES_EXPORTER",
                value: "otlp,console".to_owned(),
            }
        );
    }

    #[test]
    fn grpc_protocol_is_rejected_on_every_protocol_variable() {
        for var in [
            "OTEL_EXPORTER_OTLP_PROTOCOL",
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
            "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
        ] {
            let error = config_from(&[("OTEL_TRACES_EXPORTER", "otlp"), (var, "grpc")]).unwrap_err();
            assert!(
                matches!(&error, TelemetryConfigError::UnsupportedProtocol { var: failed, value } if *failed == var && value == "grpc"),
                "{var}: {error}"
            );
            assert!(error.to_string().contains("Rust 1.88"), "{error}");
        }
    }

    #[test]
    fn http_protobuf_is_accepted_case_insensitively() {
        let config = config_from(&[
            ("OTEL_TRACES_EXPORTER", "otlp"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", " HTTP/Protobuf "),
        ])
        .unwrap();
        assert_eq!(config.protocol(), OtlpProtocol::HttpProtobuf);
    }

    #[test]
    fn compression_defaults_to_none_and_accepts_gzip() {
        let config = config_from(&[("OTEL_TRACES_EXPORTER", "otlp")]).unwrap();
        assert_eq!(config.traces_compression(), OtlpCompression::None);
        assert_eq!(config.metrics_compression(), OtlpCompression::None);

        let config = config_from(&[
            ("OTEL_TRACES_EXPORTER", "otlp"),
            ("OTEL_METRICS_EXPORTER", "otlp"),
            ("OTEL_EXPORTER_OTLP_COMPRESSION", " GZIP "),
        ])
        .unwrap();
        assert_eq!(config.traces_compression(), OtlpCompression::Gzip);
        assert_eq!(config.metrics_compression(), OtlpCompression::Gzip);
    }

    #[test]
    fn signal_specific_compression_overrides_the_generic_variable() {
        let config = config_from(&[
            ("OTEL_TRACES_EXPORTER", "otlp"),
            ("OTEL_METRICS_EXPORTER", "otlp"),
            ("OTEL_EXPORTER_OTLP_TRACES_COMPRESSION", "gzip"),
        ])
        .unwrap();
        assert_eq!(config.traces_compression(), OtlpCompression::Gzip);
        assert_eq!(config.metrics_compression(), OtlpCompression::None);
    }

    #[test]
    fn unsupported_compression_is_rejected_on_every_compression_variable() {
        for var in [
            "OTEL_EXPORTER_OTLP_COMPRESSION",
            "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION",
            "OTEL_EXPORTER_OTLP_METRICS_COMPRESSION",
        ] {
            for value in ["zstd", "none"] {
                let error = config_from(&[("OTEL_TRACES_EXPORTER", "otlp"), (var, value)]).unwrap_err();
                assert!(
                    matches!(&error, TelemetryConfigError::UnsupportedCompression { var: failed, value: got } if *failed == var && got == value),
                    "{var}={value}: {error}"
                );
            }
        }
    }

    #[test]
    fn service_name_is_trimmed_and_defaulted() {
        let config = config_from(&[("OTEL_SERVICE_NAME", "  gateway-eu  ")]).unwrap();
        assert_eq!(config.service_name(), "gateway-eu");

        let blank = config_from(&[("OTEL_SERVICE_NAME", "   ")]).unwrap();
        assert_eq!(blank.service_name(), "agentic-api");
    }

    #[test]
    fn programmatic_endpoint_is_never_read_from_the_environment() {
        let config = config_from(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318")]).unwrap();
        assert_eq!(config.otlp_endpoint(), None);
        let overridden = config.with_otlp_endpoint("http://127.0.0.1:1");
        assert_eq!(overridden.otlp_endpoint(), Some("http://127.0.0.1:1"));
    }
}
