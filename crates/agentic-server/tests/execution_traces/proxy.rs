use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse as _, Response};
use futures::StreamExt as _;
use opentelemetry::Value;
use serde_json::json;
use tokio::sync::Mutex;

use super::harness::{Server, attr, finished, gateway, trace_test};

const INBOUND: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const RAW_RESPONSE: &str = "private proxy response bytes";

struct Observed {
    headers: HeaderMap,
    body: Bytes,
    query: String,
}

async fn proxy_handler(
    State(seen): State<Arc<Mutex<Vec<Observed>>>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let streaming = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["stream"] == true;
    let query = uri.query().unwrap_or_default().to_owned();
    let failed = query.contains("mode=error");
    let slow = query.contains("mode=slow");
    let chunked = query.contains("mode=chunked");
    let broken = query.contains("mode=broken");
    let empty = query.contains("mode=empty");
    seen.lock().await.push(Observed { headers, body, query });
    if failed {
        return (http::StatusCode::SERVICE_UNAVAILABLE, "private upstream error body").into_response();
    }
    if empty {
        return ([("content-type", "text/event-stream")], "").into_response();
    }
    if broken {
        let first = futures::stream::once(std::future::ready(Ok::<_, std::io::Error>(Bytes::from_static(
            b"data: private delta\n\n",
        ))));
        let last = futures::stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Err::<Bytes, _>(std::io::Error::other("private stream failure"))
        });
        return (
            [("content-type", "text/event-stream")],
            Body::from_stream(first.chain(last)),
        )
            .into_response();
    }
    if slow {
        let stream = futures::stream::once(std::future::ready(Ok::<_, std::io::Error>(Bytes::from_static(
            b"data: private delta\n\n",
        ))))
        .chain(futures::stream::pending());
        return ([("content-type", "text/event-stream")], Body::from_stream(stream)).into_response();
    }
    if chunked {
        let stream = futures::stream::once(std::future::ready(Ok::<_, std::io::Error>(Bytes::from_static(
            RAW_RESPONSE.as_bytes(),
        ))));
        return ([("content-type", "text/event-stream")], Body::from_stream(stream)).into_response();
    }
    (
        [(
            "content-type",
            if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
        )],
        RAW_RESPONSE,
    )
        .into_response()
}

async fn upstream() -> (Server, Arc<Mutex<Vec<Observed>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = axum::Router::new()
        .route("/v1/responses", axum::routing::post(proxy_handler))
        .route("/v1/messages", axum::routing::post(proxy_handler))
        .with_state(Arc::clone(&seen));
    (Server::start(router).await, seen)
}

fn request(api: &str, streaming: bool) -> String {
    if api == "responses" {
        json!({"model":"test","input":"private proxy prompt","store":false,"stream":streaming}).to_string()
    } else {
        json!({"model":"test","max_tokens":64,"messages":[{"role":"user","content":"private proxy prompt"}],"stream":streaming}).to_string()
    }
}

