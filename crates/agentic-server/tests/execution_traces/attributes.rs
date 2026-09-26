//! Shared export contract checked against real executor and transport traces.

use opentelemetry::Value;
use opentelemetry::trace::Status;
use opentelemetry_sdk::trace::SpanData;

const PRIVATE_VALUES: &[&str] = &[
    "the prompt text must never be exported",
    "private proxy prompt",
    "private websocket prompt",
    "private compaction input",
    "private-auth",
    "private-api-key",
    "private-query",
    "private-search-key",
    "private search result",
    "private upstream error",
    "private stream failure",
    "private proxy response bytes",
    "private delta",
    "private summary",
];

fn allowed_attributes(name: &str) -> &'static [&'static str] {
    match name {
        "agentic.execute" => &[
            "agentic.api",
            "agentic.route",
            "agentic.stream",
            "agentic.queue.wait",
            "agentic.execution.outcome",
            "agentic.delivery.outcome",
            "error.type",
        ],
        "agentic.rehydrate" => &["agentic.rehydrate.source"],
        "agentic.inference_round" => &["agentic.inference.round"],
        "agentic.tool.execute" => &["agentic.tool.type"],
        "agentic.compaction" => &["agentic.compaction.operation", "agentic.compaction.trigger"],
        "agentic.persist" => &["agentic.persist.destination"],
        // The HTTP span here is the core harness's parent stand-in.
        "agentic.websocket.session" | "http.server.request" => &[],
        "http.client.request" => &[
            "http.request.method",
            "server.address",
            "server.port",
            "http.response.status_code",
            "error.type",
        ],
        name if name.starts_with("POST /v1/") || name.starts_with("GET /v1/") => &[
            "http.request.method",
            "http.route",
            "http.response.status_code",
            "url.scheme",
            "network.protocol.version",
            "error.type",
        ],
        other => panic!("undeclared exported span: {other}"),
    }
}

fn enum_values(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "agentic.api" => &["responses", "messages"],
        "agentic.route" => &["executor", "proxy"],
        "agentic.execution.outcome" => &["completed", "incomplete", "failed", "cancelled"],
        "agentic.delivery.outcome" => &["delivered", "disconnected", "not_started"],
        "agentic.rehydrate.source" => &["none", "previous_response", "conversation"],
        "agentic.compaction.operation" => &["summarize"],
        "agentic.compaction.trigger" => &["context_management", "input_item", "explicit"],
        "agentic.persist.destination" => &["response", "conversation"],
        "agentic.tool.type" => &[
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
        "http.request.method" => &["POST", "GET"],
        "url.scheme" => &["http"],
        "network.protocol.version" => &["1.0", "1.1", "2", "3"],
        "error.type" => &[
            "storage",
            "persistence",
            "conversation_locked",
            "upstream_status",
            "upstream_transport",
            "upstream_error",
            "network",
            "parse",
            "stream",
            "not_found",
            "invalid_request",
            "payload_too_large",
            "resource_limit",
            "round_budget",
            "conflict",
            "compaction",
            "tool",
            "panic",
            "timeout",
            "transport",
            "handler_panic",
            "response_body",
            "cancelled",
        ],
        _ => return None,
    })
}

fn assert_value(key: &str, value: &Value) {
    if let Some(values) = enum_values(key) {
        let Value::String(value) = value else {
            panic!("{key} must be a bounded string enum")
        };
        assert!(values.contains(&value.as_str()), "unexpected {key} value: {value}");
        return;
    }
    match key {
        "agentic.stream" => assert!(matches!(value, Value::Bool(_))),
        "agentic.queue.wait" => assert!(matches!(value, Value::F64(wait) if wait.is_finite() && *wait >= 0.0)),
        "agentic.inference.round" => assert!(matches!(value, Value::I64(round) if *round >= 0)),
        "http.response.status_code" => assert!(matches!(value, Value::I64(status) if (100..=599).contains(status))),
        "server.port" => assert!(matches!(value, Value::I64(port) if (1..=65535).contains(port))),
        "server.address" => {
            let Value::String(address) = value else {
                panic!("server.address must be a host")
            };
            assert!(
                !address.as_str().contains(['/', '?', '#', '@']),
                "URL content leaked as server.address"
            );
        }
        "http.route" => {
            let Value::String(route) = value else {
                panic!("http.route must be a template")
            };
            assert!(
                !route.as_str().contains(['?', '#']),
                "query content leaked as http.route"
            );
        }
        other => panic!("missing value contract for {other}"),
    }
}

pub fn assert_allowed(spans: &[SpanData], fixture_values: &[&str]) {
    for span in spans {
        let allowed = allowed_attributes(&span.name);
        assert!(
            span.events.events.is_empty(),
            "log events must not enter exported spans"
        );
        assert!(
            span.links.links.iter().all(|link| link.attributes.is_empty()),
            "links contain context only"
        );
        if let Status::Error { description } = &span.status {
            assert!(description.is_empty(), "error messages must not be exported");
        }
        for attribute in &span.attributes {
            let key = attribute.key.as_str();
            assert!(allowed.contains(&key), "undeclared attribute {key} on {}", span.name);
            assert_value(key, &attribute.value);
            let value = attribute.value.to_string();
            for private in PRIVATE_VALUES.iter().chain(fixture_values) {
                assert!(!private.is_empty());
                assert!(!value.contains(*private), "private fixture value leaked in {key}");
            }
        }
    }
}
