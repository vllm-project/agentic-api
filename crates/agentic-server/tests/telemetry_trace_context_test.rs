//! Every request's inbound `traceparent` must reach the upstream model server,
//! not only the first one after the subscriber is installed (#459), and inbound
//! `baggage` must survive the stateful executor path, which re-injects context (#460).
//!
//! One test per binary: the global subscriber can only be installed once.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::HeaderMap;
use axum::routing::post;
use serde_json::json;
use tokio::net::TcpListener;

use agentic_server::telemetry::{self, TelemetryConfig};

#[allow(dead_code)]
mod common;
use common::otlp_stub::{OtlpStub, StubMode};

/// A Responses upstream that records the `traceparent` header of each request.
async fn spawn_recording_upstream() -> (String, Arc<Mutex<Vec<(Option<String>, Option<String>)>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let app = Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap| {
            let recorder = Arc::clone(&recorder);
            async move {
                let header = |name: &str| {
                    headers
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned)
                };
                recorder
                    .lock()
                    .unwrap()
                    .push((header("traceparent"), header("baggage")));
                axum::Json(json!({"id":"resp_upstream", "object":"response", "created_at":1,
                    "model":"m", "status":"completed", "output":[],
                    "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn trace_id(traceparent: &str) -> &str {
    traceparent.split('-').nth(1).expect("traceparent has a trace id")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_request_propagates_its_inbound_trace_id_upstream() {
    let (endpoint, stub) = OtlpStub::spawn(StubMode::Accept).await;
    let config = TelemetryConfig::from_lookup(|name| match name {
        "OTEL_TRACES_EXPORTER" => Some("otlp".to_owned()),
        _ => None,
    })
    .unwrap()
    .with_otlp_endpoint(&endpoint)
    .with_otlp_timeout(Duration::from_secs(2));
    let guard = telemetry::init(&config).unwrap();

    let (upstream, seen) = spawn_recording_upstream().await;
    let mut config = common::test_config(&upstream);
    config.db_url = Some("sqlite://?mode=memory".into());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::new(
        agentic_core::executor::ExecutionContext::from_config(&config)
            .await
            .unwrap(),
    );
    let (gateway, _gateway) = common::spawn_gateway(state).await;
    let client = reqwest::Client::new();

    let inbound: Vec<String> = (1..=3).map(|n| format!("{n:032x}")).collect();
    for trace in &inbound {
        let response = client
            .post(format!("{gateway}/v1/responses"))
            .header("authorization", "Bearer test-key")
            .header("content-type", "application/json")
            .header("traceparent", format!("00-{trace}-00f067aa0ba902b7-01"))
            .body(r#"{"model":"m","input":"hi","store":false}"#)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
    }

    let upstream_traces: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(traceparent, _)| trace_id(traceparent.as_deref().expect("upstream carries traceparent")).to_owned())
        .collect();
    assert_eq!(
        upstream_traces, inbound,
        "each upstream request continues its caller's trace"
    );

    // The stateful path builds its own upstream request and re-injects context
    // from the gateway span, so baggage must come through the propagator.
    let stored = client
        .post(format!("{gateway}/v1/responses"))
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .header("traceparent", "00-0000000000000000000000000000000a-00f067aa0ba902b7-01")
        .header("baggage", "tenant=acme")
        .body(r#"{"model":"m","input":"hi","store":true}"#)
        .send()
        .await
        .unwrap();
    assert!(stored.status().is_success(), "{}", stored.status());
    let (traceparent, baggage) = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        trace_id(traceparent.as_deref().unwrap()),
        "0000000000000000000000000000000a"
    );
    assert_eq!(baggage.as_deref(), Some("tenant=acme"), "baggage reaches the upstream");

    guard.shutdown(Duration::from_secs(5)).await.unwrap();
    let server_spans: Vec<String> = stub
        .trace_exports()
        .await
        .iter()
        .flat_map(|export| export.resource_spans.iter())
        .flat_map(|resource_spans| resource_spans.scope_spans.iter())
        .flat_map(|scope| scope.spans.iter())
        .filter(|span| span.name == "POST /v1/responses" && span.trace_id.last() != Some(&0x0a))
        .map(|span| opentelemetry::trace::TraceId::from_bytes(span.trace_id.as_slice().try_into().unwrap()).to_string())
        .collect();
    let mut sorted = server_spans.clone();
    sorted.sort();
    assert_eq!(
        sorted, inbound,
        "one exported server span per request, in the caller's trace"
    );
}