#[tokio::test]
async fn proxy_replaces_trace_context_and_preserves_raw_payloads_for_both_apis() {
    let _guard = trace_test().await;
    let (upstream, seen) = upstream().await;
    let gateway = gateway(&upstream.url).await;
    let client = reqwest::Client::new();
    let mut bodies = Vec::new();
    for api in ["responses", "messages"] {
        for (streaming, query) in [
            (false, "secret=private-query"),
            (true, "secret=private-query"),
            (true, "mode=chunked&secret=private-query"),
            (true, "mode=empty&secret=private-query"),
        ] {
            let body = request(api, streaming);
            let response = client
                .post(format!("{}/v1/{api}?{query}", gateway.url))
                .header("content-type", "application/json")
                .header("traceparent", INBOUND)
                .header("tracestate", "vendor=keep")
                .header("authorization", "Bearer private-auth")
                .header("x-api-key", "private-api-key")
                .body(body.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.text().await.unwrap(),
                if query.contains("mode=empty") { "" } else { RAW_RESPONSE }
            );
            bodies.push((body, api, streaming, query));
        }
    }
    let spans = finished("agentic.execute", 8).await;
    assert_eq!(spans.iter().filter(|span| span.name == "agentic.execute").count(), 8);
    let observations = seen.lock().await;
    assert_eq!(observations.len(), 8);
    for (observation, (body, api, streaming, query)) in observations.iter().zip(&bodies) {
        assert_eq!(observation.body.as_ref(), body.as_bytes());
        assert_eq!(observation.query, *query);
        assert_eq!(observation.headers["authorization"], "Bearer private-auth");
        assert_eq!(observation.headers["x-api-key"], "private-api-key");
        assert_eq!(observation.headers["tracestate"], "vendor=keep");
        let outgoing = observation.headers["traceparent"].to_str().unwrap();
        assert_ne!(outgoing, INBOUND);
        let execution = spans
            .iter()
            .find(|span| {
                span.name == "agentic.execute"
                    && outgoing == format!("00-{}-{}-01", span.span_context.trace_id(), span.span_context.span_id())
            })
            .unwrap();
        assert_eq!(attr(execution, "agentic.route"), Some(&Value::from("proxy")));
        assert_eq!(attr(execution, "agentic.api"), Some(&Value::from(*api)));
        assert_eq!(attr(execution, "agentic.stream"), Some(&Value::Bool(*streaming)));
        assert_eq!(
            attr(execution, "agentic.execution.outcome"),
            Some(&Value::from("completed"))
        );
        assert_eq!(
            attr(execution, "agentic.delivery.outcome"),
            Some(&Value::from("delivered"))
        );
        assert!(
            spans
                .iter()
                .any(|span| span.span_context.span_id() == execution.parent_span_id
                    && span.name.starts_with("POST /v1/"))
        );
    }
    drop(observations);
    gateway.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn proxy_records_upstream_rejection_and_stream_disconnect() {
    let _guard = trace_test().await;
    let (upstream, _) = upstream().await;
    let gateway = gateway(&upstream.url).await;
    let client = reqwest::Client::new();
    for api in ["responses", "messages"] {
        let response = client
            .post(format!("{}/v1/{api}?mode=error", gateway.url))
            .header("content-type", "application/json")
            .body(request(api, false))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
        assert_eq!(response.text().await.unwrap(), "private upstream error body");
        let mut response = client
            .post(format!("{}/v1/{api}?mode=slow", gateway.url))
            .header("content-type", "application/json")
            .body(request(api, true))
            .send()
            .await
            .unwrap();
        assert!(response.chunk().await.unwrap().is_some());
        drop(response);
        let response = client
            .post(format!("{}/v1/{api}?mode=broken", gateway.url))
            .header("content-type", "application/json")
            .body(request(api, true))
            .send()
            .await
            .unwrap();
        assert!(response.bytes().await.is_err());
    }
    let spans = finished("agentic.execute", 6).await;
    let executions: Vec<_> = spans.iter().filter(|span| span.name == "agentic.execute").collect();
    assert_eq!(executions.len(), 6);
    assert_eq!(
        executions
            .iter()
            .filter(|span| attr(span, "error.type") == Some(&Value::from("network")))
            .count(),
        2
    );
    for execution in executions {
        assert_eq!(attr(execution, "agentic.route"), Some(&Value::from("proxy")));
        if attr(execution, "agentic.stream") == Some(&Value::Bool(true)) {
            assert_eq!(
                attr(execution, "agentic.execution.outcome"),
                Some(&Value::from(
                    if attr(execution, "error.type") == Some(&Value::from("network")) {
                        "failed"
                    } else {
                        "cancelled"
                    }
                ))
            );
            assert_eq!(
                attr(execution, "agentic.delivery.outcome"),
                Some(&Value::from("disconnected"))
            );
        } else {
            assert_eq!(
                attr(execution, "agentic.execution.outcome"),
                Some(&Value::from("failed"))
            );
            assert_eq!(
                attr(execution, "agentic.delivery.outcome"),
                Some(&Value::from("delivered"))
            );
            assert_eq!(attr(execution, "error.type"), Some(&Value::from("upstream_status")));
        }
    }
    gateway.stop().await;
    upstream.stop().await;
}
