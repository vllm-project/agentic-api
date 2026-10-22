// Timed common-origin batch; cooperative cancellation and probe join precede return.

#![cfg(test)]

use std::time::{Duration, Instant as WallInstant};

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::super::batch;
use super::super::process::CpuTime;
use super::super::scheduler_probe::{Probe, Report};
use super::super::{Outcome, Ownership, Placement, Result};
use super::config::Config;

pub(super) struct Trial {
    pub outcomes: Vec<Outcome>,
    pub owner: Ownership,
    pub scheduler: Option<Report>,
    pub cpu: Option<CpuTime>,
    pub batch_wall_ns: u128,
    pub window_wall_ns: u128,
}

#[derive(Clone, Copy)]
pub(super) struct Observers {
    pub scheduler: bool,
    pub queues: bool,
}

pub(super) async fn measure(
    config: Config,
    placement: Placement,
    prepared: Vec<Vec<String>>,
    observers: Observers,
) -> Result<Trial> {
    let owner = Ownership::default();
    let cancel = CancellationToken::new();
    let cpu_before = CpuTime::read();
    let started = WallInstant::now();
    let origin = Instant::now();
    let mut settings = config.settings(placement, origin);
    settings.queue_observation = observers.queues;
    let probe = Probe::start(origin, observers.scheduler);
    let result = batch::with_deadline(&settings, prepared, cancel, owner.clone(), Duration::from_secs(20)).await;
    let batch_wall_ns = started.elapsed().as_nanos();
    // Always stop and observe the probe, even when the native batch failed.
    let stopped = probe.finish().await;
    let window_wall_ns = started.elapsed().as_nanos();
    let cpu = CpuTime::delta(cpu_before, CpuTime::read());
    let outcomes = result?;
    let mut scheduler = stopped?;
    if let Some(report) = &mut scheduler {
        report.summarize();
    }
    Ok(Trial {
        outcomes,
        owner,
        scheduler,
        cpu,
        batch_wall_ns,
        window_wall_ns,
    })
}
