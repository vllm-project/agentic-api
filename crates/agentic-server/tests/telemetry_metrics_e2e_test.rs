//! The gateway binary, configured only through standard `OTEL_*` variables,
//! with trace sampling at zero, exporting to a local OTLP/HTTP stub.
//!
//! Metrics must not depend on sampling, carry the configured service name,
//! and finalize requests that are still in flight when the process receives
//! SIGTERM: the drain deadline abandons them, runtime shutdown drops them,
//! and the telemetry flush that follows exports their cancellation.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::response::{IntoResponse as _, Response};
use futures::{SinkExt as _, StreamExt as _};
use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value as AnyValue;
use opentelemetry_proto::tonic::metrics::v1::metric::Data;
use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NumberValue;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

#[allow(dead_code)]
mod common;
#[path = "../../agentic-server-core/tests/execution_metrics/harness.rs"]
mod harness;
use common::otlp_stub::{OtlpStub, StubMode, service_name};
use harness::{Point, PointValue, total};

const SERVICE: &str = "agentic-metrics-e2e";
const USAGE: &str = r#"{"input_tokens":7,"input_tokens_details":{"cached_tokens":0},"output_tokens":2,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":9}"#;

/// Every instrument this scenario exercises.
const EXPECTED_INSTRUMENTS: &[&str] = &[
    "agentic.execution.count",
    "agentic.execution.active",
    "agentic.execution.duration",
    "agentic.delivery.count",
    "agentic.delivery.wait.duration",
    "agentic.stage.duration",
    "agentic.inference.rounds",
    "gen_ai.client.token.usage",
    "agentic.time_to_first_upstream_data",
    "agentic.time_to_first_client_event",
    "agentic.time_to_first_text",
    "agentic.websocket.connections.active",
    "agentic.websocket.queue.wait.duration",
    "http.server.request.duration",
    "http.server.active_requests",
];

