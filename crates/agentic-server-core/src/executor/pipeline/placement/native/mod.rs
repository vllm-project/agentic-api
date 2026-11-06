// Private native-core/relay experiment. No production placement policy changes.

#![cfg(test)]

mod batch;
mod client;
mod extended_gate;
mod gate;
mod gateway_round;
mod gateway_tests;
mod http_profile;
mod http_server;
mod http_tests;
mod matrix;
mod matrix_gate;
mod metrics;
mod process;
mod process_calibration;
mod profile;
mod queue;
mod queue_math;
mod queue_profile;
mod queue_tests;
mod scheduler_probe;
mod source;
mod tests;
mod transport;
mod worker;
mod workload_audit;

use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::arrival::ArrivalSchedule;
use super::driver::{DriverError, FinishFence, Limits, Placement};
use super::fault_probe::EarlyFailureProbe;
use crate::events::{ClassifiedSseLine, SseLine};
use crate::executor::accumulator::Validation;
use crate::executor::pipeline::RoundIngestion;
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::executor::translate::{Translation, TranslationContext};
use crate::tool::ToolRegistry;
use crate::types::request_response::ResponsePayload;
use client::Consumer;
pub(super) use metrics::{ClientProbe, Observation, Ownership};
pub(super) use source::Source;

type Result<T> = std::result::Result<T, DriverError>;
const MAX_LINES: usize = 20_000;

#[derive(Clone)]
pub(super) struct Settings {
    pub placement: Placement,
    pub limits: Limits,
    pub relay: bool,
    pub arrival_spacing: Duration,
    pub client_delay: Duration,
    pub client_entries: usize,
    pub capture: bool,
    pub finish_fence: Option<FinishFence>,
    pub disconnect_after: Option<usize>,
    pub client_probe: Option<ClientProbe>,
    pub early_failure_probe: Option<EarlyFailureProbe>,
    /// Explicit test-only request-scoped tools; None preserves the core/relay-only scope.
    pub registry: Option<ToolRegistry>,
    pub queue_observation: bool,
    pub replay_schedule: Option<ArrivalSchedule>,
}

impl Settings {
    pub fn new(placement: Placement, relay: bool) -> Self {
        Self {
            placement,
            limits: Limits::default(),
            relay,
            arrival_spacing: Duration::ZERO,
            client_delay: Duration::ZERO,
            client_entries: 1,
            capture: false,
            finish_fence: None,
            disconnect_after: None,
            client_probe: None,
            early_failure_probe: None,
            registry: None,
            queue_observation: false,
            replay_schedule: None,
        }
    }

    fn validate(&self, lines: usize) -> Result<()> {
        self.limits.validate()?;
        if lines > MAX_LINES
            || self.client_entries == 0
            || self.client_entries > Semaphore::MAX_PERMITS
            || (!self.capture && self.finish_fence.is_some())
            || (!self.capture && self.client_probe.is_some())
            || (!self.capture && self.early_failure_probe.is_some())
            || (!self.relay && (!self.client_delay.is_zero() || self.disconnect_after.is_some()))
            || (!self.relay && self.registry.is_some())
            || (self.replay_schedule.is_some() && !self.arrival_spacing.is_zero())
        {
            return Err(DriverError::InvalidLimits);
        }
        Ok(())
    }
}

pub(super) struct Outcome {
    pub payload: ResponsePayload,
    pub trace: Vec<String>,
    pub observations: Vec<Observation>,
    pub input_occupancy: Vec<usize>,
    pub output_occupancy: Vec<usize>,
    pub client_frames: usize,
    pub client_bytes: usize,
    pub source_lines: usize,
    pub gateway: Option<gateway_round::RoundOutcome>,
    queue_trace: Option<queue::Trace>,
}

struct Input {
    raw: String,
    due: Instant,
    _bytes: OwnedSemaphorePermit,
}

struct Output {
    translation: Translation,
    semantic: bool,
    due: Instant,
    ingestion_finished: Instant,
    logical_bytes: u32,
}

struct QueuedOutput {
    output: Output,
    _bytes: OwnedSemaphorePermit,
}

fn pipeline(settings: &Settings, budget: &ExecutorResponseBudget) -> RoundIngestion {
    let context = settings
        .registry
        .as_ref()
        .map_or_else(TranslationContext::default, |registry| {
            TranslationContext::new(
                registry
                    .tool_classifications()
                    .map(|(name, kind)| (name.to_owned(), kind))
                    .collect(),
                std::collections::HashSet::new(),
                false,
            )
            .with_gateway_owned_names(
                registry
                    .tool_classifications()
                    .filter(|(name, _)| registry.is_gateway_owned_name(name))
                    .map(|(name, _)| name.to_owned())
                    .collect(),
            )
        });
    RoundIngestion::new(
        "placement-native".to_owned(),
        None,
        Validation::Strict,
        context,
        Some(budget.clone()),
    )
}

