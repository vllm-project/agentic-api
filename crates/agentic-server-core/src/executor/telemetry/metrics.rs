//! Execution, stage, token, and timing metrics.
//!
//! Every instrument is created once, from a [`Meter`], and shared through
//! [`ExecutorMetrics`], which the [`ExecutionContext`] carries. The gateway
//! binary builds it from the globally registered meter provider after
//! telemetry is initialized; embedding applications pass their own meter with
//! [`ExecutionContext::with_metrics`], and without a provider every
//! instrument is a no-op.
//!
//! Metrics never depend on the trace sampling decision: they are recorded by
//! the same guards that finalize spans, not derived from exported spans.
//! Every attribute is a closed enum or a boolean; no request, response,
//! conversation, tool, model, or URL identifier becomes a metric dimension.
//!
//! [`ExecutionContext`]: crate::executor::ExecutionContext
//! [`ExecutionContext::with_metrics`]: crate::executor::ExecutionContext::with_metrics

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

use super::{Api, DeliveryOutcome, ExecutionOutcome, FailureCategory, Route};
use crate::executor::error::ExecutorError;
use crate::tool::ToolType;
use crate::types::io::ResponseUsage;

/// Instrumentation scope for every instrument this crate creates.
pub const INSTRUMENTATION_SCOPE: &str = "agentic_core";

pub(crate) const ATTR_API: &str = "agentic.api";
pub(crate) const ATTR_ROUTE: &str = "agentic.route";
pub(crate) const ATTR_STREAM: &str = "agentic.stream";
pub(crate) const ATTR_EXECUTION_OUTCOME: &str = "agentic.execution.outcome";
pub(crate) const ATTR_DELIVERY_OUTCOME: &str = "agentic.delivery.outcome";
pub(crate) const ATTR_ERROR_TYPE: &str = "error.type";
const ATTR_STAGE: &str = "agentic.stage";
const ATTR_TOOL_TYPE: &str = "agentic.tool.type";
const ATTR_OPERATION: &str = "gen_ai.operation.name";
const ATTR_TOKEN_TYPE: &str = "gen_ai.token.type";

/// Every upstream call the executor makes is a chat-style inference.
const OPERATION_CHAT: &str = "chat";
/// `error.type` for a stage whose future was dropped before it finished.
const ERROR_TYPE_CANCELLED: &str = "cancelled";

/// Execution, stage, and delivery durations: powers of two from 10 ms to
/// about 5.5 minutes, covering multi-round tool loops on slow models.
const DURATION_BOUNDARIES: &[f64] = &[
    0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92, 163.84, 327.68,
];

/// The `gen_ai.server.time_to_first_token` semantic-convention boundaries,
/// extended to 20 s for queued or cold upstreams.
const FIRST_DATA_BOUNDARIES: &[f64] = &[
    0.001, 0.005, 0.01, 0.02, 0.04, 0.06, 0.08, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0, 20.0,
];

/// Total time a streamed execution waited on its transport.
const DELIVERY_WAIT_BOUNDARIES: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

/// The `gen_ai.client.token.usage` semantic-convention boundaries.
const TOKEN_BOUNDARIES: &[f64] = &[
    1.0,
    4.0,
    16.0,
    64.0,
    256.0,
    1024.0,
    4096.0,
    16384.0,
    65536.0,
    262_144.0,
    1_048_576.0,
    4_194_304.0,
    16_777_216.0,
    67_108_864.0,
];

/// One bucket per round up to the gateway's ten-round cap.
const ROUND_BOUNDARIES: &[f64] = &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];

/// A timed executor stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Rehydrate,
    Inference,
    Tool(ToolType),
    Compaction,
    Persist,
}

impl Stage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Rehydrate => "rehydrate",
            Self::Inference => "inference",
            Self::Tool(_) => "tool",
            Self::Compaction => "compaction",
            Self::Persist => "persist",
        }
    }
}

/// The instruments the executor records, shared by every request.
///
/// Cheap to clone. Build it once per [`Meter`]; creating instruments per
/// request would defeat the SDK's instrument cache.
#[derive(Debug, Clone)]
pub struct ExecutorMetrics {
    instruments: Arc<Instruments>,
}

