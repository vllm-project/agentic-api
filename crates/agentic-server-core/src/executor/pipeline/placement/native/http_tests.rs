// Native loopback HTTP -> the same ingestion -> native relay. Not a gateway test.

#![cfg(test)]

use std::time::Duration;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use super::super::fixtures::{named_workloads, recorded_wire_rounds};
use super::http_server::HttpServer;
use super::transport::recorded_inputs;
use super::{
    ClientProbe, DriverError, EarlyFailureProbe, FinishFence, Ownership, Placement, Settings, Source, run_source,
};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .build()
        .unwrap()
}

pub(super) async fn pair(settings: Settings, server: &HttpServer) -> usize {
    let fence = FinishFence::new();
    let cancel = CancellationToken::new();
    let mut inline = settings.clone();
    inline.placement = Placement::Inline;
    inline.capture = true;
    inline.finish_fence = Some(fence.clone());
    let mut worker = inline.clone();
    worker.placement = Placement::Worker;
    let owner = Ownership::default();
    let check = async {
        let (inline, worker, released) = tokio::join!(
            run_source(
                inline,
                Source::http(server.stream(Duration::from_secs(2))),
                cancel.clone(),
                Ownership::default()
            ),
            run_source(
                worker,
                Source::http(server.stream(Duration::from_secs(2))),
                cancel.clone(),
                owner.clone()
            ),
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
        assert_eq!(inline.client_frames, worker.client_frames);
        assert_eq!(inline.client_bytes, worker.client_bytes);
        assert_eq!(inline.source_lines, worker.source_lines);
        assert!(inline.source_lines > 0);
        assert_eq!(owner.state(), (0, 1, 1, 1, 1));
        inline.source_lines
    };
    Box::pin(tokio::time::timeout(Duration::from_secs(20), check))
        .await
        .unwrap()
}

#[test]
fn http_matrix_keeps_entire_payload_and_native_client_wire_identical() {
    runtime().block_on(async {
        for workload in named_workloads()
            .into_iter()
            .filter(|workload| !workload.identity.extended_gate)
        {
            for spacing_ms in [0, 1] {
                let chunks = Bytes::from(workload.wire.clone())
                    .chunks(4096)
                    .map(Bytes::copy_from_slice)
                    .collect();
                let server = HttpServer::start(chunks, false, Duration::from_millis(spacing_ms))
                    .await
                    .unwrap();
                for entries in [1, 16, 64, 256] {
                    for budget in [65_536, 262_144] {
                        for client_ms in [0, 1] {
                            let mut settings = Settings::new(Placement::Inline, true);
                            settings.limits.input_entries = entries;
                            settings.limits.output_entries = entries;
                            settings.limits.input_bytes = budget;
                            settings.limits.output_bytes = budget;
                            settings.client_delay = Duration::from_millis(client_ms);
                            assert_eq!(
                                pair(settings, &server).await,
                                workload.identity.expected_framed_lines,
                                "{}",
                                workload.identity.id
                            );
                        }
                    }
                }
                assert_eq!(server.requests(), 32);
                server.stop().await.unwrap();
            }
            println!(
                "PLACEMENT_NAMED_HTTP_GATE {}",
                serde_json::json!({"workload": workload.identity, "full_payload_wire_pairs": 32})
            );
        }
    });
}

#[test]
fn four_concurrent_http_pairs_preserve_full_payload_and_native_wire() {
    runtime().block_on(async {
        let server = std::sync::Arc::new(
            HttpServer::start(
                vec![Bytes::from(recorded_wire_rounds().remove(0))],
                false,
                Duration::ZERO,
            )
            .await
            .unwrap(),
        );
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let server = std::sync::Arc::clone(&server);
            tasks.spawn(Box::pin(async move {
                let mut settings = Settings::new(Placement::Inline, true);
                settings.limits.input_entries = 16;
                settings.limits.output_entries = 16;
                settings.limits.input_bytes = 65_536;
                settings.limits.output_bytes = 65_536;
                pair(settings, &server).await;
            }));
        }
        while let Some(joined) = tasks.join_next().await {
            joined.unwrap();
        }
        assert_eq!(server.requests(), 8);
        let Ok(server) = std::sync::Arc::try_unwrap(server) else {
            panic!("all request owners must have joined")
        };
        server.stop().await.unwrap();
    });
}

