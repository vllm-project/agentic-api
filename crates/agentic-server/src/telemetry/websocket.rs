//! WebSocket transport metrics: open connections and per-message queue wait.
//!
//! Each `response.create` on a connection is its own execution and is counted
//! by the core execution metrics; these instruments cover what only the
//! transport sees. Neither carries attributes, so a connection or stream
//! identifier can never become a dimension.

use std::time::Duration;

use opentelemetry::global;
use opentelemetry::metrics::{Histogram, Meter, UpDownCounter};

const INSTRUMENTATION_SCOPE: &str = "agentic_server";

/// A queued message can wait behind an entire earlier turn on its stream, so
/// the boundaries match the execution durations.
const QUEUE_WAIT_BOUNDARIES: &[f64] = &[
    0.001, 0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92, 163.84, 327.68,
];

/// Instruments shared by every WebSocket connection; cheap to clone.
#[derive(Debug, Clone)]
pub struct WebSocketMetrics {
    connections: UpDownCounter<i64>,
    queue_wait: Histogram<f64>,
}

impl WebSocketMetrics {
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            connections: meter
                .i64_up_down_counter("agentic.websocket.connections.active")
                .with_unit("{connection}")
                .with_description("Responses WebSocket connections being upgraded or open.")
                .build(),
            queue_wait: meter
                .f64_histogram("agentic.websocket.queue.wait.duration")
                .with_unit("s")
                .with_description(
                    "Time a response.create waited on its connection before execution started, including 0 for \
                     messages dispatched immediately.",
                )
                .with_boundaries(QUEUE_WAIT_BOUNDARIES.to_vec())
                .build(),
        }
    }

    /// Instruments bound to the globally registered meter provider; no-ops
    /// when none is installed.
    #[must_use]
    pub fn from_global() -> Self {
        Self::new(&global::meter(INSTRUMENTATION_SCOPE))
    }

    pub(crate) fn connection_opened(&self) {
        self.connections.add(1, &[]);
    }

    pub(crate) fn connection_closed(&self) {
        self.connections.add(-1, &[]);
    }

    pub(crate) fn record_queue_wait(&self, waited: Duration) {
        self.queue_wait.record(waited.as_secs_f64(), &[]);
    }
}
