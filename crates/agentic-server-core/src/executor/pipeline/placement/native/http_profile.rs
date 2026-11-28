// Opt-in loopback HTTP -> native ingestion -> native relay CPU pilot.

#![cfg(test)]

use std::time::Duration;

use bytes::Bytes;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::super::fixtures::named_workloads;
use super::http_server::HttpServer;
use super::process::{CpuObservation, CpuTime, Process, clock_ticks_per_second, cpu_seconds};
use super::profile::{choice, percentile, pilot_runtime};
use super::{DriverError, Outcome, Ownership, Placement, Result, Settings, Source, run_source};

fn prepare(server: &HttpServer, concurrency: usize) -> Vec<Source> {
    (0..concurrency)
        .map(|_| Source::http(server.stream(Duration::from_secs(2))))
        .collect()
}

async fn batch(
    settings: &Settings,
    sources: Vec<Source>,
    cancel: CancellationToken,
    owner: Ownership,
) -> Result<Vec<Outcome>> {
    let mut tasks = JoinSet::new();
    for source in sources {
        tasks.spawn(run_source(settings.clone(), source, cancel.clone(), owner.clone()));
    }
    let mut outcomes = Vec::new();
    let mut error = None;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(outcome)) => outcomes.push(outcome),
            Ok(Err(cause)) => {
                cancel.cancel();
                error.get_or_insert(cause);
            }
            Err(_) => {
                cancel.cancel();
                error.get_or_insert(DriverError::Join);
            }
        }
    }
    if let Some(error) = error {
        return Err(error);
    }
    Ok(outcomes)
}

async fn batch_with_deadline(
    settings: &Settings,
    sources: Vec<Source>,
    owner: Ownership,
    budget: Duration,
) -> Result<Vec<Outcome>> {
    let cancel = CancellationToken::new();
    let pending = batch(settings, sources, cancel.clone(), owner);
    tokio::pin!(pending);
    tokio::select! {
        result = &mut pending => result,
        () = tokio::time::sleep(budget) => {
            cancel.cancel();
            // Keep the batch/JoinSet alive until every native owner has joined.
            let result = pending.await;
            result.and(Err(DriverError::Cancelled))
        }
    }
}

async fn warmup(settings: &Settings, server: &HttpServer, concurrency: usize) -> Result<()> {
    for _ in 0..2 {
        batch_with_deadline(
            settings,
            prepare(server, concurrency),
            Ownership::default(),
            Duration::from_secs(20),
        )
        .await?;
    }
    Ok(())
}