#[derive(Debug)]
struct Instruments {
    execution_count: Counter<u64>,
    execution_active: UpDownCounter<i64>,
    execution_duration: Histogram<f64>,
    delivery_count: Counter<u64>,
    delivery_wait: Histogram<f64>,
    stage_duration: Histogram<f64>,
    inference_rounds: Histogram<u64>,
    token_usage: Histogram<u64>,
    first_upstream_data: Histogram<f64>,
    first_client_event: Histogram<f64>,
    first_text: Histogram<f64>,
}

impl ExecutorMetrics {
    /// Create every instrument from `meter`.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        let seconds = |name: &'static str, description: &'static str, boundaries: &[f64]| {
            meter
                .f64_histogram(name)
                .with_unit("s")
                .with_description(description)
                .with_boundaries(boundaries.to_vec())
                .build()
        };
        let instruments = Instruments {
            execution_count: meter
                .u64_counter("agentic.execution.count")
                .with_unit("{execution}")
                .with_description("Finalized executions by API, route, and execution outcome.")
                .build(),
            execution_active: meter
                .i64_up_down_counter("agentic.execution.active")
                .with_unit("{execution}")
                .with_description("Executions accepted and not yet finalized.")
                .build(),
            execution_duration: seconds(
                "agentic.execution.duration",
                "Time from accepting an execution until it is finalized.",
                DURATION_BOUNDARIES,
            ),
            delivery_count: meter
                .u64_counter("agentic.delivery.count")
                .with_unit("{execution}")
                .with_description("Finalized executions by delivery outcome.")
                .build(),
            delivery_wait: seconds(
                "agentic.delivery.wait.duration",
                "Total time a streamed execution waited for its transport to take the next frame.",
                DELIVERY_WAIT_BOUNDARIES,
            ),
            stage_duration: seconds(
                "agentic.stage.duration",
                "Duration of one executor stage: rehydration, an inference round, a gateway-executed tool \
                 call, compaction, or persistence.",
                DURATION_BOUNDARIES,
            ),
            inference_rounds: meter
                .u64_histogram("agentic.inference.rounds")
                .with_unit("{round}")
                .with_description("Upstream inference rounds per execution.")
                .with_boundaries(ROUND_BOUNDARIES.to_vec())
                .build(),
            token_usage: meter
                .u64_histogram("gen_ai.client.token.usage")
                .with_unit("{token}")
                .with_description("Tokens reported by the upstream per inference call.")
                .with_boundaries(TOKEN_BOUNDARIES.to_vec())
                .build(),
            first_upstream_data: seconds(
                "agentic.time_to_first_upstream_data",
                "Time from accepting a streamed execution until its first upstream SSE line.",
                FIRST_DATA_BOUNDARIES,
            ),
            first_client_event: seconds(
                "agentic.time_to_first_client_event",
                "Time from accepting a streamed execution until its first semantic event is handed to the \
                 transport.",
                FIRST_DATA_BOUNDARIES,
            ),
            first_text: seconds(
                "agentic.time_to_first_text",
                "Time from accepting a streamed execution until its first output-text delta is handed to the \
                 transport.",
                FIRST_DATA_BOUNDARIES,
            ),
        };
        Self {
            instruments: Arc::new(instruments),
        }
    }

    /// Instruments bound to the globally registered meter provider.
    ///
    /// With no provider registered the instruments are no-ops, and they stay
    /// no-ops: build this after the provider is installed.
    #[must_use]
    pub fn from_global() -> Self {
        Self::new(&global::meter(INSTRUMENTATION_SCOPE))
    }

    /// Record the tokens one upstream inference call reported. Nothing is
    /// recorded for a count the upstream did not send.
    pub(crate) fn record_token_usage(&self, api: Api, input: Option<u64>, output: Option<u64>) {
        for (token_type, count) in [("input", input), ("output", output)] {
            if let Some(count) = count {
                self.instruments.token_usage.record(
                    count,
                    &[
                        KeyValue::new(ATTR_OPERATION, OPERATION_CHAT),
                        KeyValue::new(ATTR_TOKEN_TYPE, token_type),
                        KeyValue::new(ATTR_API, api.as_str()),
                    ],
                );
            }
        }
    }

    /// Record the usage one Responses upstream call reported; `None` records
    /// nothing.
    pub(crate) fn record_response_usage(&self, usage: Option<&ResponseUsage>) {
        if let Some(usage) = usage {
            self.record_token_usage(
                Api::Responses,
                u64::try_from(usage.input_tokens).ok(),
                u64::try_from(usage.output_tokens).ok(),
            );
        }
    }

    /// Start timing one stage; the returned timer records when it is
    /// finished or dropped.
    pub(crate) fn stage(&self, stage: Stage) -> StageTimer {
        StageTimer {
            metrics: self.clone(),
            stage,
            started: Instant::now(),
            finished: false,
        }
    }

    /// Count the inference rounds of one execution; the count is recorded
    /// when the returned counter is dropped.
    pub(crate) fn rounds(&self, api: Api) -> RoundCounter {
        RoundCounter {
            metrics: self.clone(),
            api,
            rounds: 0,
        }
    }

    fn record_stage(&self, stage: Stage, elapsed: Duration, error_type: Option<&'static str>) {
        let mut attributes = Vec::with_capacity(3);
        attributes.push(KeyValue::new(ATTR_STAGE, stage.as_str()));
        if let Stage::Tool(kind) = stage {
            attributes.push(KeyValue::new(ATTR_TOOL_TYPE, super::stages::tool_type(kind)));
        }
        if let Some(error_type) = error_type {
            attributes.push(KeyValue::new(ATTR_ERROR_TYPE, error_type));
        }
        self.instruments
            .stage_duration
            .record(elapsed.as_secs_f64(), &attributes);
    }
}