#[test]
fn native_http_timeout_is_not_hidden_by_incomplete_finalization_or_cleanup() {
    runtime().block_on(async {
        let first = recorded_inputs().await.unwrap().remove(0).remove(0);
        let server = HttpServer::start(vec![Bytes::from(format!("{first}\n\n"))], true, Duration::ZERO)
            .await
            .unwrap();
        let owner = Ownership::default();
        let result = run_source(
            Settings::new(Placement::Worker, true),
            Source::http(server.stream(Duration::from_millis(50))),
            CancellationToken::new(),
            owner.clone(),
        )
        .await;
        server.stop().await.unwrap();
        let Err(DriverError::Transport(error)) = result else {
            panic!("retain the original input transport error")
        };
        assert!(error.to_string().contains("chunk timeout"));
        assert_eq!(owner.state(), (0, 1, 1, 1, 1));
    });
}

#[test]
fn cancellation_after_a_real_http_client_frame_joins_all_owned_tasks() {
    runtime().block_on(async {
        let first = recorded_inputs().await.unwrap().remove(0).remove(0);
        let server = HttpServer::start(vec![Bytes::from(format!("{first}\n\n"))], true, Duration::ZERO)
            .await
            .unwrap();
        let owner = Ownership::default();
        let control = CancellationToken::new();
        let probe = ClientProbe::default();
        let mut settings = Settings::new(Placement::Worker, true);
        settings.capture = true;
        settings.client_probe = Some(probe.clone());
        let mut task = tokio::spawn(run_source(
            settings,
            Source::http(server.stream(Duration::from_secs(2))),
            control.clone(),
            owner.clone(),
        ));
        let observed = tokio::time::timeout(Duration::from_secs(5), async {
            while probe.frames() == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        control.cancel();
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut task)
            .await
            .unwrap()
            .unwrap();
        server.stop().await.unwrap();
        assert!(
            observed.is_ok(),
            "cancel only after observing a real native client frame"
        );
        assert!(matches!(joined, Err(DriverError::Cancelled)));
        assert_eq!(owner.state(), (0, 1, 1, 1, 1));
    });
}

#[test]
fn native_http_client_disconnect_joins_the_worker_feeder_and_client() {
    runtime().block_on(async {
        let server = HttpServer::start(
            vec![Bytes::from(recorded_wire_rounds().remove(0))],
            false,
            Duration::ZERO,
        )
        .await
        .unwrap();
        let owner = Ownership::default();
        let mut settings = Settings::new(Placement::Worker, true);
        settings.disconnect_after = Some(2);
        let result = run_source(
            settings,
            Source::http(server.stream(Duration::from_secs(2))),
            CancellationToken::new(),
            owner.clone(),
        )
        .await;
        server.stop().await.unwrap();
        assert!(result.is_err());
        assert_eq!(owner.state(), (0, 1, 1, 1, 1));
    });
}

#[test]
fn timed_http_settings_reject_correctness_probes_and_double_pacing() {
    runtime().block_on(async {
        for probe in [false, true] {
            let mut settings = Settings::new(Placement::Inline, true);
            if probe {
                settings.client_probe = Some(ClientProbe::default());
            } else {
                settings.arrival_spacing = Duration::from_secs(1);
            }
            let source = Source::http(Box::pin(futures::stream::empty()));
            assert!(matches!(
                run_source(settings, source, CancellationToken::new(), Ownership::default()).await,
                Err(DriverError::InvalidLimits)
            ));
        }
        for capture in [false, true] {
            let mut settings = Settings::new(Placement::Worker, true);
            settings.capture = capture;
            settings.early_failure_probe = Some(EarlyFailureProbe::default());
            let source = Source::http(Box::pin(futures::stream::empty()));
            assert!(matches!(
                run_source(settings, source, CancellationToken::new(), Ownership::default()).await,
                Err(DriverError::InvalidLimits)
            ));
        }
    });
}
