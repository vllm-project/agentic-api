//! Request input is checked at the typed boundary, before execution or routing.

use agentic_core::executor::ExecutorError;
use agentic_core::types::io::{InOutItem, InputItem, ResponsesInput};
use agentic_core::types::request_response::{CompactRequest, RequestPayload};
use http::StatusCode;
use serde_json::json;

#[test]
fn unsupported_input_item_is_rejected_during_request_deserialization() {
    for store in [false, true] {
        for stream in [false, true] {
            for kind in [
                "computer_call",
                "image_generation_call",
                "file_search_call",
                "local_shell_call",
                "mcp_approval_response",
                "future_item",
                "unknown",
            ] {
                let error = serde_json::from_value::<RequestPayload>(json!({
                    "model": "synthetic",
                    "store": store,
                    "stream": stream,
                    "input": [
                        {"role": "user", "content": "hi"},
                        {"type": kind, "id": "unsupported_1", "role": "user", "content": "not a message"}
                    ]
                }))
                .expect_err("unmodeled input must fail before execution or proxy routing");

                let error = ExecutorError::from(error);
                assert_eq!(error.http_status(), StatusCode::BAD_REQUEST);
                assert_eq!(error.error_type(), "invalid_request_error");
            }
        }
    }
}

#[test]
fn compaction_rejects_unsupported_input_during_deserialization() {
    let error = serde_json::from_value::<CompactRequest>(json!({
        "model": "synthetic", "input": [{"type": "future_item"}]
    }))
    .expect_err("compaction must not silently discard unknown input");
    assert!(error.is_data());
}

#[test]
fn supported_input_keeps_its_payload_through_request_deserialization() {
    for input in [
        json!("hi"),
        json!([]),
        json!([
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "sunny"}
        ]),
    ] {
        let payload: RequestPayload = serde_json::from_value(json!({
            "model": "synthetic", "input": input
        }))
        .expect("supported input should parse");
        assert_eq!(serde_json::to_value(payload.input).unwrap(), input);
    }
}

#[test]
fn malformed_known_input_is_rejected() {
    let error = serde_json::from_value::<RequestPayload>(json!({
        "model": "synthetic",
        "input": [{"type": "function_call", "name": "weather", "role": "user", "content": "not a message"}]
    }))
    .expect_err("missing call fields must fail parsing");
    assert!(error.is_data());
}

#[test]
fn gateway_call_input_preserves_typed_payloads_in_canonical_history() {
    let wire = json!([
        {"type": "web_search_call", "id": "ws_1", "status": "completed",
         "action": {"type": "search", "query": "weather", "queries": ["weather"],
                    "sources": [{"url": "https://example.com", "title": "Forecast"}]}},
        {"type": "mcp_call", "id": "mcp_1", "server_label": "weather", "name": "forecast",
         "arguments": "{}", "status": "completed", "output": "sunny",
         "approval_request_id": null, "error": null}
    ]);
    let payload: RequestPayload = serde_json::from_value(json!({
        "model": "synthetic", "input": wire
    }))
    .expect("gateway calls are typed request input");
    assert_eq!(
        serde_json::to_value(Vec::<InputItem>::from(&payload.input)).unwrap(),
        wire
    );
    assert_eq!(serde_json::to_value(payload.input.model_input()).unwrap(), wire);
    for stream in [false, true] {
        let upstream = payload.to_upstream_request(stream).unwrap();
        assert_eq!(serde_json::to_value(&upstream.input).unwrap(), wire);
    }
    let items = payload.input.into_canonical_items();
    assert!(matches!(
        items.as_slice(),
        [
            InOutItem::Input(InputItem::WebSearchCall(_)),
            InOutItem::Input(InputItem::McpCall(_))
        ]
    ));
    assert_eq!(serde_json::to_value(ResponsesInput::Items(items)).unwrap(), wire);

    for kind in ["web_search_call", "mcp_call"] {
        assert!(serde_json::from_value::<InOutItem>(json!({"type": kind, "id": "missing_payload"})).is_err());
    }
}