fn check_size(bytes: usize, limit: u32, direction: &'static str) -> Result<u32> {
    let bytes = u32::try_from(bytes).map_err(|_| DriverError::ItemLimit(direction))?;
    if bytes > limit {
        return Err(DriverError::ItemLimit(direction));
    }
    Ok(bytes)
}

fn push(round: &mut RoundIngestion, raw: &str, due: Instant, limits: Limits) -> Result<Output> {
    check_size(raw.len(), limits.input_item_bytes, "input")?;
    let line = SseLine::parse(raw);
    let semantic = matches!(line, ClassifiedSseLine::Data(_));
    let translation = round.push(line)?;
    let ingestion_finished = Instant::now();
    // Same bounded logical-wire sizing in both placements. This accounting
    // work is included in measured paths; it is not heap-allocation accounting.
    let logical_bytes = check_size(
        metrics::logical_wire_bytes(&translation)?,
        limits.output_item_bytes,
        "output",
    )?;
    Ok(Output {
        translation,
        semantic,
        due,
        ingestion_finished,
        logical_bytes,
    })
}

async fn finish(round: RoundIngestion, settings: &Settings, cancel: &CancellationToken) -> Result<ResponsePayload> {
    if let Some(fence) = &settings.finish_fence {
        fence.wait(cancel).await?;
    }
    if cancel.is_cancelled() {
        return Err(DriverError::Cancelled);
    }
    Ok(round.finish("placement-model", None, None)?)
}

pub(super) async fn run(
    settings: Settings,
    lines: Vec<String>,
    control: CancellationToken,
    ownership: Ownership,
) -> Result<Outcome> {
    settings.validate(lines.len())?;
    let source = if let Some(schedule) = settings.replay_schedule {
        Source::replay_at(lines, schedule)
    } else {
        Source::replay(lines, settings.arrival_spacing)
    };
    run_source(settings, source, control, ownership).await
}

pub(super) async fn run_source(
    settings: Settings,
    mut source: Source,
    control: CancellationToken,
    ownership: Ownership,
) -> Result<Outcome> {
    settings.validate(0)?;
    if source.is_http() && (!settings.arrival_spacing.is_zero() || settings.replay_schedule.is_some()) {
        return Err(DriverError::InvalidLimits);
    }
    if settings.early_failure_probe.is_some()
        && (source.is_http() || source.count() != 0 || source.remaining().is_none_or(|count| count < 2))
    {
        return Err(DriverError::InvalidLimits);
    }
    let cancel = control.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let mut consumer = Consumer::new(&settings, cancel.clone(), ownership.clone());
    let result = match settings.placement {
        Placement::Inline => {
            let mut round = pipeline(&settings, &consumer.budget);
            let mut error = None;
            loop {
                let next = match source.next(&cancel, settings.limits).await {
                    Ok(Some(next)) => next,
                    Ok(None) => break,
                    Err(cause) => {
                        error = Some(cause);
                        break;
                    }
                };
                let step = async {
                    consumer
                        .accept(push(&mut round, &next.raw, next.due, settings.limits)?, &cancel)
                        .await
                }
                .await;
                if let Err(cause) = step {
                    error = Some(cause);
                    break;
                }
            }
            match error {
                Some(error) => Err(error),
                None => finish(round, &settings, &cancel)
                    .await
                    .map(|payload| (payload, Vec::new(), Vec::new(), source.count(), None)),
            }
        }
        Placement::Worker => worker::run(&settings, source, &cancel, &ownership, &mut consumer).await,
    };
    let result = match result {
        Ok((payload, input, output, count, queue_trace)) => consumer
            .complete_round(payload, &cancel)
            .await
            .map(|payload| (payload, input, output, count, queue_trace)),
        Err(error) => Err(error),
    };
    if result.is_err() {
        cancel.cancel();
    }
    let client = consumer.close().await;
    let (payload, input_occupancy, output_occupancy, source_lines, queue_trace) = result?;
    let (trace, client_frames, client_bytes) = client?;
    Ok(Outcome {
        payload,
        trace,
        observations: consumer.observations,
        input_occupancy,
        output_occupancy,
        client_frames,
        client_bytes,
        source_lines,
        gateway: consumer.gateway.take(),
        queue_trace,
    })
}