/// Records one `agentic.stage.duration` sample exactly once: on
/// [`StageTimer::finish`], or as `cancelled` when dropped unfinished.
pub(crate) struct StageTimer {
    metrics: ExecutorMetrics,
    stage: Stage,
    started: Instant,
    finished: bool,
}

impl StageTimer {
    /// Record the stage, labelled with the error category when it failed.
    pub(crate) fn finish_result<T>(self, result: &Result<T, ExecutorError>) {
        self.finish(result.as_ref().err().map(FailureCategory::from));
    }

    /// The stage turned out not to run; record nothing.
    pub(crate) fn discard(mut self) {
        self.finished = true;
    }

    pub(crate) fn finish(mut self, failure: Option<FailureCategory>) {
        self.finished = true;
        self.metrics
            .record_stage(self.stage, self.started.elapsed(), failure.map(FailureCategory::as_str));
    }
}

impl Drop for StageTimer {
    fn drop(&mut self) {
        if !self.finished {
            self.metrics
                .record_stage(self.stage, self.started.elapsed(), Some(ERROR_TYPE_CANCELLED));
        }
    }
}

/// Counts one execution's upstream inference rounds and records the total
/// once, when dropped, however the loop ended.
pub(crate) struct RoundCounter {
    metrics: ExecutorMetrics,
    api: Api,
    rounds: u64,
}

impl RoundCounter {
    pub(crate) fn begin_round(&mut self) {
        self.rounds = self.rounds.saturating_add(1);
    }
}

impl Drop for RoundCounter {
    fn drop(&mut self) {
        if self.rounds > 0 {
            self.metrics
                .instruments
                .inference_rounds
                .record(self.rounds, &[KeyValue::new(ATTR_API, self.api.as_str())]);
        }
    }
}

/// Per-execution timing state shared by the execution guard, the upstream
/// reader, and the client relay, which may run on different tasks.
///
/// Each first-* timing is measured from the moment the execution was
/// accepted and recorded at most once.
#[derive(Debug, Clone)]
pub(crate) struct ExecutionClock {
    inner: Arc<ClockState>,
}

#[derive(Debug)]
struct ClockState {
    metrics: ExecutorMetrics,
    api: Api,
    route: Route,
    stream: bool,
    started: Instant,
    first_upstream_data: AtomicBool,
    first_client_event: AtomicBool,
    first_text: AtomicBool,
    handed_frames: AtomicBool,
    delivery_wait_nanos: AtomicU64,
}

