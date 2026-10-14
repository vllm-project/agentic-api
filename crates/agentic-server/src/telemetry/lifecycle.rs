//! OpenTelemetry SDK provider construction, global registration, and
//! deadline-bounded shutdown.
//!
//! Nothing in this module runs unless the configuration enables at least one
//! signal, and nothing here is visible to `agentic-server-core`: embedding
//! applications supply their own providers.

use std::fmt;
use std::time::Duration;

use opentelemetry::metrics::{Meter, MeterProvider as _};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::{
    Compression, ExporterBuildError, MetricExporter, SpanExporter, WithExportConfig, WithHttpConfig,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkError;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use tracing::warn;

use super::config::{ExporterSelection, OtlpCompression, TelemetryConfig, TelemetryConfigError};

/// Upper bound for flushing and shutting down providers at process exit.
///
/// Independent of the gateway drain timeout: both run back to back, and the
/// sum must stay well inside the deployment's termination grace period.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Instrumentation scope reported for spans and metrics created by this crate.
const INSTRUMENTATION_SCOPE: &str = "agentic_server";

const SERVICE_VERSION_KEY: &str = "service.version";

/// Telemetry signal, used to attribute errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Traces,
    Metrics,
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Traces => f.write_str("traces"),
            Self::Metrics => f.write_str("metrics"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("invalid telemetry configuration: {0}")]
    Config(#[from] TelemetryConfigError),
    #[error("failed to build OTLP {signal} exporter: {source}")]
    ExporterBuild {
        signal: Signal,
        #[source]
        source: ExporterBuildError,
    },
    #[error("failed to install tracing subscriber: {0}")]
    Subscriber(#[from] tracing_subscriber::util::TryInitError),
    #[error("telemetry shutdown did not finish within {deadline:?}")]
    ShutdownTimeout { deadline: Duration },
    #[error("telemetry shutdown task failed: {0}")]
    ShutdownJoin(#[source] tokio::task::JoinError),
    #[error("telemetry shutdown thread failed: {0}")]
    ShutdownThread(#[source] std::io::Error),
    #[error("{signal} provider shutdown failed: {source}")]
    ProviderShutdown {
        signal: Signal,
        #[source]
        source: OTelSdkError,
    },
}

/// Handles for creating spans and instruments from the configured providers.
///
/// Each handle is `None` when its signal is not exported.
#[derive(Debug, Clone, Default)]
pub struct InstrumentationHandles {
    pub tracer: Option<SdkTracer>,
    pub meter: Option<Meter>,
}

/// The SDK providers that own exporter threads and buffers.
#[derive(Debug, Default)]
pub(crate) struct Providers {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
}

impl Providers {
    fn is_empty(&self) -> bool {
        self.tracer.is_none() && self.meter.is_none()
    }

    /// Blocking shutdown of every provider, reporting the first failure.
    ///
    /// The tracer provider honours `deadline`; the meter provider's shutdown
    /// ignores its timeout argument in SDK 0.32, so callers must bound this
    /// call themselves.
    pub(crate) fn shutdown_blocking(self, deadline: Duration) -> Result<(), TelemetryError> {
        let mut first_error = None;
        if let Some(tracer) = self.tracer
            && let Err(source) = tracer.shutdown_with_timeout(deadline)
        {
            first_error.get_or_insert(TelemetryError::ProviderShutdown {
                signal: Signal::Traces,
                source,
            });
        }
        if let Some(meter) = self.meter
            && let Err(source) = meter.shutdown()
        {
            first_error.get_or_insert(TelemetryError::ProviderShutdown {
                signal: Signal::Metrics,
                source,
            });
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// Build providers for every enabled signal without touching global state.
///
/// Returns an empty [`Providers`] (and no handles) when the configuration is
/// disabled, so no exporter, thread, or buffer is created.
///
/// # Errors
///
/// Returns [`TelemetryError::ExporterBuild`] when an OTLP exporter cannot be
/// constructed (for example an unparsable programmatic endpoint).
pub(crate) fn build_providers(config: &TelemetryConfig) -> Result<(Providers, InstrumentationHandles), TelemetryError> {
    if !config.is_enabled() {
        return Ok((Providers::default(), InstrumentationHandles::default()));
    }

    let resource = Resource::builder()
        .with_service_name(config.service_name().to_owned())
        .with_attribute(KeyValue::new(SERVICE_VERSION_KEY, env!("CARGO_PKG_VERSION")))
        .build();

    let mut providers = Providers::default();
    let mut handles = InstrumentationHandles::default();

    if config.traces() == ExporterSelection::Otlp {
        let mut builder = SpanExporter::builder().with_http();
        if let Some(endpoint) = config.otlp_endpoint() {
            builder = builder.with_endpoint(signal_url(endpoint, "v1/traces"));
        }
        if let Some(timeout) = config.otlp_timeout() {
            builder = builder.with_timeout(timeout);
        }
        if let Some(compression) = exporter_compression(config.traces_compression()) {
            builder = builder.with_compression(compression);
        }
        let exporter = builder.build().map_err(|source| TelemetryError::ExporterBuild {
            signal: Signal::Traces,
            source,
        })?;
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource.clone())
            .build();
        handles.tracer = Some(provider.tracer(INSTRUMENTATION_SCOPE));
        providers.tracer = Some(provider);
    }

    if config.metrics() == ExporterSelection::Otlp {
        let mut builder = MetricExporter::builder().with_http();
        if let Some(endpoint) = config.otlp_endpoint() {
            builder = builder.with_endpoint(signal_url(endpoint, "v1/metrics"));
        }
        if let Some(timeout) = config.otlp_timeout() {
            builder = builder.with_timeout(timeout);
        }
        if let Some(compression) = exporter_compression(config.metrics_compression()) {
            builder = builder.with_compression(compression);
        }
        let exporter = builder.build().map_err(|source| TelemetryError::ExporterBuild {
            signal: Signal::Metrics,
            source,
        })?;
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter).build())
            .with_resource(resource)
            .build();
        handles.meter = Some(provider.meter(INSTRUMENTATION_SCOPE));
        providers.meter = Some(provider);
    }

    Ok((providers, handles))
}

/// The exporter builder only takes an algorithm, never "none": for `None` the
/// builder is left untouched, and the exporter then reads the (validated,
/// unset) compression variables itself and sends bodies uncompressed.
fn exporter_compression(compression: OtlpCompression) -> Option<Compression> {
    match compression {
        OtlpCompression::None => None,
        OtlpCompression::Gzip => Some(Compression::Gzip),
    }
}

/// Register the providers and the W3C `traceparent` propagator globally so
/// instrumentation that only sees the `opentelemetry` API can reach them.
pub(crate) fn install_globals(providers: &Providers) {
    if let Some(tracer) = &providers.tracer {
        global::set_tracer_provider(tracer.clone());
        global::set_text_map_propagator(TraceContextPropagator::new());
    }
    if let Some(meter) = &providers.meter {
        global::set_meter_provider(meter.clone());
    }
}

/// A programmatic endpoint is used verbatim by the exporter, so the signal
/// path the environment-variable path would append must be added here.
fn signal_url(base: &str, path: &str) -> String {
    format!("{}/{path}", base.trim_end_matches('/'))
}

#[derive(Debug)]
enum GuardState {
    Disabled,
    Enabled(Providers),
    ShutDown,
}

/// Owns the provider lifecycle for the process.
///
/// Call [`TelemetryGuard::shutdown`] after request draining; dropping an
/// enabled guard without shutting it down logs a warning and may lose
/// buffered telemetry, but never blocks.
#[derive(Debug)]
pub struct TelemetryGuard {
    state: GuardState,
}

impl TelemetryGuard {
    #[must_use]
    pub(crate) fn disabled() -> Self {
        Self {
            state: GuardState::Disabled,
        }
    }

    pub(crate) fn from_providers(providers: Providers) -> Self {
        if providers.is_empty() {
            return Self::disabled();
        }
        Self {
            state: GuardState::Enabled(providers),
        }
    }

    /// `true` when at least one provider is alive.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        matches!(self.state, GuardState::Enabled(_))
    }

    fn take_providers(&mut self) -> Option<Providers> {
        match std::mem::replace(&mut self.state, GuardState::ShutDown) {
            GuardState::Enabled(providers) => Some(providers),
            GuardState::Disabled | GuardState::ShutDown => None,
        }
    }

    /// Flush and shut down every provider on the blocking pool, returning
    /// within `deadline` even if an exporter is stuck on a slow collector.
    ///
    /// For callers inside a Tokio runtime. The gateway binary instead uses
    /// [`Self::shutdown_blocking`] after its runtime has stopped, so that
    /// request tasks abandoned by the drain deadline have already dropped
    /// their spans and metrics into the still-running providers.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::ShutdownTimeout`] when the deadline elapses
    /// (the blocking shutdown keeps running in the background and is bounded
    /// by the exporter timeout), [`TelemetryError::ProviderShutdown`] when a
    /// provider reports a failure, and [`TelemetryError::ShutdownJoin`] if
    /// the blocking task panics.
    pub async fn shutdown(mut self, deadline: Duration) -> Result<(), TelemetryError> {
        let Some(providers) = self.take_providers() else {
            return Ok(());
        };
        let task = tokio::task::spawn_blocking(move || providers.shutdown_blocking(deadline));
        match tokio::time::timeout(deadline, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(join_error)) => Err(TelemetryError::ShutdownJoin(join_error)),
            Err(_elapsed) => Err(TelemetryError::ShutdownTimeout { deadline }),
        }
    }

    /// Flush and shut down every provider from a plain thread, returning
    /// within `deadline`.
    ///
    /// Must not be called from inside a Tokio runtime (it blocks the calling
    /// thread). If the deadline elapses the shutdown thread is left running;
    /// it is bounded by the exporter timeout and dies with the process.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::ShutdownTimeout`] when the deadline elapses,
    /// [`TelemetryError::ProviderShutdown`] when a provider reports a failure,
    /// and [`TelemetryError::ShutdownThread`] if the thread cannot be spawned
    /// or panics.
    pub fn shutdown_blocking(mut self, deadline: Duration) -> Result<(), TelemetryError> {
        let Some(providers) = self.take_providers() else {
            return Ok(());
        };
        let (done, result) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("agentic-telemetry-shutdown".to_owned())
            .spawn(move || {
                // The receiver may be gone if the deadline already passed.
                let _ = done.send(providers.shutdown_blocking(deadline));
            })
            .map_err(TelemetryError::ShutdownThread)?;
        match result.recv_timeout(deadline) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(TelemetryError::ShutdownTimeout { deadline }),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(TelemetryError::ShutdownThread(
                std::io::Error::other("telemetry shutdown thread exited without reporting"),
            )),
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let GuardState::Enabled(_) = self.state {
            warn!("telemetry guard dropped without shutdown; buffered traces and metrics may be lost");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_url_appends_path_once() {
        assert_eq!(
            signal_url("http://127.0.0.1:4318", "v1/traces"),
            "http://127.0.0.1:4318/v1/traces"
        );
        assert_eq!(
            signal_url("http://127.0.0.1:4318/", "v1/metrics"),
            "http://127.0.0.1:4318/v1/metrics"
        );
    }

    #[test]
    fn disabled_config_builds_nothing() {
        let (providers, handles) = build_providers(&TelemetryConfig::disabled()).unwrap();
        assert!(providers.is_empty());
        assert!(handles.tracer.is_none());
        assert!(handles.meter.is_none());
        assert!(!TelemetryGuard::from_providers(providers).is_enabled());
    }
}
