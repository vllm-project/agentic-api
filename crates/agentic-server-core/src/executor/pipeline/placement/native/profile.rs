// Opt-in CPU pilot. Correctness capture and gates run in a separate process.

#![cfg(test)]

use std::time::Duration;

use tokio::runtime::Builder;
use tokio::time::Instant;

use super::batch::run as batch;
use super::process::{CpuObservation, CpuTime, Process, clock_ticks_per_second, cpu_seconds};
use super::transport::{FramedWorkload, named_inputs};
use super::{Placement, Settings};

pub(super) const RUNTIME_WORKERS: usize = 2;
pub(super) const MAX_BLOCKING_THREADS: usize = 8;

pub(super) fn choice(name: &str, allowed: &[u32], default: u32) -> u32 {
    let value = std::env::var(name)
        .ok()
        .map_or(default, |value| value.parse().expect("numeric pilot option"));
    assert!(
        allowed.contains(&value),
        "pilot option outside its bounded matrix: {name}"
    );
    value
}

pub(super) fn percentile<T: Ord + Copy>(values: &mut [T], percent: usize) -> Option<T> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[(values.len() * percent).div_ceil(100).saturating_sub(1)])
}

pub(super) fn pilot_runtime() -> tokio::runtime::Runtime {
    Builder::new_multi_thread()
        .worker_threads(RUNTIME_WORKERS)
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .enable_all()
        .build()
        .unwrap()
}

fn prepare(lines: &[String], concurrency: usize) -> Vec<Vec<String>> {
    (0..concurrency).map(|_| lines.to_vec()).collect()
}