#[test]
#[ignore = "explicit native loopback HTTP CPU pilot; execute untimed HTTP contracts first"]
fn measure_native_http_pipeline() {
    let arm = std::env::var("PLACEMENT_HTTP_ARM").expect("explicit placement required");
    let placement = match arm.as_str() {
        "inline" => Placement::Inline,
        "worker" => Placement::Worker,
        _ => panic!("unknown placement"),
    };
    let entries = choice("PLACEMENT_HTTP_QUEUE", &[1, 16, 64, 256], 1);
    let budget = choice("PLACEMENT_HTTP_BYTES", &[65_536, 262_144], 65_536);
    let concurrent = choice("PLACEMENT_HTTP_STREAMS", &[1, 4], 1);
    let spacing = choice("PLACEMENT_HTTP_BODY_MS", &[0, 1], 0);
    let client_ms = choice("PLACEMENT_HTTP_CLIENT_MS", &[0, 1], 0);
    let mut settings = Settings::new(placement, true);
    settings.limits.input_entries = entries as usize;
    settings.limits.output_entries = entries as usize;
    settings.limits.input_bytes = budget;
    settings.limits.output_bytes = budget;
    settings.client_delay = Duration::from_millis(u64::from(client_ms));
    settings.validate(0).unwrap();
    let hz = clock_ticks_per_second();
    let runtime = pilot_runtime();
    runtime.block_on(async {
        for (turn, workload) in named_workloads().into_iter().enumerate() {
            let chunks = Bytes::from(workload.wire).chunks(4096).map(Bytes::copy_from_slice).collect();
            let server = HttpServer::start(chunks, false, Duration::from_millis(u64::from(spacing))).await.unwrap();
            warmup(&settings, &server, concurrent as usize).await.unwrap();
            for trial in 0..3 {
                let sources = prepare(&server, concurrent as usize);
                let requests_before = server.requests();
                let before = Process::read();
                let cpu_before = CpuTime::read();
                let started = Instant::now();
                let owner = Ownership::default();
                let outcomes = batch_with_deadline(&settings, sources, owner.clone(), Duration::from_secs(20))
                    .await.unwrap();
                let elapsed = started.elapsed();
                let cpu = CpuTime::delta(cpu_before, CpuTime::read());
                let after = Process::read();
                let requests = server.requests() - requests_before;
                assert_eq!(requests, concurrent as usize, "every measured request must actually poll the native HTTP transport");
                let mut ingestion = Vec::new();
                let mut delivery = Vec::new();
                let mut input = Vec::new();
                let mut output = Vec::new();
                let mut lines = 0u32;
                let mut frames = 0usize;
                let mut bytes = 0usize;
                for outcome in outcomes {
                    assert!(outcome.trace.is_empty());
                    assert_eq!(outcome.payload.status, "completed");
                    assert_eq!(outcome.source_lines, outcome.observations.len());
                    assert_eq!(outcome.source_lines, workload.identity.expected_framed_lines, "{}", workload.identity.id);
                    lines += u32::try_from(outcome.source_lines).unwrap();
                    frames += outcome.client_frames;
                    bytes += outcome.client_bytes;
                    for sample in outcome.observations {
                        ingestion.push(sample.due_to_ingestion_ns);
                        delivery.push(sample.due_to_relay_return_ns);
                    }
                    input.extend(outcome.input_occupancy);
                    output.extend(outcome.output_occupancy);
                }
                let state = owner.state();
                assert_eq!(state.0, 0);
                let expected_workers = if arm == "worker" { concurrent as usize } else { 0 };
                assert_eq!(state.2, expected_workers);
                assert_eq!(state.3, expected_workers);
                assert_eq!(state.4, concurrent as usize);
                let row = serde_json::json!({
                    "schema": 4, "base": "0b98f4ba", "scope": "native_loopback_http_to_core_to_relay_not_full_gateway",
                    "workload": workload.identity,
                    "arm": arm, "turn": turn, "trial": trial, "queue_entries": entries, "logical_byte_budget": budget,
                    "concurrency": concurrent, "body_chunk_bytes": 4096, "configured_body_chunk_spacing_ms": spacing,
                    "configured_client_frame_delay_ms": client_ms, "native_http_requests_in_window": requests,
                    "latency_origin": "native_framed_line_observed_ready_after_poll_not_socket_arrival",
                    "wall_seconds": elapsed.as_secs_f64(), "framed_lines": lines,
                    "framed_lines_per_second": f64::from(lines) / elapsed.as_secs_f64(),
                    "ready_to_ingestion_p50_ns": percentile(&mut ingestion, 50),
                    "ready_to_ingestion_p99_ns": percentile(&mut ingestion, 99),
                    "ready_to_ingestion_max_ns": ingestion.iter().max(),
                    "ready_to_relay_return_p50_ns": percentile(&mut delivery, 50),
                    "ready_to_relay_return_p99_ns": percentile(&mut delivery, 99),
                    "ready_to_relay_return_max_ns": delivery.iter().max(),
                    "input_reserved_entries_p99_at_admission": percentile(&mut input, 99),
                    "output_reserved_entries_p99_at_admission": percentile(&mut output, 99),
                    "cpu": CpuObservation::new(cpu, "all_process_threads_includes_local_mock_backend"),
                    "user_cpu_seconds_includes_local_mock_backend": cpu_seconds(before.user_ticks, after.user_ticks, hz),
                    "system_cpu_seconds_includes_local_mock_backend": cpu_seconds(before.system_ticks, after.system_ticks, hz),
                    "cpu_tick_hz": hz, "process_rss_high_water_kib_includes_setup_and_warmups": after.rss_high_water_kib,
                    "process_threads_observed_after_trial_not_peak": after.threads_observed,
                    "active_workers_peak": state.1, "workers_joined": state.2, "feeders_joined": state.3, "clients_joined": state.4,
                    "actual_client_frames": frames, "actual_client_bytes": bytes,
                    "http_in_timing_window": true, "capture_enabled": false, "correctness_fence_or_probe_enabled": false
                });
                println!("PLACEMENT_HTTP_JSON {}", serde_json::to_string(&row).unwrap());
            }
            let requests = server.requests();
            server.stop().await.unwrap();
            println!("PLACEMENT_HTTP_SERVER_JOINED {}", serde_json::json!({"turn": turn, "workload": workload.identity, "requests": requests}));
        }
    });
}

#[test]
fn http_batch_deadline_observes_joined_owners_before_return() {
    pilot_runtime().block_on(async {
        for placement in [Placement::Inline, Placement::Worker] {
            // Hand-authored transport control, not a model-response cassette.
            let server = HttpServer::start(vec![Bytes::from_static(b": awaiting data\n\n")], true, Duration::ZERO)
                .await
                .unwrap();
            let owner = Ownership::default();
            let settings = Settings::new(placement, true);
            let pending = batch_with_deadline(
                &settings,
                prepare(&server, 4),
                owner.clone(),
                Duration::from_millis(100),
            );
            // A watchdog may fail this regression; it cannot certify teardown.
            let result = tokio::time::timeout(Duration::from_secs(5), pending).await;
            let requests = server.requests();
            server.stop().await.unwrap();
            assert!(requests > 0, "exercise real native HTTP before the batch deadline");
            assert!(matches!(result, Ok(Err(DriverError::Cancelled))));
            let workers = if matches!(placement, Placement::Worker) { 4 } else { 0 };
            let state = owner.state();
            assert_eq!((state.0, state.2, state.3, state.4), (0, workers, workers, 4));
        }
    });
}
