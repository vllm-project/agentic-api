// Case-level stable runtime counters; busy wall duration is not thread CPU time.

#![cfg(test)]

use std::time::Instant;

use tokio::runtime::RuntimeMetrics;

use super::super::process::Process;
use super::super::profile::MAX_BLOCKING_THREADS;

#[derive(serde::Serialize)]
pub(super) struct Report {
    pub process_rss_high_water_kib_includes_setup_warmups_and_logging: Option<u64>,
    pub process_threads_observed_after_case_not_peak: Option<u64>,
    pub measured_ingestion_active_workers_peak: usize,
    pub runtime_worker_count: usize,
    pub configured_max_blocking_threads: usize,
    pub runtime_worker_busy_wall_counter_delta_ns: Option<Vec<u128>>,
    pub runtime_counter_window_wall_ns: u128,
    pub runtime_counter_scope: &'static str,
    pub runtime_counter_has_publication_boundary_uncertainty: bool,
    pub busy_wall_duration_is_thread_cpu_time: bool,
    pub observer_cost_subtracted: bool,
}

pub(super) struct Observer {
    metrics: RuntimeMetrics,
    before: Option<Vec<u128>>,
    started: Instant,
}

fn snapshot(metrics: &RuntimeMetrics) -> Option<Vec<u128>> {
    #[cfg(target_has_atomic = "64")]
    {
        let workers = metrics.num_workers();
        if workers == 0 {
            return None;
        }
        Some(
            (0..workers)
                .map(|worker| metrics.worker_total_busy_duration(worker).as_nanos())
                .collect(),
        )
    }
    #[cfg(not(target_has_atomic = "64"))]
    {
        let _ = metrics;
        None
    }
}

fn checked_delta(before: Option<Vec<u128>>, after: Option<Vec<u128>>) -> Option<Vec<u128>> {
    let before = before?;
    let after = after?;
    if before.len() != after.len() || before.is_empty() {
        return None;
    }
    before
        .into_iter()
        .zip(after)
        .map(|(before, after)| after.checked_sub(before))
        .collect()
}

impl Observer {
    /// Called after untimed native framing, outside every request CPU/wall clock.
    pub fn new(metrics: RuntimeMetrics) -> Self {
        let started = Instant::now();
        let before = snapshot(&metrics);
        Self {
            metrics,
            before,
            started,
        }
    }

    /// Counter reads and proc parsing occur after all measured request clocks.
    pub fn finish(self, active_workers_peak: usize) -> Report {
        let after = snapshot(&self.metrics);
        let window = self.started.elapsed().as_nanos();
        let process = Process::read();
        Report {
            process_rss_high_water_kib_includes_setup_warmups_and_logging: process.rss_high_water_kib,
            process_threads_observed_after_case_not_peak: process.threads_observed,
            measured_ingestion_active_workers_peak: active_workers_peak,
            runtime_worker_count: self.metrics.num_workers(),
            configured_max_blocking_threads: MAX_BLOCKING_THREADS,
            runtime_worker_busy_wall_counter_delta_ns: checked_delta(self.before, after),
            runtime_counter_window_wall_ns: window,
            runtime_counter_scope: "batched_tokio_worker_busy_counters_read_around_case_not_request_cpu_or_exact_case_busy_time",
            runtime_counter_has_publication_boundary_uncertainty: true,
            busy_wall_duration_is_thread_cpu_time: false,
            observer_cost_subtracted: false,
        }
    }
}

#[test]
fn missing_changed_or_decreasing_runtime_counters_remain_unknown() {
    assert_eq!(checked_delta(None, Some(vec![1])), None);
    assert_eq!(checked_delta(Some(vec![1]), None), None);
    assert_eq!(checked_delta(Some(vec![1]), Some(vec![1, 2])), None);
    assert_eq!(checked_delta(Some(vec![2]), Some(vec![1])), None);
    assert_eq!(checked_delta(Some(vec![]), Some(vec![])), None);
    assert_eq!(checked_delta(Some(vec![1, 4]), Some(vec![3, 4])), Some(vec![2, 0]));
}

#[test]
fn runtime_resource_report_keeps_busy_wall_and_process_resource_scopes_separate() {
    let runtime = super::super::profile::pilot_runtime();
    let observer = Observer::new(runtime.metrics());
    let report = observer.finish(0);
    assert_eq!(report.runtime_worker_count, 2);
    assert_eq!(report.configured_max_blocking_threads, 8);
    assert!(!report.busy_wall_duration_is_thread_cpu_time && !report.observer_cost_subtracted);
    #[cfg(target_has_atomic = "64")]
    assert_eq!(report.runtime_worker_busy_wall_counter_delta_ns.unwrap().len(), 2);
    assert!(report.runtime_counter_has_publication_boundary_uncertainty);
    #[cfg(target_os = "linux")]
    {
        assert!(
            report
                .process_rss_high_water_kib_includes_setup_warmups_and_logging
                .unwrap()
                > 0
        );
        assert!(report.process_threads_observed_after_case_not_peak.unwrap() >= 3);
    }
}
