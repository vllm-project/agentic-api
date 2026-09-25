//! Metric capture and the metric cardinality contract, shared by the core
//! and server metric tests.
//!
//! Every test builds its own meter provider with an in-memory exporter, so
//! tests run in parallel without sharing instruments. [`assert_allowed`] is the
//! build-failing cardinality check: an instrument, attribute key, or attribute
//! value outside the tables below fails the test that recorded it. The tables
//! mirror `docs/deploying/observability.md`; change both together.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::time::Duration;

use agentic_core::executor::telemetry::ExecutorMetrics;
use opentelemetry::metrics::{Meter, MeterProvider as _};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

/// Attribute keys each instrument may carry.
const INSTRUMENT_KEYS: &[(&str, &[&str])] = &[
    (
        "agentic.execution.count",
        &[
            "agentic.api",
            "agentic.route",
            "agentic.stream",
            "agentic.execution.outcome",
            "error.type",
        ],
    ),
    (
        "agentic.execution.active",
        &["agentic.api", "agentic.route", "agentic.stream"],
    ),
    (
        "agentic.execution.duration",
        &[
            "agentic.api",
            "agentic.route",
            "agentic.stream",
            "agentic.execution.outcome",
            "error.type",
        ],
    ),
    (
        "agentic.delivery.count",
        &[
            "agentic.api",
            "agentic.route",
            "agentic.stream",
            "agentic.delivery.outcome",
        ],
    ),
    ("agentic.delivery.wait.duration", &["agentic.api"]),
    (
        "agentic.stage.duration",
        &["agentic.stage", "agentic.tool.type", "error.type"],
    ),
    ("agentic.inference.rounds", &["agentic.api"]),
    (
        "gen_ai.client.token.usage",
        &["gen_ai.operation.name", "gen_ai.token.type", "agentic.api"],
    ),
    ("agentic.time_to_first_upstream_data", &["agentic.api"]),
    ("agentic.time_to_first_client_event", &["agentic.api"]),
    ("agentic.time_to_first_text", &["agentic.api"]),
    ("agentic.websocket.connections.active", &[]),
    ("agentic.websocket.queue.wait.duration", &[]),
    (
        "http.server.request.duration",
        &[
            "http.request.method",
            "url.scheme",
            "http.route",
            "http.response.status_code",
        ],
    ),
    ("http.server.active_requests", &["http.request.method", "url.scheme"]),
];

/// Values each attribute key may take. Keys absent here are checked by
/// [`value_allowed`].
const KEY_VALUES: &[(&str, &[&str])] = &[
    ("agentic.api", &["responses", "messages"]),
    ("agentic.route", &["executor", "proxy"]),
    ("agentic.stream", &["true", "false"]),
    (
        "agentic.execution.outcome",
        &["completed", "incomplete", "failed", "cancelled"],
    ),
    (
        "agentic.delivery.outcome",
        &["delivered", "disconnected", "not_started"],
    ),
    (
        "error.type",
        &[
            "storage",
            "persistence",
            "conversation_locked",
            "upstream_status",
            "upstream_transport",
            "network",
            "parse",
            "stream",
            "not_found",
            "invalid_request",
            "payload_too_large",
            "resource_limit",
            "conflict",
            "compaction",
            "tool",
            "upstream_error",
            "round_budget",
            "panic",
            "cancelled",
        ],
    ),
    (
        "agentic.stage",
        &["rehydrate", "inference", "tool", "compaction", "persist"],
    ),
    (
        "agentic.tool.type",
        &[
            "function",
            "tool_search",
            "custom",
            "shell",
            "codex_namespace",
            "mcp",
            "web_search",
            "file_search",
            "code_interpreter",
        ],
    ),
    ("gen_ai.operation.name", &["chat"]),
    ("gen_ai.token.type", &["input", "output"]),
    (
        "http.request.method",
        &[
            "CONNECT", "DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT", "TRACE", "_OTHER",
        ],
    ),
    ("url.scheme", &["http"]),
];

/// One exported data point, flattened.
#[derive(Debug, Clone)]
pub struct Point {
    pub name: String,
    pub attributes: BTreeMap<String, String>,
    pub value: PointValue,
}

#[derive(Debug, Clone, Copy)]
pub enum PointValue {
    /// A counter or up-down counter value.
    Sum(i64),
    /// A histogram of seconds.
    Histogram { count: u64, sum: f64 },
    /// A histogram of integers: tokens or rounds.
    IntHistogram { count: u64, sum: u64 },
}

impl Point {
    fn matches(&self, name: &str, filters: &[(&str, &str)]) -> bool {
        self.name == name
            && filters
                .iter()
                .all(|(key, value)| self.attributes.get(*key).is_some_and(|actual| actual == value))
    }
}

/// A meter provider whose exports stay in memory for one test.
pub struct Metrics {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

impl Metrics {
    pub fn new() -> Self {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        Self { provider, exporter }
    }

    pub fn meter(&self, scope: &'static str) -> Meter {
        self.provider.meter(scope)
    }

    /// Executor instruments bound to this provider.
    pub fn executor(&self) -> ExecutorMetrics {
        ExecutorMetrics::new(&self.meter("agentic_core"))
    }

