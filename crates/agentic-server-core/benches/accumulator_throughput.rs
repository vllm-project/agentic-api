//! Accumulator throughput benchmarks.
//!
//! Measures `ResponseAccumulator::from_json` (non-streaming path) in
//! isolation — no mock server, no DB, no executor overhead.
//!
//! The cassette YAML file from `benches/cassettes/` is parsed **once** at bench
//! init (outside criterion's measurement loop). It covers all three output item
//! types: reasoning, `function_call`, and message. The streaming cassette fails
//! strict `decode_upstream` validation (its terminal output does not match its
//! completed item events), so there is no streaming group.
//!
//! | Group | Path timed |
//! |---|---|
//! | `accumulator_concurrent_json` | N threads × `from_json` |
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
use serde::Deserialize;

use agentic_core::executor::accumulator::ResponseAccumulator;

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
    body: Option<serde_json::Value>,
}

fn load_cassette(path: &str) -> TurnCassette {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_yml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
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
    /// Serialised JSON bodies extracted from the non-streaming cassette (all turns).
    json_bodies: Vec<String>,
}

impl Fixtures {
    fn load() -> Self {
        let nonstreaming = load_cassette(CASSETTE_NONSTREAMING);
        Self {
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
                                    s.spawn(|| black_box(ResponseAccumulator::from_json(black_box(body)).unwrap()))
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

fn init_benches(c: &mut Criterion) {
    let fixtures = Fixtures::load();
    bench_accumulator_concurrent_json(c, &fixtures);
}

criterion_group!(accumulator_benches, init_benches);