/// Completes unless the prompt says `HANG`, which streams `response.created`
/// and then never finishes.
async fn upstream() -> (String, tokio::task::JoinHandle<()>) {
    let never = Arc::new(Notify::new());
    let router = axum::Router::new().route(
        "/v1/responses",
        axum::routing::post(move |body: Bytes| {
            let never = Arc::clone(&never);
            async move { respond(&body, never) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() }),
    )
}

fn respond(body: &[u8], never: Arc<Notify>) -> Response {
    let message = r#"{"id":"msg_up","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hi","annotations":[]}]}"#;
    let request: Value = serde_json::from_slice(body).unwrap();
    if request["stream"] != true {
        let body = format!(
            r#"{{"id":"resp_up","object":"response","created_at":0,"model":"m","status":"completed","output":[{message}],"usage":{USAGE}}}"#
        );
        return ([("content-type", "application/json")], body).into_response();
    }
    let head = "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_up\",\"status\":\"in_progress\",\"output\":[]}}\n\n";
    let tail = format!(
        "data: {{\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{{\"id\":\"msg_up\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}}}\n\n\
         data: {{\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"output_index\":0,\"content_index\":0,\"delta\":\"hi\"}}\n\n\
         data: {{\"type\":\"response.output_item.done\",\"sequence_number\":3,\"output_index\":0,\"item\":{message}}}\n\n\
         data: {{\"type\":\"response.completed\",\"sequence_number\":4,\"response\":{{\"id\":\"resp_up\",\"status\":\"completed\",\"output\":[{message}],\"usage\":{USAGE}}}}}\n\n\
         data: [DONE]\n\n"
    );
    let hang = String::from_utf8_lossy(body).contains("HANG");
    let rest = futures::stream::once(async move {
        if hang {
            never.notified().await;
        }
        Ok::<_, std::io::Error>(Bytes::from(tail))
    });
    let body = futures::stream::once(std::future::ready(Ok(Bytes::from_static(head.as_bytes())))).chain(rest);
    ([("content-type", "text/event-stream")], Body::from_stream(body)).into_response()
}

struct Gateway {
    child: Child,
    url: String,
    home: tempfile::TempDir,
}

impl Gateway {
    fn spawn(llm: &str, otlp: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let log = std::fs::File::create(home.path().join("server.log")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentic-server"))
            .env_clear()
            .env("HOME", home.path())
            .env("AGENTIC_API_HOME", home.path())
            .env("RUST_LOG", "info")
            .env("OTEL_TRACES_EXPORTER", "otlp")
            .env("OTEL_METRICS_EXPORTER", "otlp")
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", otlp)
            .env("OTEL_SERVICE_NAME", SERVICE)
            .env("OTEL_TRACES_SAMPLER", "parentbased_traceidratio")
            .env("OTEL_TRACES_SAMPLER_ARG", "0")
            .env("OTEL_BSP_SCHEDULE_DELAY", "50")
            .args(["--llm-api-base", llm, "--skip-llm-ready-check"])
            .args(["--gateway-host", "127.0.0.1", "--gateway-port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("agentic-server starts");
        Self {
            child,
            url: format!("http://127.0.0.1:{port}"),
            home,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.home.path().join("server.log")).unwrap_or_default()
    }

    async fn wait_ready(&self) {
        let client = reqwest::Client::new();
        for _ in 0..500 {
            if client.get(format!("{}/health", self.url)).send().await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("gateway did not start: {}", self.log());
    }

    async fn terminate(&mut self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        // Drain deadline (8 s), runtime shutdown (1 s), telemetry flush (3 s).
        for _ in 0..1000 {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "gateway exited with {status}: {}", self.log());
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("gateway did not exit: {}", self.log());
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn request(prompt: &str, stream: bool) -> Value {
    json!({"model": "m", "input": prompt, "stream": stream, "store": true})
}

fn create(prompt: &str) -> Message {
    let mut event = request(prompt, true);
    event["store"] = json!(false);
    event["type"] = json!("response.create");
    Message::Text(event.to_string().into())
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn read_until(socket: &mut Socket, event_type: &str, count: usize) {
    let mut seen = 0;
    tokio::time::timeout(Duration::from_secs(10), async {
        while seen < count {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                let event: Value = serde_json::from_str(&text).unwrap();
                seen += usize::from(event["type"] == event_type);
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("saw {seen} of {count} {event_type} events"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsampled_gateway_exports_metrics_and_finalizes_shutdown_cancelled_requests() {
    let (otlp, stub) = OtlpStub::spawn(StubMode::Accept).await;
    let (llm, _llm) = upstream().await;
    let mut gateway = Gateway::spawn(&llm, &otlp);
    gateway.wait_ready().await;
    let client = reqwest::Client::new();
    let url = format!("{}/v1/responses", gateway.url);

    // Completed over HTTP: streamed and blocking.
    let streamed = client.post(&url).json(&request("hello", true)).send().await.unwrap();
    assert!(streamed.text().await.unwrap().contains("response.completed"));
    let blocking = client.post(&url).json(&request("hello", false)).send().await.unwrap();
    assert_eq!(blocking.status(), 200, "{}", gateway.log());
    blocking.bytes().await.unwrap();

    // Completed over WebSocket, twice on one connection.
    let ws_url = format!("{}/v1/responses", gateway.url.replace("http:", "ws:"));
    let (mut socket, _) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
    socket.send(create("hello")).await.unwrap();
    socket.send(create("hello")).await.unwrap();
    read_until(&mut socket, "response.completed", 2).await;

    // Still running at SIGTERM: one HTTP stream and one WebSocket request.
    let mut in_flight = client
        .post(&url)
        .json(&request("HANG", true))
        .send()
        .await
        .unwrap()
        .bytes_stream();
    assert!(String::from_utf8_lossy(&in_flight.next().await.unwrap().unwrap()).contains("response.created"));
    socket.send(create("HANG")).await.unwrap();
    read_until(&mut socket, "response.created", 1).await;

    gateway.terminate().await;
    drop((in_flight, socket));

    let traces = stub.trace_exports().await;
    let spans: usize = traces
        .iter()
        .flat_map(|export| &export.resource_spans)
        .flat_map(|resource| &resource.scope_spans)
        .map(|scope| scope.spans.len())
        .sum();
    assert_eq!(spans, 0, "OTEL_TRACES_SAMPLER_ARG=0 samples nothing");

    let exports = stub.metric_exports().await;
    let latest = exports.last().expect("metrics are flushed at shutdown");
    for resource in &latest.resource_metrics {
        assert_eq!(service_name(resource.resource.as_ref()), Some(SERVICE));
    }
    let points = otlp_points(latest);
    harness::assert_allowed(&points);
    for instrument in EXPECTED_INSTRUMENTS {
        assert!(
            points.iter().any(|point| point.name == *instrument),
            "{instrument} was not exported"
        );
    }

    let outcome = |outcome: &str| {
        total(
            &points,
            "agentic.execution.count",
            &[("agentic.execution.outcome", outcome)],
        )
    };
    assert_eq!(total(&points, "agentic.execution.count", &[]), 6, "{points:#?}");
    assert_eq!(outcome("completed"), 4);
    assert_eq!(outcome("cancelled"), 2, "both in-flight requests finalized at shutdown");
    assert_eq!(
        total(
            &points,
            "agentic.delivery.count",
            &[("agentic.delivery.outcome", "disconnected")]
        ),
        2
    );
    assert_eq!(
        total(&points, "agentic.execution.active", &[]),
        0,
        "nothing left active"
    );
    assert_eq!(total(&points, "agentic.websocket.connections.active", &[]), 0);
    assert_eq!(total(&points, "agentic.websocket.queue.wait.duration", &[]), 3);
    assert_eq!(
        total(&points, "gen_ai.client.token.usage", &[("gen_ai.token.type", "input")]),
        4,
        "one sample per completed upstream call"
    );
    assert_eq!(total(&points, "http.server.active_requests", &[]), 0);
}

fn otlp_points(export: &opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest) -> Vec<Point> {
    let mut points = Vec::new();
    for metric in export
        .resource_metrics
        .iter()
        .flat_map(|resource| &resource.scope_metrics)
        .flat_map(|scope| &scope.metrics)
    {
        let mut push = |attributes: &[KeyValue], value| {
            points.push(Point {
                name: metric.name.clone(),
                attributes: otlp_attributes(attributes),
                value,
            });
        };
        match &metric.data {
            Some(Data::Sum(sum)) => {
                for point in &sum.data_points {
                    let Some(NumberValue::AsInt(value)) = point.value else {
                        panic!("{}: every gateway sum is an integer", metric.name);
                    };
                    push(&point.attributes, PointValue::Sum(value));
                }
            }
            Some(Data::Histogram(histogram)) => {
                for point in &histogram.data_points {
                    let value = PointValue::Histogram {
                        count: point.count,
                        sum: point.sum.unwrap_or_default(),
                    };
                    push(&point.attributes, value);
                }
            }
            other => panic!("{}: unexpected data {other:?}", metric.name),
        }
    }
    points
}

fn otlp_attributes(attributes: &[KeyValue]) -> BTreeMap<String, String> {
    attributes
        .iter()
        .map(|attribute| {
            let value = match attribute.value.as_ref().and_then(|value| value.value.as_ref()) {
                Some(AnyValue::StringValue(value)) => value.clone(),
                Some(AnyValue::BoolValue(value)) => value.to_string(),
                Some(AnyValue::IntValue(value)) => value.to_string(),
                Some(AnyValue::DoubleValue(value)) => value.to_string(),
                other => panic!("{}: unexpected attribute value {other:?}", attribute.key),
            };
            (attribute.key.clone(), value)
        })
        .collect()
}
