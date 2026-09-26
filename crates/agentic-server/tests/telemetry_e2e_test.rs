//! The whole pipeline as the binary runs it: `telemetry::init` installs the
//! global subscriber and providers, the real router is built afterwards (so
//! its instruments bind to the real meter provider), requests flow over TCP,
//! and a local OTLP/HTTP stub receives the spans and metrics.
//!
//! One test per binary: the global subscriber can only be installed once.

use std::time::Duration;

use opentelemetry_proto::tonic::metrics::v1::metric::Data;
use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NumberValue;

use agentic_server::telemetry::{self, TelemetryConfig};

mod common;
use common::otlp_stub::{OtlpStub, StubMode, service_name};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_requests_reach_the_collector_with_service_identity() {
    let (endpoint, stub) = OtlpStub::spawn(StubMode::Accept).await;
    let config = TelemetryConfig::from_lookup(|name| match name {
        "OTEL_TRACES_EXPORTER" | "OTEL_METRICS_EXPORTER" => Some("otlp".to_owned()),
        "OTEL_SERVICE_NAME" => Some("agentic-api-e2e".to_owned()),
        _ => None,
    })
    .unwrap()
    .with_otlp_endpoint(&endpoint)
    .with_otlp_timeout(Duration::from_secs(2));
    let guard = telemetry::init(&config).unwrap();
    assert!(guard.is_enabled());

    // Providers are registered before the router exists, as in `main`.
    let (llm_url, _llm) = common::spawn_mock_llm().await;
    let (gateway, _gateway) = common::spawn_gateway(common::test_state(&common::test_config(&llm_url))).await;
    let client = reqwest::Client::new();

    let health = client.get(format!("{gateway}/health")).send().await.unwrap();
    assert_eq!(health.status(), 200);
    let missing = client.get(format!("{gateway}/no-such-route")).send().await.unwrap();
    assert_eq!(missing.status(), 404);
    // A request the proxy forwards upstream: the mock LLM has no such route, so
    // the gateway relays its 404 — the span still carries the matched template.
    let proxied = client
        .post(format!("{gateway}/v1/responses"))
        .header("authorization", "Bearer test-key")
        .body(r#"{"model":"m","input":"hi","store":false}"#)
        .send()
        .await
        .unwrap();
    assert!(proxied.status().is_client_error() || proxied.status().is_server_error());

    guard.shutdown(Duration::from_secs(5)).await.unwrap();

    let traces = stub.trace_exports().await;
    let mut span_names: Vec<String> = traces
        .iter()
        .flat_map(|export| {
            export.resource_spans.iter().inspect(|resource_spans| {
                assert_eq!(service_name(resource_spans.resource.as_ref()), Some("agentic-api-e2e"));
            })
        })
        .flat_map(|resource_spans| resource_spans.scope_spans.iter())
        .flat_map(|scope| scope.spans.iter().map(|span| span.name.clone()))
        .collect();
    span_names.sort();
    assert_eq!(
        span_names,
        ["GET", "GET /health", "POST /v1/responses", "agentic.execute"]
    );

    let metrics = stub.metric_exports().await;
    let latest = metrics.last().expect("metrics exported on shutdown");
    let mut duration_count = 0;
    let mut active_value = None;
    for metric in latest
        .resource_metrics
        .iter()
        .inspect(|resource_metrics| {
            assert_eq!(
                service_name(resource_metrics.resource.as_ref()),
                Some("agentic-api-e2e")
            );
        })
        .flat_map(|resource_metrics| resource_metrics.scope_metrics.iter())
        .flat_map(|scope| scope.metrics.iter())
    {
        match (metric.name.as_str(), metric.data.as_ref()) {
            ("http.server.request.duration", Some(Data::Histogram(histogram))) => {
                duration_count += histogram.data_points.iter().map(|point| point.count).sum::<u64>();
            }
            ("http.server.active_requests", Some(Data::Sum(sum))) => {
                active_value = Some(
                    sum.data_points
                        .iter()
                        .map(|point| match point.value {
                            Some(NumberValue::AsInt(value)) => value,
                            other => panic!("unexpected active_requests value {other:?}"),
                        })
                        .sum::<i64>(),
                );
            }
            _ => {}
        }
    }
    assert_eq!(duration_count, 3, "one duration sample per request");
    assert_eq!(active_value, Some(0), "no request left in flight");
}
