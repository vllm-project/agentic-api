//! Opt-in OpenTelemetry integration for the gateway binary.
//!
//! Telemetry is off unless `OTEL_TRACES_EXPORTER` and/or
//! `OTEL_METRICS_EXPORTER` select `otlp`; with nothing selected no exporter,
//! provider, or bridge layer is created and only the local log subscriber is
//! installed. Providers, exporters, and the global subscriber are owned here,
//! in the server crate: `agentic-server-core` only ever sees the
//! `opentelemetry` API and `tracing`.

pub mod config;
pub mod http;
mod lifecycle;
pub(crate) mod proxy;
mod subscriber;

pub use config::{ExporterSelection, OtlpCompression, OtlpProtocol, TelemetryConfig, TelemetryConfigError};
pub use lifecycle::{DEFAULT_SHUTDOWN_TIMEOUT, InstrumentationHandles, Signal, TelemetryError, TelemetryGuard};
pub use subscriber::{DEFAULT_LOG_FILTER, build_subscriber};

/// Build providers for the enabled signals and register them globally,
/// without installing a `tracing` subscriber.
///
/// This is the entry point for embedding applications that own their own
/// subscriber; the gateway binary uses [`init`].
///
/// # Errors
///
/// Returns [`TelemetryError::ExporterBuild`] when an exporter cannot be
/// constructed.
pub fn init_providers(config: &TelemetryConfig) -> Result<(TelemetryGuard, InstrumentationHandles), TelemetryError> {
    let (providers, handles) = lifecycle::build_providers(config)?;
    lifecycle::install_globals(&providers);
    Ok((TelemetryGuard::from_providers(providers), handles))
}

/// Build and register providers, then install the global subscriber.
///
/// Call once, before starting the Tokio runtime, so the returned guard can be
/// shut down after the runtime has drained and dropped after it has stopped.
///
/// # Errors
///
/// Returns [`TelemetryError::ExporterBuild`] when an exporter cannot be
/// constructed and [`TelemetryError::Subscriber`] when a global subscriber is
/// already installed; providers created before a subscriber failure are shut
/// down synchronously before returning.
pub fn init(config: &TelemetryConfig) -> Result<TelemetryGuard, TelemetryError> {
    let (providers, handles) = lifecycle::build_providers(config)?;
    if let Err(error) = subscriber::install(handles.tracer) {
        // Best effort, synchronous: nothing else can flush these providers.
        let _ = providers.shutdown_blocking(DEFAULT_SHUTDOWN_TIMEOUT);
        return Err(error);
    }
    lifecycle::install_globals(&providers);
    Ok(TelemetryGuard::from_providers(providers))
}
