// Native-path equivalence and lifecycle guards, not performance recommendations.

#![cfg(test)]

use std::time::Duration;

use tokio::runtime::Builder;
use tokio_util::sync::CancellationToken;

use super::super::fixtures::recorded_rounds;
use super::{DriverError, FinishFence, Ownership, Placement, Settings, run};

fn runtime() -> tokio::runtime::Runtime {
    Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn typed_path_keeps_entire_payload_and_native_relay_wire_identical() {
    runtime().block_on(async {
        for relay in [false, true] {
            for lines in recorded_rounds() {
                let fence = FinishFence::new();
                let cancel = CancellationToken::new();
                let mut inline = Settings::new(Placement::Inline, relay);
                inline.capture = true;
                inline.finish_fence = Some(fence.clone());
                let mut worker = inline.clone();
                worker.placement = Placement::Worker;
                let owner = Ownership::default();
                let check = async {
                    let (inline, worker, released) = tokio::join!(
                        run(inline, lines.clone(), cancel.clone(), Ownership::default()),
                        run(worker, lines, cancel.clone(), owner.clone()),
                        fence.release(&cancel),
                    );
                    released.unwrap();
                    let inline = inline.unwrap();
                    let worker = worker.unwrap();
                    assert_eq!(
                        serde_json::to_string(&inline.payload).unwrap(),
                        serde_json::to_string(&worker.payload).unwrap()
                    );
                    assert_eq!(inline.trace, worker.trace);
                    assert!(!inline.trace.is_empty());
                    assert!(inline.observations.is_empty() && worker.observations.is_empty());
                    assert_eq!(owner.state().0, 0);
                    assert_eq!(owner.state().2, 1);
                    assert_eq!(owner.state().3, 1);
                    assert_eq!(owner.state().4, usize::from(relay));
                };
                Box::pin(tokio::time::timeout(Duration::from_secs(20), check))
                    .await
                    .unwrap();
            }
        }
    });
}

#[test]
fn native_relay_cancellation_wakes_a_paced_feeder_and_joins_every_owner() {
    runtime().block_on(async {
        let mut settings = Settings::new(Placement::Worker, true);
        settings.arrival_spacing = Duration::from_secs(3600);
        let control = CancellationToken::new();
        let owner = Ownership::default();
        let mut task = tokio::spawn(run(
            settings,
            recorded_rounds().remove(0),
            control.clone(),
            owner.clone(),
        ));
        let started = tokio::time::timeout(Duration::from_secs(5), async {
            while owner.state().1 == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        control.cancel();
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut task)
            .await
            .unwrap()
            .unwrap();
        assert!(started.is_ok(), "the worker must have started before cancellation");
        assert!(matches!(joined, Err(DriverError::Cancelled)));
        assert_eq!(owner.state(), (0, 1, 1, 1, 1));
    });
}

#[test]
fn native_ingestion_error_returns_without_waiting_for_a_future_arrival() {
    runtime().block_on(async {
        let mut settings = Settings::new(Placement::Worker, true);
        settings.arrival_spacing = Duration::from_secs(3600);
        let control = CancellationToken::new();
        let owner = Ownership::default();
        let mut task = tokio::spawn(run(
            settings,
            vec!["data: {".to_owned(), ": never wait an hour".to_owned()],
            control.clone(),
            owner.clone(),
        ));
        let result = tokio::time::timeout(Duration::from_secs(1), &mut task).await;
        let prompt = result.is_ok();
        let joined = if let Ok(joined) = result {
            joined
        } else {
            control.cancel();
            tokio::time::timeout(Duration::from_secs(5), &mut task).await.unwrap()
        }
        .unwrap();
        assert_eq!(owner.state(), (0, 1, 1, 1, 1));
        assert!(prompt, "the primary ingestion error must wake the paced feeder");
        let Err(DriverError::Ingestion(error)) = joined else {
            panic!("client/feeder cancellation must not replace the native ingestion error");
        };
        assert!(error.to_string().contains("malformed data frame"));
    });
}

#[test]
fn timing_mode_rejects_a_correctness_timestamp_fence() {
    runtime().block_on(async {
        let mut settings = Settings::new(Placement::Worker, true);
        settings.finish_fence = Some(FinishFence::new());
        let owner = Ownership::default();
        assert!(matches!(
            run(settings, Vec::new(), CancellationToken::new(), owner.clone()).await,
            Err(DriverError::InvalidLimits)
        ));
        assert_eq!(owner.state(), (0, 0, 0, 0, 0));
    });
}

#[test]
fn native_relay_disconnect_observes_worker_feeder_and_client_joins() {
    runtime().block_on(async {
        let mut settings = Settings::new(Placement::Worker, true);
        settings.disconnect_after = Some(2);
        let owner = Ownership::default();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run(
                settings,
                recorded_rounds().remove(0),
                CancellationToken::new(),
                owner.clone(),
            ),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert_eq!(owner.state().0, 0);
        assert_eq!(owner.state().2, 1);
        assert_eq!(owner.state().3, 1);
        assert_eq!(owner.state().4, 1);
    });
}

#[test]
fn native_timing_retains_no_correctness_transcript_and_counts_actual_client_frames() {
    runtime().block_on(async {
        for relay in [false, true] {
            let owner = Ownership::default();
            let result = run(
                Settings::new(Placement::Worker, relay),
                recorded_rounds().remove(0),
                CancellationToken::new(),
                owner,
            )
            .await
            .unwrap();
            assert!(result.trace.is_empty());
            assert!(!result.observations.is_empty());
            assert_eq!(result.client_frames > 0, relay);
            assert_eq!(result.client_bytes > 0, relay);
            assert!(result.input_occupancy.iter().all(|size| *size <= 1));
            assert!(result.output_occupancy.iter().all(|size| *size <= 1));
        }
    });
}
