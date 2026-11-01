//! A gateway MCP `tools/call` to an operator-configured server carries the calling
//! span's trace context and baggage, both as HTTP headers and in `params._meta`
//! (#460). A server URL declared in a request gets neither: baggage may hold values
//! an operator's edge added, and that server may belong to anyone.
//!
//! One test per binary: the global subscriber and propagator are process-wide.

use std::sync::{Arc, Mutex};

use std::collections::HashMap;

use agentic_core::config::ToolRuntimeConfig;
use agentic_core::executor::ExecuteRequest;
use agentic_core::tool::GatewayExecutors;
use agentic_core::tool::{McpClientPool, McpServerEntry};
use agentic_core::types::tools::McpToolParam;
use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use either::Either;
use opentelemetry::propagation::TextMapCompositePropagator;
use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use serde_json::{Value, json};
use tracing::Instrument as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::layer::SubscriberExt as _;

mod support;

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
                    Some("tools/list") => {
                        json!({"tools":[{"name":"echo", "inputSchema":{"type":"object", "properties":{}}}]})
                    }
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

async fn assert_gateway_trace_policy(configured: bool, retained: bool) {
    let (url, seen) = spawn_recording_mcp().await;
    let mut responses = vec![
        support::function_call_response("fc_echo", "call_echo", "mcp__tools__echo", "{}"),
        support::text_response("Finished."),
    ];
    if retained {
        responses.insert(0, support::text_response("Ready."));
    }
    let mut fixture = support::TestFixture::new_with_responses(responses).await;
    let mut body = json!({"model":"test", "store":false, "input":"Use echo.", "tools":[{
        "type":"mcp", "server_label":"tools", "server_url":url, "allowed_tools":["echo"], "require_approval":"never"
    }]});
    if configured {
        let config = ToolRuntimeConfig {
            mcp_servers: HashMap::from([(
                "tools".to_owned(),
                McpServerEntry::Http {
                    url,
                    headers: None,
                    allowed_tools: None,
                    require_approval: Some("never".to_owned()),
                    description: None,
                    defer_loading: None,
                },
            )]),
            ..Default::default()
        };
        Arc::get_mut(&mut fixture.exec_ctx).unwrap().gateway_executors =
            GatewayExecutors::from_config(Arc::new(reqwest::Client::new()), &config).unwrap();
        body["tools"][0].as_object_mut().unwrap().remove("server_url");
    }
    if retained {
        let Either::Left(first) =
            ExecuteRequest::new(serde_json::from_value(body.clone()).unwrap(), fixture.exec_ctx.clone())
                .run()
                .await
                .unwrap()
        else {
            panic!("JSON response")
        };
        let output = serde_json::to_value(first.output).unwrap();
        assert!(
            output
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "mcp_list_tools")
        );
        let mut history = vec![json!({"role":"user", "content":"Use echo."})];
        history.extend(output.as_array().unwrap().iter().cloned());
        history.push(json!({"role":"user", "content":"Use echo now."}));
        body["input"] = json!(history);
    }
    let root = tracing::info_span!("test.gateway");
    let inbound = HashMap::from([("baggage".to_owned(), "tenant=acme".to_owned())]);
    let parent = opentelemetry::global::get_text_map_propagator(|propagator| propagator.extract(&inbound));
    let _ = root.set_parent(parent);
    let Either::Left(response) = ExecuteRequest::new(serde_json::from_value(body).unwrap(), fixture.exec_ctx.clone())
        .run()
        .instrument(root)
        .await
        .unwrap()
    else {
        panic!("JSON response")
    };
    assert_eq!(response.status, "completed");
    let seen = seen.lock().unwrap();
    let headers = seen.headers.as_ref().expect("gateway executed MCP call");
    let meta = seen.meta.as_ref().unwrap();
    for field in ["traceparent", "baggage"] {
        assert_eq!(
            headers.contains_key(field),
            configured,
            "{field}: configured={configured}, retained={retained}"
        );
        assert_eq!(
            meta.get(field).is_some(),
            configured,
            "{field} metadata: configured={configured}, retained={retained}"
        );
    }
    if configured {
        assert_eq!(headers["baggage"], "tenant=acme");
        assert_eq!(meta["baggage"], "tenant=acme");
    }
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
    {
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
    for configured in [false, true] {
        for retained in [false, true] {
            assert_gateway_trace_policy(configured, retained).await;
        }
    }
}
