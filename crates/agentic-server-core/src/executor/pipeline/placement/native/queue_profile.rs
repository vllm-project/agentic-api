// Opt-in observer on/off comparison; histograms are computed after trial clocks.

#![cfg(test)]

use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::process::CpuTime;
use super::profile::pilot_runtime;
use super::transport::named_inputs;
use super::{Ownership, Placement, Settings, run};

#[test]
#[ignore = "explicit observer-enabled queue envelope pilot; run all correctness gates first"]
fn measure_queue_envelope_and_observer_effect() {
    let enabled: bool = std::env::var("PLACEMENT_QUEUE_OBSERVER")
        .expect("explicit observer selection")
        .parse()
        .expect("true/false observer selection");
    pilot_runtime().block_on(async {
        let workloads = named_inputs().await.unwrap();
        for entries in [1, 16] {
            for workload in &workloads {
                let mut settings = Settings::new(Placement::Worker, true);
                settings.queue_observation = enabled;
                settings.limits.input_entries = entries;
                settings.limits.output_entries = entries;
                settings.limits.input_bytes = 65_536;
                settings.limits.output_bytes = 65_536;
                for _ in 0..2 {
                    run(settings.clone(), workload.lines.clone(), CancellationToken::new(), Ownership::default()).await.unwrap();
                }
                for trial in 0..3 {
                    let lines = workload.lines.clone();
                    let cancel = CancellationToken::new();
                    let owner = Ownership::default();
                    let cpu_before = CpuTime::read();
                    let started = Instant::now();
                    let outcome = tokio::time::timeout(Duration::from_secs(20), run(settings.clone(), lines, cancel, owner.clone())).await.unwrap().unwrap();
                    let wall_ns = started.elapsed().as_nanos();
                    let cpu = CpuTime::delta(cpu_before, CpuTime::read());
                    assert_eq!(outcome.source_lines, workload.identity.expected_framed_lines);
                    assert_eq!(outcome.payload.status, "completed");
                    assert!(outcome.trace.is_empty());
                    assert_eq!(owner.state(), (0, 1, 1, 1, 1));
                    let queues = outcome.queue_trace.as_ref().map(|trace| trace.summarize().unwrap());
                    assert_eq!(queues.is_some(), enabled);
                    println!("PLACEMENT_QUEUE_JSON {}", serde_json::json!({
                        "schema": 1, "base": "0b98f4ba", "workload": workload.identity,
                        "scope": "worker_core_and_relay_after_untimed_native_http_preparation_not_full_gateway",
                        "observer_enabled": enabled, "histograms_computed_after_clocks": true,
                        "queue_entries": entries, "logical_byte_budget": 65536,
                        "concurrency": 1, "arrival_ms": 0, "client_ms": 0, "trial": trial,
                        "wall_ns": wall_ns, "process_cpu_time": cpu,
                        "cpu_scope": "all_process_threads", "observer_cost_subtracted": false,
                        "source_lines": outcome.source_lines, "client_frames": outcome.client_frames,
                        "client_bytes": outcome.client_bytes, "owners_joined": owner.state(),
                        "queue_window": "observer_initialized_before_tasks_spawned_until_worker_and_feeder_joined_excludes_client_close",
                        "queues": queues, "performance_recommendation": null,
                    }));
                }
            }
        }
    });
}
