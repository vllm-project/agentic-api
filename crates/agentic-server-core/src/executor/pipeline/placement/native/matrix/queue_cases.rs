// Queue qualification on the same case contract; never merge replica quantiles.

#![cfg(test)]

use super::super::queue::Summary;
use super::super::transport::named_inputs;
use super::super::{Outcome, Placement};
use super::{observations, pilot_runtime, selection, trial};

#[derive(serde::Serialize)]
struct Replica {
    completion_rank_not_input_identity: usize,
    source_lines: usize,
    queues: Summary,
}

fn take_queues(outcomes: &mut [Outcome], enabled: bool) -> Option<Vec<Replica>> {
    assert!(!outcomes.is_empty());
    if !enabled {
        assert!(outcomes.iter().all(|outcome| outcome.queue_trace.is_none()));
        return None;
    }
    Some(
        outcomes
            .iter_mut()
            .enumerate()
            .map(|(rank, outcome)| {
                let trace = outcome
                    .queue_trace
                    .take()
                    .expect("every observed worker must return its own queue trace");
                let queues = trace
                    .summarize()
                    .expect("incomplete queue history is not a measurement");
                assert_eq!(queues.input.intervals.len(), outcome.source_lines);
                assert_eq!(queues.output.intervals.len(), outcome.source_lines);
                Replica {
                    completion_rank_not_input_identity: rank,
                    source_lines: outcome.source_lines,
                    queues,
                }
            })
            .collect(),
    )
}

#[test]
#[ignore = "explicit same-case queue qualification; require native and observer-enabled matching gates first"]
fn measure_queue_cases() {
    let case = std::env::var("PLACEMENT_QUEUE_CASE").expect("explicit full-plan case required");
    let selected = selection::select("case", Some(&case)).expect("invalid queue case");
    let (index, config) = selected[0];
    let enabled: bool = std::env::var("PLACEMENT_QUEUE_OBSERVER")
        .expect("explicit true/false observer selection")
        .parse()
        .unwrap();
    let observers = trial::Observers {
        scheduler: false,
        queues: enabled,
    };
    let runtime = pilot_runtime();
    let workloads = runtime.block_on(named_inputs()).unwrap();
    runtime.block_on(async {
        println!("PLACEMENT_QUEUE_CASE_MANIFEST {}", serde_json::json!({
            "schema": 1, "base": "0b98f4ba", "case_index": index, "config": config,
            "workloads": workloads.iter().map(|workload| &workload.identity).collect::<Vec<_>>(),
            "full_plan_configs": 144, "executed_case_configs": 1,
        }));
        for workload in &workloads {
            let prepare = || (0..config.concurrency).map(|_| workload.lines.clone()).collect();
            for _ in 0..2 { trial::measure(config, Placement::Worker, prepare(), observers).await.unwrap(); }
            for trial_index in 0..3 {
                let mut measured = trial::measure(config, Placement::Worker, prepare(), observers).await.unwrap();
                let state = measured.owner.state();
                assert_eq!((state.0, state.2, state.3, state.4), (0, config.concurrency, config.concurrency, if config.relay { config.concurrency } else { 0 }));
                assert!(measured.scheduler.is_none());
                // Histograms, cleanup, event quantiles and serialization are outside every trial clock.
                let replicas = take_queues(&mut measured.outcomes, enabled);
                let events = observations(measured.outcomes, workload.identity.expected_framed_lines);
                println!("PLACEMENT_QUEUE_CASE_JSON {}", serde_json::json!({
                    "schema": 1, "base": "0b98f4ba", "case_index": index, "config": config, "arm": "worker",
                    "workload": workload.identity, "trial": trial_index, "observer_enabled": enabled,
                    "scope": "worker_native_core_or_relay_after_untimed_http_preparation_not_full_gateway",
                    "arrival_origin": "shared_before_dispatch_for_every_stream_not_per_first_poll",
                    "arrival_shape": "controlled_fixed_or_repeating_gaps_2_0_1_1_mean_1ms_not_recorded_timing",
                    "batch_wall_ns": measured.batch_wall_ns, "window_wall_ns": measured.window_wall_ns,
                    "cpu": measured.cpu, "cpu_scope": "all_process_threads_including_queue_observer_setup_recording_and_native_joins",
                    "owners_joined": state, "events": events, "replicas": replicas,
                    "replica_identity": "completion_rank_not_stable_input_index; identical_workload_replicas",
                    "queue_window": "each_worker_before_tasks_spawned_until_worker_and_feeder_joined_excludes_client_close",
                    "replica_p99_aggregated_into_batch_p99": false,
                    "statistics_and_histograms_after_clocks": true, "observer_cost_subtracted": false,
                    "scheduler_probe_enabled": false, "correctness_capture_or_fence": false,
                    "full_plan_configs": 144, "executed_case_configs": 1, "performance_recommendation": null,
                }));
            }
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observed_concurrent_case_keeps_each_native_queue_and_observes_every_join() {
    let config = super::pilot()[1];
    let lines = super::super::super::fixtures::recorded_rounds().remove(0);
    let mut measured = trial::measure(
        config,
        Placement::Worker,
        vec![lines.clone(); config.concurrency],
        trial::Observers {
            scheduler: false,
            queues: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        (
            measured.owner.state().0,
            measured.owner.state().2,
            measured.owner.state().3,
            measured.owner.state().4
        ),
        (0, 4, 4, 4)
    );
    let replicas = take_queues(&mut measured.outcomes, true).unwrap();
    assert_eq!(replicas.len(), 4);
    for (rank, replica) in replicas.iter().enumerate() {
        assert_eq!(replica.completion_rank_not_input_identity, rank);
        assert_eq!(replica.source_lines, lines.len());
        assert!(replica.queues.input.window_ns <= measured.window_wall_ns);
        assert_eq!(replica.queues.input.entries_capacity, config.queue_entries);
        assert_eq!(replica.queues.input.logical_byte_budget, config.logical_byte_budget);
    }
    assert!(measured.outcomes.iter().all(|outcome| outcome.queue_trace.is_none()));
}
