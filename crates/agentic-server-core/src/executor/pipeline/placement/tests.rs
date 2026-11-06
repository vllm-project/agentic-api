// Preserve the original #245 contracts on the single typed native driver.

#![cfg(test)]

use std::time::Duration;
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

use super::driver::{DriverError, FinishFence, Placement};
use super::fault_probe::EarlyFailureProbe;
use super::fixtures::{recorded_rounds, rejected_benchmark_rounds};
use super::native::{ClientProbe, Outcome, Ownership, Settings, run};

fn runtime() -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

fn options(placement: Placement) -> Settings {
    let mut settings = Settings::new(placement, false);
    settings.capture = true;
    settings
}

async fn run_arm(
    settings: Settings,
    lines: Vec<String>,
    control: CancellationToken,
    owner: Ownership,
) -> Result<Outcome, DriverError> {
    let result = run(settings, lines, control.clone(), owner).await;
    if result.is_err() {
        control.cancel();
    }
    result
}

#[test]
fn recorded_round_is_valid_under_native_strict_ingestion() {
    runtime().block_on(async {
        for lines in recorded_rounds() {
            run(
                options(Placement::Inline),
                lines,
                CancellationToken::new(),
                Ownership::default(),
            )
            .await
            .expect("unchanged recording must pass native strict ingestion");
        }
    });
}

#[test]
fn capacity_one_preserves_entire_payload_and_translated_wire() {
    runtime().block_on(async {
        let rounds = recorded_rounds();
        assert!(rounds.iter().map(Vec::len).sum::<usize>() > 100);
        assert!(rounds.len() > 1, "keep independent request lifecycles separate");
        for lines in rounds {
            let fence = FinishFence::new();
            let control = CancellationToken::new();
            let mut inline = options(Placement::Inline);
            inline.finish_fence = Some(fence.clone());
            let mut worker = options(Placement::Worker);
            worker.finish_fence = Some(fence.clone());
            let owner = Ownership::default();
            let check = async {
                let (inline, worker, released) = tokio::join!(
                    run_arm(inline, lines.clone(), control.clone(), Ownership::default()),
                    run_arm(worker, lines, control.clone(), owner.clone()),
                    fence.release(&control),
                );
                released.unwrap();
                let inline = inline.unwrap();
                let worker = worker.unwrap();
                assert_eq!(
                    serde_json::to_string(&inline.payload).unwrap(),
                    serde_json::to_string(&worker.payload).unwrap(),
                    "do not normalize payload fields"
                );
                assert_eq!(
                    inline.trace, worker.trace,
                    "every translated frame and deferral boundary"
                );
                assert!(!inline.trace.is_empty());
                assert_eq!(inline.payload.status, "completed");
                let payload = serde_json::to_value(&inline.payload).unwrap();
                assert!(!payload["output"].as_array().unwrap().is_empty());
                assert_eq!(owner.state(), (0, 1, 1, 1, 0));
            };
            Box::pin(tokio::time::timeout(Duration::from_secs(15), check))
                .await
                .unwrap();
        }
    });
}

#[test]
fn original_benchmark_recording_is_rejected_consistently_and_worker_is_joined() {
    runtime().block_on(async {
        for lines in rejected_benchmark_rounds() {
            let mut errors = Vec::new();
            for placement in [Placement::Inline, Placement::Worker] {
                let owner = Ownership::default();
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    run(
                        options(placement),
                        lines.clone(),
                        CancellationToken::new(),
                        owner.clone(),
                    ),
                )
                .await
                .unwrap();
                let Err(DriverError::Ingestion(error)) = result else {
                    panic!("retain strict terminal-output mismatch on the unchanged older recording")
                };
                errors.push(error.to_string());
                let workers = usize::from(matches!(placement, Placement::Worker));
                assert_eq!(owner.state(), (0, workers, workers, workers, 0));
            }
            assert_eq!(errors[0], errors[1]);
            assert!(errors[0].contains("terminal upstream response output does not match completed item events"));
        }
    });
}

#[test]
fn cancellation_mid_stream_joins_the_blocking_worker() {
    runtime().block_on(async {
        let owner = Ownership::default();
        let control = CancellationToken::new();
        let probe = ClientProbe::default();
        let mut settings = options(Placement::Worker);
        settings.relay = true;
        settings.client_probe = Some(probe.clone());
        settings.client_delay = Duration::from_millis(5);
        let mut task = tokio::spawn(run(
            settings,
            recorded_rounds().remove(0),
            control.clone(),
            owner.clone(),
        ));
        let progressed = tokio::time::timeout(Duration::from_secs(5), async {
            while probe.frames() < 8 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        control.cancel();
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            progressed.is_ok(),
            "cancel after at least eight real native client frames"
        );
        assert!(
            matches!(&joined, Err(DriverError::Cancelled)),
            "unexpected cancellation result: {:?}",
            joined.as_ref().err().map(ToString::to_string)
        );
        assert_eq!(owner.state(), (0, 1, 1, 1, 1));
    });
}

#[test]
fn disconnected_consumer_unblocks_and_joins_the_worker() {
    runtime().block_on(async {
        let owner = Ownership::default();
        let mut settings = options(Placement::Worker);
        settings.relay = true;
        settings.disconnect_after = Some(2);
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
        let error = result.err().expect("a disconnected real client must return an error");
        assert!(
            error.to_string().contains("stream receiver closed"),
            "unexpected disconnect error: {error}"
        );
        assert_eq!(owner.state(), (0, 1, 1, 1, 1));
    });
}

#[test]
fn strict_pipeline_error_is_returned_after_worker_join() {
    runtime().block_on(async {
        let owner = Ownership::default();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run(
                options(Placement::Worker),
                vec!["data: {".to_owned()],
                CancellationToken::new(),
                owner.clone(),
            ),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(DriverError::Ingestion(_))));
        assert_eq!(owner.state(), (0, 1, 1, 1, 0));
    });
}

