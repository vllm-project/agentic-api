use agentic_core::events::{EventPayload, SSEEventType, normalize_sse_line};
use agentic_core::executor::{RequestContext, UpstreamBody, decode_upstream};
use agentic_core::types::request_response::RequestPayload;
use serde_json::{Value, json};

fn context() -> RequestContext {
    let request: RequestPayload = serde_json::from_value(json!({"model":"test", "input":"hi"})).unwrap();
    RequestContext {
        original_request: request.clone(),
        enriched_request: request,
        new_input_items: Vec::new(),
        response_id: "resp_local".to_owned(),
        conversation_id: None,
        conversation_version: None,
        continuation: None,
    }
}

fn stream(terminal: &Value) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: {terminal}\n\ndata: [DONE]\n\n",
        json!({"type":"response.created", "response":{"id":"resp_upstream", "status":"in_progress"}}),
        json!({"type":"response.in_progress", "response":{"id":"resp_upstream", "status":"in_progress"}})
    )
}

#[test]
fn incomplete_completion_normalizes_classification_and_wire_type() {
    for event_type in ["response.completed", "response.done"] {
        for details in [
            json!({"reason":"max_output_tokens"}),
            json!({"reason":"content_filter"}),
            Value::Null,
        ] {
            let event = json!({"type":event_type, "sequence_number":17, "provider_extension":"雪",
                "response":{"id":"resp_upstream", "status":"incomplete", "incomplete_details":details,
                    "output":[], "usage":{"input_tokens":3,"output_tokens":5,"total_tokens":8}}});
            for separator in ["", " "] {
                let frame = normalize_sse_line(&format!("data:{separator}{event}")).unwrap();
                assert_eq!(frame.event_type, SSEEventType::ResponseIncomplete);
                assert!(matches!(&frame.payload, EventPayload::Response { status, .. } if status == "incomplete"));
                let mut expected = event.clone();
                expected["type"] = json!("response.incomplete");
                assert_eq!(serde_json::to_value(frame.wire).unwrap(), expected);
            }
        }
    }
}

#[test]
fn normalization_does_not_infer_incomplete_from_usage_or_details() {
    for event_type in [
        "response.created",
        "response.in_progress",
        "response.completed",
        "response.done",
        "response.failed",
        "response.incomplete",
        "provider.unknown",
    ] {
        for status in [
            json!("completed"),
            json!("failed"),
            json!("in_progress"),
            json!("incomplete"),
            json!("unknown"),
            Value::Null,
            json!(17),
            json!("Incomplete"),
            json!("incomplete "),
        ] {
            if status == "incomplete" && matches!(event_type, "response.completed" | "response.done") {
                continue;
            }
            let event = json!({"type":event_type, "response":{"id":"resp_upstream", "status":status,
                "max_output_tokens":5, "usage":{"input_tokens":3,"output_tokens":5,"total_tokens":8},
                "incomplete_details":{"reason":"max_output_tokens"}}});
            let frame = normalize_sse_line(&format!("data: {event}")).unwrap();
            assert_eq!(frame.event_type, SSEEventType::from(event_type));
            assert_eq!(serde_json::to_value(frame.wire).unwrap(), event);
        }
    }
    for response in [Value::Null, json!(17), json!([]), json!({})] {
        let event = json!({"type":"response.completed", "response":response});
        let frame = normalize_sse_line(&format!("data: {event}")).unwrap();
        assert_eq!(frame.event_type, SSEEventType::ResponseCompleted);
        assert_eq!(serde_json::to_value(frame.wire).unwrap(), event);
    }
}

#[tokio::test]
async fn strict_decode_preserves_incomplete_completion_like_json() {
    for event_type in ["response.completed", "response.done"] {
        assert_incomplete_parity(event_type).await;
    }
}

#[tokio::test]
async fn strict_decode_preserves_canonical_incomplete() {
    assert_incomplete_parity("response.incomplete").await;
}

async fn assert_incomplete_parity(event_type: &str) {
    let response = json!({"id":"resp_upstream", "status":"incomplete", "output":[],
            "usage":{"input_tokens":3,"output_tokens":5,"total_tokens":8},
            "incomplete_details":{"reason":"max_output_tokens"}});
    let sse = stream(&json!({"type":event_type, "response":response}));
    let (decoded, _) = decode_upstream(context(), UpstreamBody::Sse(&sse))
        .await
        .expect("supported incomplete terminal");
    let (control, _) = decode_upstream(context(), UpstreamBody::Json(&response.to_string()))
        .await
        .unwrap();
    let mut decoded = serde_json::to_value(decoded).unwrap();
    let mut control = serde_json::to_value(control).unwrap();
    // Each decoding operation assigns its own local creation time.
    decoded.as_object_mut().unwrap().remove("created_at");
    control.as_object_mut().unwrap().remove("created_at");
    assert_eq!(decoded, control);
}

#[tokio::test]
async fn strict_decode_still_rejects_other_mismatches_and_invalid_lifecycles() {
    for (event_type, status) in [
        ("response.completed", "failed"),
        ("response.completed", "in_progress"),
        ("response.incomplete", "completed"),
        ("response.failed", "incomplete"),
    ] {
        let sse = stream(&json!({"type":event_type, "response":{"id":"resp_upstream","status":status,"output":[]}}));
        let error = decode_upstream(context(), UpstreamBody::Sse(&sse)).await.unwrap_err();
        assert!(error.to_string().contains("expected"), "{error}");
    }
    let terminal =
        json!({"type":"response.incomplete", "response":{"id":"resp_upstream", "status":"incomplete", "output":[]}});
    let valid = stream(&terminal);
    for (sse, diagnostic) in [
        (format!("data: {terminal}\n"), "out of lifecycle order"),
        (format!("{valid}data: {terminal}\n"), "after its terminal event"),
    ] {
        let error = decode_upstream(context(), UpstreamBody::Sse(&sse)).await.unwrap_err();
        assert!(error.to_string().contains(diagnostic), "{error}");
    }
    for (response, diagnostic) in [
        (json!({"status":"incomplete","output":[]}), "no valid 'id'"),
        (
            json!({"id":"other","status":"incomplete","output":[]}),
            "changes the response id",
        ),
    ] {
        let sse = stream(&json!({"type":"response.incomplete", "response":response}));
        let error = decode_upstream(context(), UpstreamBody::Sse(&sse)).await.unwrap_err();
        assert!(error.to_string().contains(diagnostic), "{error}");
    }
}