impl ExecutionClock {
    /// Start the clock and count the execution as active.
    pub(super) fn start(metrics: ExecutorMetrics, api: Api, route: Route, stream: bool) -> Self {
        let clock = Self {
            inner: Arc::new(ClockState {
                metrics,
                api,
                route,
                stream,
                started: Instant::now(),
                first_upstream_data: AtomicBool::new(false),
                first_client_event: AtomicBool::new(false),
                first_text: AtomicBool::new(false),
                handed_frames: AtomicBool::new(false),
                delivery_wait_nanos: AtomicU64::new(0),
            }),
        };
        clock
            .inner
            .metrics
            .instruments
            .execution_active
            .add(1, &clock.base_attributes());
        clock
    }

    fn base_attributes(&self) -> [KeyValue; 3] {
        [
            KeyValue::new(ATTR_API, self.inner.api.as_str()),
            KeyValue::new(ATTR_ROUTE, self.inner.route.as_str()),
            KeyValue::new(ATTR_STREAM, self.inner.stream),
        ]
    }

    /// The first upstream SSE line reached the pipeline.
    pub(crate) fn upstream_data(&self) {
        self.first(&self.inner.first_upstream_data, |instruments| {
            &instruments.first_upstream_data
        });
    }

    /// A semantic event of the response is being handed to the transport.
    /// `text` marks an output-text delta.
    pub(crate) fn client_event(&self, text: bool) {
        self.first(&self.inner.first_client_event, |instruments| {
            &instruments.first_client_event
        });
        if text {
            self.first(&self.inner.first_text, |instruments| &instruments.first_text);
        }
    }

    fn first(&self, seen: &AtomicBool, histogram: impl FnOnce(&Instruments) -> &Histogram<f64>) {
        // Every later event takes the load alone, without writing the flag.
        if !seen.load(Ordering::Relaxed) && !seen.swap(true, Ordering::Relaxed) {
            histogram(&self.inner.metrics.instruments).record(
                self.inner.started.elapsed().as_secs_f64(),
                &[KeyValue::new(ATTR_API, self.inner.api.as_str())],
            );
        }
    }

    /// A frame was handed to the transport.
    pub(super) fn frame_handed(&self) {
        if !self.inner.handed_frames.load(Ordering::Relaxed) {
            self.inner.handed_frames.store(true, Ordering::Relaxed);
        }
    }

    /// The transport asked for the next frame `waited` after taking the
    /// previous one.
    pub(super) fn transport_waited(&self, waited: Duration) {
        let nanos = u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX);
        self.inner.delivery_wait_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    /// Record the execution's terminal metrics and release its active count.
    /// The execution guard calls this exactly once, from `Drop`.
    pub(super) fn finalize(
        &self,
        execution: ExecutionOutcome,
        failure: Option<FailureCategory>,
        delivery: DeliveryOutcome,
    ) {
        let instruments = &self.inner.metrics.instruments;
        let base = self.base_attributes();
        instruments.execution_active.add(-1, &base);

        let mut attributes = Vec::with_capacity(5);
        attributes.extend_from_slice(&base);
        attributes.push(KeyValue::new(ATTR_EXECUTION_OUTCOME, execution.as_str()));
        if let Some(failure) = failure {
            attributes.push(KeyValue::new(ATTR_ERROR_TYPE, failure.as_str()));
        }
        instruments.execution_count.add(1, &attributes);
        instruments
            .execution_duration
            .record(self.inner.started.elapsed().as_secs_f64(), &attributes);

        let mut attributes = base.to_vec();
        attributes.push(KeyValue::new(ATTR_DELIVERY_OUTCOME, delivery.as_str()));
        instruments.delivery_count.add(1, &attributes);

        if self.inner.handed_frames.load(Ordering::Relaxed) {
            let waited = Duration::from_nanos(self.inner.delivery_wait_nanos.load(Ordering::Relaxed));
            instruments.delivery_wait.record(
                waited.as_secs_f64(),
                &[KeyValue::new(ATTR_API, self.inner.api.as_str())],
            );
        }
    }
}