#[test]
#[ignore = "explicit private CPU pilot; run correctness gate first, then select bounded PLACEMENT_NATIVE_* options"]
fn measure_native_pipeline() {
    let arm = std::env::var("PLACEMENT_NATIVE_ARM").expect("explicit placement required");
    let placement = match arm.as_str() {
        "inline" => Placement::Inline,
        "worker" => Placement::Worker,
        _ => panic!("unknown placement"),
    };
    let mode = std::env::var("PLACEMENT_NATIVE_MODE").expect("explicit core/relay mode required");
    let relay = match mode.as_str() {
        "core" => false,
        "relay" => true,
        _ => panic!("unknown native mode"),
    };
    let entries = choice("PLACEMENT_NATIVE_QUEUE", &[1, 16, 64, 256], 1);
    let budget = choice("PLACEMENT_NATIVE_BYTES", &[65_536, 262_144], 262_144);
    let concurrent = choice("PLACEMENT_NATIVE_STREAMS", &[1, 4], 1);
    let spacing_ms = choice("PLACEMENT_NATIVE_ARRIVAL_MS", &[0, 1], 0);
    let client_ms = choice("PLACEMENT_NATIVE_CLIENT_MS", &[0, 1], 0);
    let mut settings = Settings::new(placement, relay);
    settings.limits.input_entries = entries as usize;
    settings.limits.output_entries = entries as usize;
    settings.limits.input_bytes = budget;
    settings.limits.output_bytes = budget;
    settings.arrival_spacing = Duration::from_millis(u64::from(spacing_ms));
    settings.client_delay = Duration::from_millis(u64::from(client_ms));
    settings.validate(0).unwrap();
    let hz = clock_ticks_per_second();
    let runtime = pilot_runtime();
    runtime.block_on(async {
        // Production HTTP/framing preparation is outside every timed block.
        let workloads = named_inputs().await.unwrap();
        for (round_index, FramedWorkload { identity, lines }) in workloads.iter().enumerate() {
            for _ in 0..2 { batch(&settings, prepare(lines, concurrent as usize)).await.unwrap(); }
            for trial in 0..3 {
                // Fixture cloning happens before both CPU and wall clocks.
                let prepared = prepare(lines, concurrent as usize);
                let before = Process::read();
                let cpu_before = CpuTime::read();
                let started = Instant::now();
                let (outcomes, owner) = batch(&settings, prepared).await.unwrap();
                let elapsed = started.elapsed();
                let cpu = CpuTime::delta(cpu_before, CpuTime::read());
                let after = Process::read();
                let mut ingestion = Vec::new();
                let mut delivery = Vec::new();
                let mut input = Vec::new();
                let mut output = Vec::new();
                let mut frames = 0usize;
                let mut bytes = 0usize;
                for outcome in outcomes {
                    assert_eq!(outcome.source_lines, lines.len());
                    assert!(outcome.trace.is_empty());
                    assert_eq!(outcome.payload.status, "completed");
                    for observation in outcome.observations {
                        ingestion.push(observation.due_to_ingestion_ns);
                        delivery.push(observation.due_to_relay_return_ns);
                    }
                    input.extend(outcome.input_occupancy);
                    output.extend(outcome.output_occupancy);
                    frames += outcome.client_frames;
                    bytes += outcome.client_bytes;
                }
                let state = owner.state();
                assert_eq!(state.0, 0);
                let expected_workers = if arm == "worker" { concurrent as usize } else { 0 };
                assert_eq!(state.2, expected_workers);
                assert_eq!(state.3, expected_workers);
                assert_eq!(state.4, if relay { concurrent as usize } else { 0 });
                let framed_lines = u32::try_from(lines.len()).unwrap() * concurrent;
                let semantic_inputs = u32::try_from(ingestion.len()).unwrap();
                let row = serde_json::json!({
                    "schema": 6, "base": "0b98f4ba", "scope": "core_or_relay_after_native_http_preparation_not_http_timed",
                    "workload": identity,
                    "input_shape": "native_inference_response_lines",
                    "arm": arm, "mode": mode, "round": round_index, "trial": trial,
                    "queue_entries": entries, "logical_byte_budget": budget, "concurrency": concurrent,
                    "controlled_arrival": {"spacing_ms": spacing_ms, "origin": "shared_before_batch_dispatch_not_per_stream_first_poll"},
                    "configured_client_frame_delay_ms": client_ms,
                    "wall_seconds": elapsed.as_secs_f64(), "framed_lines": framed_lines, "semantic_inputs": semantic_inputs,
                    "framed_lines_per_second": f64::from(framed_lines) / elapsed.as_secs_f64(),
                    "semantic_inputs_per_second": f64::from(semantic_inputs) / elapsed.as_secs_f64(),
                    "due_to_ingestion_p50_ns": percentile(&mut ingestion, 50),
                    "due_to_ingestion_p99_ns": percentile(&mut ingestion, 99),
                    "due_to_ingestion_max_ns": ingestion.iter().max(),
                    "due_to_relay_return_p50_ns": percentile(&mut delivery, 50),
                    "due_to_relay_return_p99_ns": percentile(&mut delivery, 99),
                    "due_to_relay_return_max_ns": delivery.iter().max(),
                    "input_reserved_entries_p99_at_admission": percentile(&mut input, 99),
                    "output_reserved_entries_p99_at_admission": percentile(&mut output, 99),
                    "cpu": CpuObservation::new(cpu, "all_process_threads_excludes_untimed_http_preparation"),
                    "user_cpu_seconds": cpu_seconds(before.user_ticks, after.user_ticks, hz),
                    "system_cpu_seconds": cpu_seconds(before.system_ticks, after.system_ticks, hz),
                    "cpu_tick_hz": hz, "process_rss_high_water_kib_includes_setup_and_warmups": after.rss_high_water_kib,
                    "process_threads_observed_after_trial_not_peak": after.threads_observed,
                    "active_workers_peak": state.1, "workers_joined": state.2, "feeders_joined": state.3, "clients_joined": state.4,
                    "actual_client_frames": frames, "actual_client_bytes": bytes,
                    "capture_enabled": false, "correctness_timestamp_fence_enabled": false
                });
                println!("PLACEMENT_NATIVE_JSON {}", serde_json::to_string(&row).unwrap());
            }
        }
    });
}

#[test]
fn nearest_rank_percentiles_preserve_empty_as_unknown() {
    assert_eq!(percentile::<u32>(&mut [], 99), None);
    assert_eq!(percentile(&mut [3, 1, 2], 50), Some(2));
    assert_eq!(percentile(&mut [3, 1, 2], 99), Some(3));
}
