// Untimed full-payload/native-wire gate for the bounded pilot matrix.

#![cfg(test)]

use std::time::Duration;

use tokio::runtime::Builder;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::super::fixtures::recorded_rounds;
use super::transport::{named_inputs, recorded_inputs};
use super::{FinishFence, Ownership, Placement, Settings, run};

pub(super) async fn pair(mut settings: Settings, lines: Vec<String>) {
    pair_inputs(&mut settings, lines.clone(), lines).await;
}

async fn pair_inputs(settings: &mut Settings, inline_lines: Vec<String>, worker_lines: Vec<String>) {
    let expected_inline = inline_lines.len();
    let expected_worker = worker_lines.len();
    let fence = FinishFence::new();
    let cancel = CancellationToken::new();
    settings.capture = true;
    settings.finish_fence = Some(fence.clone());
    settings.placement = Placement::Inline;
    let mut worker = settings.clone();
    worker.placement = Placement::Worker;
    let owner = Ownership::default();
    let check = async {
        let (inline, worker, released) = tokio::join!(
            run(settings.clone(), inline_lines, cancel.clone(), Ownership::default()),
            run(worker, worker_lines, cancel.clone(), owner.clone()),
            fence.release(&cancel),
        );
        released.unwrap();
        let inline = inline.unwrap();
        let worker = worker.unwrap();
        assert_eq!(inline.source_lines, expected_inline);
        assert_eq!(worker.source_lines, expected_worker);
        assert_eq!(
            serde_json::to_string(&inline.payload).unwrap(),
            serde_json::to_string(&worker.payload).unwrap(),
        );
        assert_eq!(inline.trace, worker.trace);
        assert_eq!(inline.client_frames, worker.client_frames);
        assert_eq!(inline.client_bytes, worker.client_bytes);
        assert!(inline.queue_trace.is_none());
        if settings.queue_observation {
            let summary = worker.queue_trace.as_ref().unwrap().summarize().unwrap();
            assert_eq!(summary.input.intervals.len(), expected_worker);
            assert_eq!(summary.output.intervals.len(), expected_worker);
            assert_eq!(summary.input.window_ns, summary.output.window_ns);
        } else {
            assert!(worker.queue_trace.is_none());
        }
        assert_eq!(owner.state().0, 0);
        assert_eq!(owner.state().2, 1);
        assert_eq!(owner.state().3, 1);
        assert_eq!(owner.state().4, usize::from(settings.relay));
    };
    Box::pin(tokio::time::timeout(Duration::from_secs(20), check))
        .await
        .unwrap();
}

#[test]
fn pilot_matrix_keeps_full_payload_and_native_wire_identical() {
    Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let rounds = recorded_inputs().await.unwrap();
            assert_eq!(rounds.len(), 4, "all four recorded requests must reach the gate");
            let raw_rounds = recorded_rounds();
            assert_eq!(raw_rounds.len(), rounds.len());
            // Removing transport metadata must not change any payload field or
            // native client wire. Compare old raw inline to new framed worker.
            for (raw, framed) in raw_rounds.into_iter().zip(&rounds) {
                let mut settings = Settings::new(Placement::Inline, true);
                pair_inputs(&mut settings, raw, framed.clone()).await;
            }
            let workloads: Vec<_> = named_inputs()
                .await
                .unwrap()
                .into_iter()
                .filter(|workload| !workload.identity.extended_gate)
                .collect();
            for entries in [1, 16, 64, 256] {
                for budget in [65_536, 262_144] {
                    for (relay, arrival_ms, client_ms) in
                        [(false, 0, 0), (false, 1, 0), (true, 0, 0), (true, 1, 0), (true, 0, 1)]
                    {
                        let mut settings = Settings::new(Placement::Inline, relay);
                        settings.limits.input_entries = entries;
                        settings.limits.output_entries = entries;
                        settings.limits.input_bytes = budget;
                        settings.limits.output_bytes = budget;
                        settings.arrival_spacing = Duration::from_millis(arrival_ms);
                        settings.client_delay = Duration::from_millis(client_ms);
                        for workload in &workloads {
                            pair(settings.clone(), workload.lines.clone()).await;
                        }
                    }
                }
            }
            for workload in &workloads {
                println!(
                    "PLACEMENT_NAMED_ISOLATED_GATE {}",
                    serde_json::json!({"workload": workload.identity, "full_payload_wire_pairs": 40})
                );
            }
            // Also exercise independent simultaneous requests without sharing
            // a timestamp fence between requests or dropping any payload field.
            let mut tasks = JoinSet::new();
            for _ in 0..4 {
                let mut settings = Settings::new(Placement::Inline, true);
                settings.limits.input_entries = 16;
                settings.limits.output_entries = 16;
                settings.limits.input_bytes = 65_536;
                settings.limits.output_bytes = 65_536;
                tasks.spawn(Box::pin(pair(settings, rounds[0].clone())));
            }
            while let Some(joined) = tasks.join_next().await {
                joined.unwrap();
            }
        });
}
