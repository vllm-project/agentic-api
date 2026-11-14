// Optional periodic task wake-lateness probe, not executor CPU occupancy.

#![cfg(test)]

use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::profile::percentile;
use super::{DriverError, Result};

const PERIOD: Duration = Duration::from_millis(1);
const MAX_SAMPLES: usize = 20_000;

#[derive(serde::Serialize)]
pub(super) struct Sample {
    planned_offset_ns: u128,
    observed_offset_ns: u128,
    lateness_ns: u128,
    skipped_deadlines: u32,
}

#[derive(serde::Serialize)]
pub(super) struct Report {
    pub samples: Vec<Sample>,
    pub storage_exhausted: bool,
    pub period_ns: u128,
    pub wake_lateness_p99_ns: Option<u128>,
    pub wake_lateness_max_ns: Option<u128>,
    pub skipped_deadlines: u64,
    pub joined: bool,
    pub scope: &'static str,
}

impl Report {
    /// Called after trial clocks; missing samples stay unknown, never zero.
    pub fn summarize(&mut self) {
        let mut delays: Vec<_> = self.samples.iter().map(|sample| sample.lateness_ns).collect();
        self.wake_lateness_p99_ns = percentile(&mut delays, 99);
        self.wake_lateness_max_ns = delays.iter().max().copied();
        self.skipped_deadlines = self
            .samples
            .iter()
            .map(|sample| u64::from(sample.skipped_deadlines))
            .sum();
    }
}

struct Active {
    cancel: CancellationToken,
    task: JoinHandle<Report>,
}
pub(super) struct Probe(Option<Active>);

impl Probe {
    pub fn start(origin: Instant, enabled: bool) -> Self {
        Self(enabled.then(|| {
            let cancel = CancellationToken::new();
            let stopping = cancel.clone();
            Active {
                cancel,
                task: tokio::spawn(observe(origin, stopping)),
            }
        }))
    }
    pub async fn finish(mut self) -> Result<Option<Report>> {
        let Some(active) = self.0.take() else { return Ok(None) };
        active.cancel.cancel();
        let mut report = active.task.await.map_err(|_| DriverError::Join)?;
        report.joined = true;
        Ok(Some(report))
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(active) = &self.0 {
            active.cancel.cancel();
        }
    }
}

async fn observe(origin: Instant, cancel: CancellationToken) -> Report {
    let mut samples = Vec::new();
    let mut index = 1u32;
    while !cancel.is_cancelled() && samples.len() < MAX_SAMPLES {
        let deadline = origin + PERIOD * index;
        tokio::select! {
            () = cancel.cancelled() => break,
            () = tokio::time::sleep_until(deadline) => {},
        }
        let observed = Instant::now();
        let lateness = observed.saturating_duration_since(deadline).as_nanos();
        let skipped = u32::try_from(lateness / PERIOD.as_nanos()).unwrap_or(u32::MAX - 1);
        samples.push(Sample {
            planned_offset_ns: (PERIOD * index).as_nanos(),
            observed_offset_ns: observed.saturating_duration_since(origin).as_nanos(),
            lateness_ns: lateness,
            skipped_deadlines: skipped,
        });
        let Some(next) = index.checked_add(skipped).and_then(|index| index.checked_add(1)) else {
            break;
        };
        index = next;
    }
    Report {
        storage_exhausted: samples.len() == MAX_SAMPLES,
        period_ns: PERIOD.as_nanos(),
        wake_lateness_p99_ns: None,
        wake_lateness_max_ns: None,
        skipped_deadlines: 0,
        samples,
        joined: false,
        scope: "tokio_periodic_task_wake_lateness_includes_os_timer_and_scheduler_not_executor_cpu_occupancy",
    }
}

#[tokio::test]
async fn disabled_and_immediately_cancelled_probes_do_not_invent_zero_latency() {
    assert!(Probe::start(Instant::now(), false).finish().await.unwrap().is_none());
    let report = Probe::start(Instant::now(), true).finish().await.unwrap().unwrap();
    assert!(report.joined && report.samples.is_empty());
    assert_eq!(report.wake_lateness_p99_ns, None);
}

#[tokio::test]
async fn deliberately_blocked_runtime_produces_lateness_without_burst_catchup() {
    let probe = Probe::start(Instant::now(), true);
    tokio::task::yield_now().await;
    std::thread::sleep(Duration::from_millis(15));
    tokio::time::sleep(Duration::from_millis(3)).await;
    let mut report = probe.finish().await.unwrap().unwrap();
    report.summarize();
    assert!(report.joined && report.wake_lateness_max_ns.unwrap() >= 5_000_000);
    assert!(report.skipped_deadlines >= 5);
    for samples in report.samples.windows(2) {
        assert_eq!(
            samples[1].planned_offset_ns,
            samples[0].planned_offset_ns + (u128::from(samples[0].skipped_deadlines) + 1) * report.period_ns
        );
    }
}
