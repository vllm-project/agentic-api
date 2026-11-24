//! Execution metrics across a multi-agent response.
use super::*;
use opentelemetry::KeyValue;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, HistogramDataPoint, MetricData, ResourceMetrics, ScopeMetrics,
};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

fn has_attribute<'a>(mut attributes: impl Iterator<Item = &'a KeyValue>, filter: Option<(&str, &str)>) -> bool {
    filter.is_none_or(|(key, value)| attributes.any(|kv| kv.key.as_str() == key && kv.value.to_string() == value))
}

/// Number of samples a histogram recorded, optionally filtered by one attribute.
fn samples(export: &ResourceMetrics, name: &str, filter: Option<(&str, &str)>) -> u64 {
    let mut count = 0;
    for metric in export.scope_metrics().flat_map(ScopeMetrics::metrics) {
        if metric.name() != name {
            continue;
        }
        count += match metric.data() {
            AggregatedMetrics::U64(MetricData::Histogram(data)) => data
                .data_points()
                .filter(|point| has_attribute(point.attributes(), filter))
                .map(HistogramDataPoint::count)
                .sum::<u64>(),
            AggregatedMetrics::F64(MetricData::Histogram(data)) => data
                .data_points()
                .filter(|point| has_attribute(point.attributes(), filter))
                .map(HistogramDataPoint::count)
                .sum::<u64>(),
            other => panic!("{name}: unexpected aggregation {other:?}"),
        };
    }
    count
}

/// Sum of an integer histogram's samples.
fn int_sum(export: &ResourceMetrics, name: &str) -> u64 {
    export
        .scope_metrics()
        .flat_map(ScopeMetrics::metrics)
        .filter(|metric| metric.name() == name)
        .map(|metric| match metric.data() {
            AggregatedMetrics::U64(MetricData::Histogram(data)) => {
                data.data_points().map(HistogramDataPoint::sum).sum::<u64>()
            }
            other => panic!("{name}: unexpected aggregation {other:?}"),
        })
        .sum()
}

/// One multi-agent execution records one `agentic.inference.rounds` sample
/// covering every agent's rounds, and times its first upstream data even
/// though each round runs on its own pipeline.
#[tokio::test]
async fn multi_agent_execution_records_rounds_and_first_data_once() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let (exec, server) = setup().await;
    let metrics = crate::executor::telemetry::ExecutorMetrics::new(&provider.meter("agentic_core"));
    let exec = Arc::new(ExecutionContext::clone(&exec).with_metrics(metrics));
    let request = RequestPayload {
        model: "test".into(),
        store: true,
        stream: true,
        input: ResponsesInput::Text("Compare proposals".into()),
        multi_agent: Some(MultiAgentConfig {
            enabled: true,
            max_concurrent_subagents: Some(2),
        }),
        ..Default::default()
    };
    let result = tokio::time::timeout(Duration::from_secs(15), response(request, exec)).await;
    server.abort();
    assert_eq!(result.unwrap().status, "completed");

    provider.force_flush().unwrap();
    let exports = exporter.get_finished_metrics().unwrap();
    let export = exports.last().expect("an export");
    let rounds = int_sum(export, "agentic.inference.rounds");
    assert_eq!(
        samples(export, "agentic.inference.rounds", None),
        1,
        "one rounds sample for the whole execution"
    );
    assert_eq!(
        rounds,
        samples(export, "agentic.stage.duration", Some(("agentic.stage", "inference"))),
        "every agent's rounds are counted"
    );
    assert!(rounds >= 3, "the root and both children ran: {rounds}");
    assert_eq!(samples(export, "agentic.time_to_first_upstream_data", None), 1);
    let _ = provider.shutdown();
}
