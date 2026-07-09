//! Accumulator throughput benchmarks.
//!
//! Measures `ResponseAccumulator::from_stream` (streaming path) and
//! `ResponseAccumulator::from_json` (non-streaming path) in isolation —
//! no mock server, no DB, no executor overhead.
//!
//! Cassette YAML files from `benches/cassettes/` are parsed **once** at bench
//! init (outside criterion's measurement loop). Each cassette pair covers all
//! three output item types: reasoning, `function_call`, and message.
//!
//! | Group | Path timed |
//! |---|---|
//! | `accumulator_json` | `from_json` — all cassettes per iteration |
//! | `accumulator_stream` | `from_stream` — all cassettes per iteration |
//! | `accumulator_concurrent_json` | N threads × `from_json` |
//! | `accumulator_concurrent_stream` | N tokio tasks × `from_stream` |
//!
//! # Configuring concurrency levels
//!
//! Set `BENCH_CONCURRENCY` to a comma-separated list of integers before running.
//! Defaults to `2,4,8,16` when unset.
//!
//! ```bash
//! BENCH_CONCURRENCY=2,4,8,16,32 cargo bench --bench benches -- accumulator
//! ```

use std::thread;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, SamplingMode, Throughput, black_box, criterion_group};
use futures::stream;
use serde::Deserialize;
use tokio::runtime::Runtime;

use agentic_core::executor::accumulator::ResponseAccumulator;
use agentic_core::executor::error::ExecutorError;

const CASSETTE_STREAMING: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/benches/cassettes/reasoning-and-tool-call-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml"
);
const CASSETTE_NONSTREAMING: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/benches/cassettes/reasoning-and-tool-call-Qwen-Qwen3-30B-A3B-FP8-nonstreaming.yaml"
);

#[derive(Deserialize)]
struct TurnCassette {
    turns: Vec<Turn>,
}

#[derive(Deserialize)]
struct Turn {
    response: TurnResponse,
}

#[derive(Deserialize)]
struct TurnResponse {
    #[serde(default)]
    sse: Vec<String>,
    body: Option<serde_json::Value>,
}

fn load_cassette(path: &str) -> TurnCassette {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_yml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

/// Extract `data: ...` lines from raw SSE entries across all turns.
fn extract_data_lines(cassette: &TurnCassette) -> Vec<String> {
    cassette
        .turns
        .iter()
        .flat_map(|t| t.response.sse.iter())
        .flat_map(|entry| entry.lines())
        .filter(|line| line.starts_with("data: "))
        .map(ToString::to_string)
        .collect()
}

/// Extract JSON body strings from all turns that have a non-streaming body.
fn extract_json_bodies(cassette: &TurnCassette) -> Vec<String> {
    cassette
        .turns
        .iter()
        .filter_map(|t| t.response.body.as_ref())
        .map(|v| serde_json::to_string(v).unwrap())
        .collect()
}

/// Parse `BENCH_CONCURRENCY` env var as a comma-separated list of integers.
/// Falls back to `[2, 4, 8, 16]` when unset or unparseable.
fn concurrency_levels() -> Vec<usize> {
    std::env::var("BENCH_CONCURRENCY")
        .ok()
        .and_then(|val| {
            let levels: Vec<usize> = val.split(',').filter_map(|s| s.trim().parse().ok()).collect();
            if levels.is_empty() { None } else { Some(levels) }
        })
        .unwrap_or_else(|| vec![2, 4, 8, 16])
}

struct Fixtures {
    /// `data: ...` lines extracted from the streaming cassette (all turns).
    sse_lines: Vec<String>,
    /// Serialised JSON bodies extracted from the non-streaming cassette (all turns).
    json_bodies: Vec<String>,
}

impl Fixtures {
    fn load() -> Self {
        let streaming = load_cassette(CASSETTE_STREAMING);
        let nonstreaming = load_cassette(CASSETTE_NONSTREAMING);
        Self {
            sse_lines: extract_data_lines(&streaming),
            json_bodies: extract_json_bodies(&nonstreaming),
        }
    }
}

fn bench_accumulator_concurrent_json(c: &mut Criterion, f: &Fixtures) {
    let mut group = c.benchmark_group("accumulator_concurrent_json");
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));
    group.sampling_mode(SamplingMode::Flat);
    for concurrency in concurrency_levels() {
        let total = f.json_bodies.len() as u64 * concurrency as u64;
        eprintln!("  accumulator_concurrent_json/{concurrency}t ({total} total bodies)");
        group.throughput(Throughput::Elements(total));
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &concurrency| {
                b.iter(|| {
                    thread::scope(|s| {
                        let handles: Vec<_> = f
                            .json_bodies
                            .iter()
                            .flat_map(|body| {
                                (0..concurrency).map(move |_| {
                                    s.spawn(|| {
                                        black_box(ResponseAccumulator::from_json(black_box(body), None).unwrap())
                                    })
                                })
                            })
                            .collect();
                        for h in handles {
                            h.join().unwrap();
                        }
                    });
                });
            },
        );
    }
    group.finish();
}

fn bench_accumulator_concurrent_stream(c: &mut Criterion, f: &Fixtures) {
    let sse_lines = f.sse_lines.clone();
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("accumulator_concurrent_stream");
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));
    group.sampling_mode(SamplingMode::Flat);
    for concurrency in concurrency_levels() {
        let total = sse_lines.len() as u64 * concurrency as u64;
        eprintln!("  accumulator_concurrent_stream/{concurrency}t ({total} total SSE lines)");
        group.throughput(Throughput::Elements(total));
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &concurrency| {
                b.to_async(&rt).iter_batched(
                    || {
                        (0..concurrency)
                            .map(|_| Box::pin(stream::iter(sse_lines.clone().into_iter().map(Ok::<_, ExecutorError>))))
                            .collect::<Vec<_>>()
                    },
                    |streams| async move {
                        let handles: Vec<_> = streams
                            .into_iter()
                            .map(|s| {
                                tokio::spawn(async move {
                                    black_box(ResponseAccumulator::from_stream(s, None).await.unwrap())
                                })
                            })
                            .collect();
                        for h in handles {
                            h.await.unwrap();
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

fn init_benches(c: &mut Criterion) {
    let fixtures = Fixtures::load();
    bench_accumulator_concurrent_json(c, &fixtures);
    bench_accumulator_concurrent_stream(c, &fixtures);
}

criterion_group!(accumulator_benches, init_benches);