    /// Every data point of the latest (cumulative) export.
    pub fn points(&self) -> Vec<Point> {
        self.provider.force_flush().unwrap();
        let exports = self.exporter.get_finished_metrics().unwrap();
        exports.last().map(sdk_points).unwrap_or_default()
    }

    /// Poll until `ready` holds, then check the cardinality contract.
    ///
    /// Executions finalize when their guard drops, which can land a few
    /// milliseconds after the request future returns (a stream reaped on a
    /// later tick, a `sqlx` worker releasing a span).
    pub async fn wait_for(&self, what: &str, ready: impl Fn(&[Point]) -> bool) -> Vec<Point> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let points = self.points();
            if ready(&points) {
                assert_allowed(&points);
                return points;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}: {points:#?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

impl Drop for Metrics {
    fn drop(&mut self) {
        let _ = self.provider.shutdown();
    }
}

fn sdk_points(export: &ResourceMetrics) -> Vec<Point> {
    let mut points = Vec::new();
    for metric in export.scope_metrics().flat_map(ScopeMetrics::metrics) {
        let name = metric.name().to_owned();
        let mut push = |attributes: BTreeMap<String, String>, value| {
            points.push(Point {
                name: name.clone(),
                attributes,
                value,
            });
        };
        match metric.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                for p in sum.data_points() {
                    let value = PointValue::Sum(i64::try_from(p.value()).unwrap());
                    push(attributes(p.attributes()), value);
                }
            }
            AggregatedMetrics::I64(MetricData::Sum(sum)) => {
                for p in sum.data_points() {
                    push(attributes(p.attributes()), PointValue::Sum(p.value()));
                }
            }
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                for p in histogram.data_points() {
                    let value = PointValue::Histogram {
                        count: p.count(),
                        sum: p.sum(),
                    };
                    push(attributes(p.attributes()), value);
                }
            }
            AggregatedMetrics::U64(MetricData::Histogram(histogram)) => {
                for p in histogram.data_points() {
                    let value = PointValue::IntHistogram {
                        count: p.count(),
                        sum: p.sum(),
                    };
                    push(attributes(p.attributes()), value);
                }
            }
            other => panic!("{name}: unexpected aggregation {other:?}"),
        }
    }
    points
}

fn attributes<'a>(kvs: impl Iterator<Item = &'a opentelemetry::KeyValue>) -> BTreeMap<String, String> {
    kvs.map(|kv| (kv.key.to_string(), kv.value.to_string())).collect()
}

/// Sum of matching counter values, or number of matching histogram samples.
pub fn total(points: &[Point], name: &str, filters: &[(&str, &str)]) -> i64 {
    points
        .iter()
        .filter(|point| point.matches(name, filters))
        .map(|point| match point.value {
            PointValue::Sum(value) => value,
            PointValue::Histogram { count, .. } | PointValue::IntHistogram { count, .. } => {
                i64::try_from(count).unwrap()
            }
        })
        .sum()
}

/// Sum of the seconds recorded into matching histogram points.
pub fn histogram_sum(points: &[Point], name: &str, filters: &[(&str, &str)]) -> f64 {
    points
        .iter()
        .filter(|point| point.matches(name, filters))
        .map(|point| match point.value {
            PointValue::Histogram { sum, .. } => sum,
            _ => panic!("{name} is not a histogram of seconds"),
        })
        .sum()
}

/// Sum of the integers recorded into matching histogram points.
pub fn int_histogram_sum(points: &[Point], name: &str, filters: &[(&str, &str)]) -> u64 {
    points
        .iter()
        .filter(|point| point.matches(name, filters))
        .map(|point| match point.value {
            PointValue::IntHistogram { sum, .. } => sum,
            _ => panic!("{name} is not an integer histogram"),
        })
        .sum()
}

/// Whether any point of `name` exists at all.
pub fn recorded(points: &[Point], name: &str) -> bool {
    points.iter().any(|point| point.name == name)
}

/// Fail on any instrument, attribute key, or attribute value outside the
/// cardinality contract.
pub fn assert_allowed(points: &[Point]) {
    for point in points {
        let keys = INSTRUMENT_KEYS
            .iter()
            .find(|(name, _)| *name == point.name)
            .unwrap_or_else(|| panic!("instrument {} is not in the metric allow-list", point.name))
            .1;
        for (key, value) in &point.attributes {
            assert!(
                keys.contains(&key.as_str()),
                "{}: attribute {key} is not allowed on this instrument",
                point.name
            );
            assert!(
                value_allowed(key, value),
                "{}: {key}={value:?} is outside the allowed values",
                point.name
            );
        }
    }
}

fn value_allowed(key: &str, value: &str) -> bool {
    if let Some((_, values)) = KEY_VALUES.iter().find(|(allowed, _)| *allowed == key) {
        return values.contains(&value);
    }
    match key {
        // Route templates come from the router, never the request target.
        "http.route" => value.starts_with('/') && !value.contains('?'),
        "http.response.status_code" => value.parse::<u16>().is_ok_and(|status| (100..=599).contains(&status)),
        _ => false,
    }
}
