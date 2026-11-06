// Opt-in shared-origin matrix; same native ingestion, bounded worker and relay.

#![cfg(test)]

mod case_resources;
mod config;
mod queue_cases;
mod selection;
mod trial;
pub(super) use config::{full_plan, pilot};

use super::profile::{percentile, pilot_runtime};
use super::transport::named_inputs;
use super::{Outcome, Placement};

fn observations(outcomes: Vec<Outcome>, expected: usize) -> serde_json::Value {
    let mut ingestion = Vec::new();
    let mut delivery = Vec::new();
    let (mut source_lines, mut frames, mut bytes) = (0usize, 0usize, 0usize);
    for outcome in outcomes {
        assert_eq!(outcome.source_lines, expected);
        assert_eq!(outcome.payload.status, "completed");
        assert!(outcome.trace.is_empty() && outcome.queue_trace.is_none());
        source_lines += outcome.source_lines;
        frames += outcome.client_frames;
        bytes += outcome.client_bytes;
        for sample in outcome.observations {
            ingestion.push(sample.due_to_ingestion_ns);
            delivery.push(sample.due_to_relay_return_ns);
        }
    }
    serde_json::json!({
        "source_lines": source_lines, "client_frames": frames, "client_bytes": bytes,
        "semantic_inputs": ingestion.len(),
        "due_to_ingestion_p50_ns": percentile(&mut ingestion, 50),
        "due_to_ingestion_p99_ns": percentile(&mut ingestion, 99),
        "due_to_ingestion_max_ns": ingestion.iter().max(),
        "due_to_core_or_relay_return_p50_ns": percentile(&mut delivery, 50),
        "due_to_core_or_relay_return_p99_ns": percentile(&mut delivery, 99),
        "due_to_core_or_relay_return_max_ns": delivery.iter().max(),
    })
}

#[test]
#[ignore = "explicit shared-origin matrix; run all native and matching configuration gates first"]
fn measure_shared_origin_matrix() {
    let mode = std::env::var("PLACEMENT_MATRIX_MODE").expect("explicit plan/pilot/full/case");
    let requested = std::env::var_os("PLACEMENT_MATRIX_CASE")
        .map(|value| value.into_string().expect("case selection must be Unicode"));
    let selected = selection::select(&mode, requested.as_deref()).expect("invalid explicit case/matrix selection");
    if mode == "plan" {
        println!(
            "PLACEMENT_MATRIX_PLAN {}",
            serde_json::json!({"full": full_plan(), "pilot": pilot()})
        );
        return;
    }
    let configs: Vec<_> = selected.iter().map(|(_, config)| *config).collect();
    let arm = std::env::var("PLACEMENT_MATRIX_ARM").expect("explicit placement arm");
    let placement = match arm.as_str() {
        "inline" => Placement::Inline,
        "worker" => Placement::Worker,
        _ => panic!("unknown arm"),
    };
    let enabled: bool = std::env::var("PLACEMENT_MATRIX_PROBE")
        .expect("explicit true/false probe selection")
        .parse()
        .unwrap();
    let observers = trial::Observers {
        scheduler: enabled,
        queues: false,
    };
    let runtime = pilot_runtime();
    let workloads = runtime.block_on(named_inputs()).unwrap();
    let resource = (mode == "case").then(|| case_resources::Observer::new(runtime.metrics()));
    let mut active_workers_peak = 0;
    runtime.block_on(async {
        println!("PLACEMENT_MATRIX_MANIFEST {}", serde_json::json!({"mode": mode, "configs": configs, "workloads": workloads.iter().map(|w| &w.identity).collect::<Vec<_>>()}));
        for &(config_index, config) in &selected {
            for workload in &workloads {
                let prepare = || (0..config.concurrency).map(|_| workload.lines.clone()).collect();
                for _ in 0..2 { trial::measure(config, placement, prepare(), observers).await.unwrap(); }
                for trial_index in 0..3 {
                    let measured = trial::measure(config, placement, prepare(), observers).await.unwrap();
                    let state = measured.owner.state();
                    active_workers_peak = active_workers_peak.max(state.1);
                    let workers = if arm == "worker" { config.concurrency } else { 0 };
                    assert_eq!((state.0, state.2, state.3, state.4), (0, workers, workers, if config.relay { config.concurrency } else { 0 }));
                    assert_eq!(measured.scheduler.is_some(), enabled);
                    let row = serde_json::json!({
                        "schema": 1, "base": "0b98f4ba", "mode": mode, "arm": arm,
                        "config_index": config_index, "config": config, "workload": workload.identity,
                        "trial": trial_index, "probe_enabled": enabled,
                        "arrival_origin": "shared_before_dispatch_for_every_stream_not_per_first_poll",
                        "arrival_shape": "controlled_fixed_or_repeating_gaps_2_0_1_1_mean_1ms_not_recorded_timing",
                        "scope": "native_core_or_relay_after_untimed_http_preparation_not_full_gateway",
                        "batch_wall_ns": measured.batch_wall_ns, "window_wall_ns": measured.window_wall_ns,
                        "cpu": measured.cpu, "cpu_scope": "all_process_threads_including_probe_setup_stop_join",
                        "scheduler": measured.scheduler, "owners_joined": state,
                        "events": observations(measured.outcomes, workload.identity.expected_framed_lines),
                        "queue_observer_enabled": false, "correctness_capture_or_fence": false,
                        "statistics_computed_after_clocks": true, "cost_subtracted": false,
                        "full_factor_plan_configurations": 144, "executed_configurations_in_this_process": configs.len(),
                        "performance_recommendation": null,
                    });
                    println!("PLACEMENT_MATRIX_JSON {row}");
                }
            }
        }
    });
    if let Some(resource) = resource {
        assert_eq!(selected.len(), 1);
        println!(
            "PLACEMENT_MATRIX_CASE_RESOURCE {}",
            serde_json::json!({
                "schema": 1, "mode": mode, "base": "0b98f4ba", "arm": arm,
                "config_index": selected[0].0, "config": selected[0].1, "probe_enabled": enabled,
                "named_requests": workloads.len(), "warmups_per_request": 2, "trials_per_request": 3,
                "executed_configurations_in_this_process": 1, "full_factor_plan_configurations": 144,
                "resources": resource.finish(active_workers_peak), "performance_recommendation": null,
            })
        );
    }
}