#[test]
fn early_worker_error_cancels_paced_feeder_and_observes_both_joins() {
    runtime().block_on(async {
        let owner = Ownership::default();
        let control = CancellationToken::new();
        let probe = EarlyFailureProbe::default();
        let mut settings = options(Placement::Worker);
        settings.early_failure_probe = Some(probe.clone());
        let mut task = tokio::spawn(run(
            settings,
            vec!["data: {".to_owned(), ": never process the second input".to_owned()],
            control.clone(),
            owner.clone(),
        ));
        assert!(
            tokio::time::timeout(Duration::from_secs(5), probe.wait_until_paused(&control))
                .await
                .unwrap()
        );
        let result = tokio::time::timeout(Duration::from_secs(1), &mut task).await;
        let prompt = result.is_ok();
        let joined = if let Ok(joined) = result {
            joined
        } else {
            control.cancel();
            tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .expect("cleanup must observe the request join")
        }
        .unwrap();
        assert_eq!(owner.state(), (0, 1, 1, 1, 0));
        assert!(prompt, "worker error waited on the paced feeder");
        let Err(DriverError::Ingestion(error)) = joined else {
            panic!("secondary feeder cancellation must not replace native ingestion error")
        };
        assert!(error.to_string().contains("malformed data frame"));
    });
}

#[test]
fn early_failure_probe_cannot_wait_on_a_missing_second_input() {
    runtime().block_on(async {
        let owner = Ownership::default();
        let mut settings = options(Placement::Worker);
        settings.early_failure_probe = Some(EarlyFailureProbe::default());
        let result = run(
            settings,
            vec!["data: {".to_owned()],
            CancellationToken::new(),
            owner.clone(),
        )
        .await;
        assert!(matches!(result, Err(DriverError::InvalidLimits)));
        assert_eq!(owner.state(), (0, 0, 0, 0, 0));
    });
}

#[test]
fn oversized_input_and_output_are_rejected_without_leaking_a_worker() {
    runtime().block_on(async {
        for direction in ["input", "output"] {
            let owner = Ownership::default();
            let mut settings = options(Placement::Worker);
            match direction {
                "input" => settings.limits.input_item_bytes = 1,
                "output" => settings.limits.output_item_bytes = 1,
                _ => unreachable!(),
            }
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
            assert!(matches!(result, Err(DriverError::ItemLimit(actual)) if actual == direction));
            assert_eq!(owner.state(), (0, 1, 1, 1, 0));
        }
    });
}

#[test]
fn zero_queue_capacity_is_rejected_before_spawning() {
    runtime().block_on(async {
        let owner = Ownership::default();
        let mut settings = options(Placement::Worker);
        settings.limits.input_entries = 0;
        let result = run(settings, Vec::new(), CancellationToken::new(), owner.clone()).await;
        assert!(matches!(result, Err(DriverError::InvalidLimits)));
        assert_eq!(owner.state(), (0, 0, 0, 0, 0));
    });
}

#[test]
fn impossible_queue_and_byte_limits_are_rejected_before_spawning() {
    runtime().block_on(async {
        for limits in [
            super::driver::Limits {
                input_entries: usize::MAX,
                ..Default::default()
            },
            super::driver::Limits {
                output_entries: usize::MAX,
                ..Default::default()
            },
            super::driver::Limits {
                output_entries: 0,
                ..Default::default()
            },
            super::driver::Limits {
                input_item_bytes: 0,
                ..Default::default()
            },
            super::driver::Limits {
                output_item_bytes: 0,
                ..Default::default()
            },
            super::driver::Limits {
                input_bytes: 1,
                ..Default::default()
            },
            super::driver::Limits {
                output_bytes: 1,
                ..Default::default()
            },
        ] {
            let owner = Ownership::default();
            let mut settings = options(Placement::Worker);
            settings.limits = limits;
            let result = run(settings, Vec::new(), CancellationToken::new(), owner.clone()).await;
            assert!(matches!(result, Err(DriverError::InvalidLimits)));
            assert_eq!(owner.state(), (0, 0, 0, 0, 0));
        }
    });
}
