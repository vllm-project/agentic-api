//! A gateway MCP `tools/call` to an operator-configured server carries the calling
//! span's trace context and baggage, both as HTTP headers and in `params._meta`
//! (#460). A server URL declared in a request gets neither: baggage may hold values
//! an operator's edge added, and that server may belong to anyone.
//!
//! One test per binary: the global subscriber and propagator are process-wide.

use std::sync::{Arc, Mutex};

use std::collections::HashMap;

use agentic_core::tool::{McpClientPool, McpServerEntry};
use agentic_core::types::tools::McpToolParam;
use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use opentelemetry::propagation::TextMapCompositePropagator;
use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use serde_json::{Value, json};
use tracing::Instrument as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::layer::SubscriberExt as _;

#[derive(Debug, Default)]
struct Seen {
    headers: Option<HeaderMap>,
    meta: Option<Value>,
}

/// Stateless streamable-HTTP MCP server recording the `tools/call` request.
async fn spawn_recording_mcp() -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let recorder = Arc::clone(&seen);
    let app = Router::new().route(
        "/mcp",
        post(move |headers: HeaderMap, axum::Json(request): axum::Json<Value>| {
            let recorder = Arc::clone(&recorder);
            async move {
                let id = request["id"].clone();
                if id.is_null() {
                    return StatusCode::ACCEPTED.into_response();
                }
                let result = match request["method"].as_str() {
                    Some("initialize") => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                        "serverInfo": {"name": "stub", "version": "0"}}),
                    Some("tools/call") => {
                        let mut seen = recorder.lock().unwrap();
                        seen.headers = Some(headers);
                        seen.meta = Some(request["params"]["_meta"].clone());
                        json!({"content": [{"type": "text", "text": "ok"}], "isError": false})
                    }
                    _ => json!({}),
                };
                axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}/mcp"), seen)
}

fn trace_id_of(traceparent: &str) -> &str {
    traceparent.split('-').nth(1).expect("traceparent has a trace id")
}

/// Run one `tools/call` under a root span whose baggage was extracted from an inbound
/// `baggage` header, returning the root's trace id.
async fn call_under_root(pool: &McpClientPool, inbound_baggage: &str) -> String {
    let root = tracing::info_span!("test.root");
    let inbound = HashMap::from([("baggage".to_owned(), inbound_baggage.to_owned())]);
    let parent = opentelemetry::global::get_text_map_propagator(|propagator| propagator.extract(&inbound));
    let _ = root.set_parent(parent);
    let trace_id = format!("{}", root.context().span().span_context().trace_id());
    pool.get("tools")
        .expect("connected")
        .call_tool("echo", Some(json!({})))
        .instrument(root)
        .await
        .expect("tools/call");
    trace_id
}

#[tokio::test]
async fn only_configured_mcp_servers_receive_the_calling_trace_and_baggage() {
    // Same propagators the server installs.
    opentelemetry::global::set_text_map_propagator(TextMapCompositePropagator::new(vec![
        Box::new(TraceContextPropagator::new()),
        Box::new(BaggagePropagator::new()),
    ]));
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(InMemorySpanExporter::default())
        .build();
    let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer)).unwrap();

    // Operator-configured: context in headers and in `_meta`, replacing any
    // propagation headers the operator configured.
    let (url, seen) = spawn_recording_mcp().await;
    let configured_headers = HashMap::from([
        (
            "traceparent".to_owned(),
            "00-11111111111111111111111111111111-2222222222222222-01".to_owned(),
        ),
        ("tracestate".to_owned(), "vendor=stale".to_owned()),
        ("baggage".to_owned(), "tenant=stale".to_owned()),
    ]);
    let configured = McpClientPool::from_config(HashMap::from([(
        "tools".to_owned(),
        McpServerEntry::Http {
            url,
            headers: Some(configured_headers),
            allowed_tools: None,
            require_approval: None,
            description: None,
            defer_loading: None,
        },
    )]))
    .await;
    let trace_id = call_under_root(&configured, "tenant=acme").await;
    {
        let seen = seen.lock().unwrap();
        let headers = seen.headers.as_ref().expect("tools/call reached the server");
        let traceparents: Vec<_> = headers.get_all("traceparent").iter().collect();
        assert_eq!(traceparents.len(), 1, "one traceparent, not the configured one too");
        let traceparent = traceparents[0].to_str().unwrap();
        assert!(
            headers.get("tracestate").is_none(),
            "the configured tracestate belongs to the replaced trace"
        );
        assert_eq!(headers.get_all("baggage").iter().count(), 1);
        assert_eq!(
            trace_id_of(traceparent),
            trace_id,
            "the tool call joins the caller's trace"
        );
        assert_eq!(headers.get("baggage").expect("baggage header"), "tenant=acme");
        let meta = seen.meta.as_ref().unwrap();
        assert_eq!(
            meta["traceparent"], traceparent,
            "_meta carries the same context for non-HTTP transports"
        );
        assert_eq!(meta["baggage"], "tenant=acme");
    }

    // A baggage property that decodes to a control character is not a valid header
    // value: the call still goes out, with the trace context and with no baggage at all,
    // not the configured value.
    let trace_id = call_under_root(&configured, "tenant=acme;note=%0Afoo").await;
    {
        let seen = seen.lock().unwrap();
        let headers = seen.headers.as_ref().expect("tools/call reached the server");
        let traceparent = headers.get("traceparent").unwrap().to_str().unwrap();
        assert_eq!(trace_id_of(traceparent), trace_id);
        assert!(
            headers.get("baggage").is_none(),
            "the configured baggage would misattribute the call: {:?}",
            headers.get("baggage")
        );
    }

    // Request-declared: nothing beyond what the request itself supplied.
    let (url, seen) = spawn_recording_mcp().await;
    let param: McpToolParam = serde_json::from_value(json!({"server_label": "tools", "server_url": url})).unwrap();
    let declared = McpClientPool::from_params_with_allowed_hosts(&[param], &[]).await;
    call_under_root(&declared, "tenant=acme").await;
    let seen = seen.lock().unwrap();
    let headers = seen.headers.as_ref().expect("tools/call reached the server");
    assert!(
        headers.get("traceparent").is_none(),
        "no trace context for a request-declared server"
    );
    assert!(
        headers.get("baggage").is_none(),
        "no baggage for a request-declared server"
    );
    let meta = seen.meta.as_ref().unwrap();
    for field in ["traceparent", "tracestate", "baggage"] {
        assert!(
            meta.get(field).is_none(),
            "no {field} in _meta for a request-declared server: {meta}"
        );
    }
}
