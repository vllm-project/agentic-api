// Shared-origin pilot correctness for every request and requested concurrency.

#![cfg(test)]

use std::time::Duration;

use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::gate::pair;
use super::matrix::{full_plan, pilot};
use super::profile::pilot_runtime;
use super::scheduler_probe::Probe;
use super::transport::named_inputs;
use super::{ArrivalSchedule, DriverError, Ownership, Placement, Settings, Source, run_source};

#[test]
#[ignore = "explicit matching shared-origin pilot gate, required before pilot timing"]
fn shared_origin_pilot_preserves_entire_payload_wire_and_concurrent_ownership() {
    shared_origin_gate(false);
}

#[test]
#[ignore = "explicit queue-observed case configuration gate, required before queue qualification"]
fn queue_observed_cases_preserve_entire_payload_wire_and_concurrent_ownership() {
    shared_origin_gate(true);
}

fn shared_origin_gate(queues: bool) {
    pilot_runtime().block_on(async {
        let mode = std::env::var("PLACEMENT_SHARED_GATE_MODE").unwrap_or_else(|_| "pilot".to_owned());
        let configs = match mode.as_str() {
            "pilot" => pilot(),
            "full" => full_plan(),
            _ => panic!("unknown gate mode"),
        };
        let workloads = named_inputs().await.unwrap();
        let mut pairs = 0;
        for &config in &configs {
            for workload in &workloads {
                let origin = Instant::now();
                let mut settings = config.settings(Placement::Inline, origin);
                settings.queue_observation = queues;
                let probe = Probe::start(origin, true);
                let mut tasks = JoinSet::new();
                for _ in 0..config.concurrency {
                    tasks.spawn(Box::pin(pair(settings.clone(), workload.lines.clone())));
                }
                while let Some(joined) = tasks.join_next().await {
                    joined.unwrap();
                    pairs += 1;
                }
                assert!(probe.finish().await.unwrap().unwrap().joined);
            }
        }
        assert_eq!(
            pairs,
            configs.iter().map(|config| config.concurrency).sum::<usize>() * workloads.len()
        );
        let marker = if queues {
            "PLACEMENT_QUEUE_CONFIG_GATE"
        } else {
            "PLACEMENT_SHARED_ORIGIN_GATE"
        };
        println!(
            "{marker} {}",
            serde_json::json!({
                "mode": mode, "configs": configs, "workloads": workloads.len(), "full_payload_wire_pairs": pairs,
                "shared_origin": true, "probe_stopped_and_joined": true, "full_plan_configs": full_plan().len(),
                "full_factor_gate_completed": mode == "full",
            })
        );
    });
}

#[tokio::test]
async fn explicit_replay_schedule_cannot_stack_with_spacing_or_http_pacing() {
    let mut settings = Settings::new(Placement::Inline, true);
    settings.replay_schedule = Some(ArrivalSchedule::at(Instant::now(), Duration::ZERO));
    settings.arrival_spacing = Duration::from_millis(1);
    assert!(settings.validate(0).is_err());
    settings.arrival_spacing = Duration::ZERO;
    let source = Source::http(Box::pin(futures::stream::empty()));
    assert!(matches!(
        run_source(settings, source, CancellationToken::new(), Ownership::default()).await,
        Err(DriverError::InvalidLimits)
    ));
}
