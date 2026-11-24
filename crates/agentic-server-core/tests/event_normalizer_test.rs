use agentic_core::events::{EventFrame, SSEEventType, WireEvent};

#[path = "event_normalizer/cassette_flows.rs"]
mod cassette_flows;
#[path = "event_normalizer/reasoning_events.rs"]
mod reasoning_events;
#[path = "event_normalizer/tool_call_events.rs"]
mod tool_call_events;
#[path = "event_normalizer/wire_events.rs"]
mod wire_events;

#[test]
fn typed_event_conversion_uses_existing_normalization_and_json_errors() {
    let mut wire = WireEvent::new("response.completed");
    wire.rest
        .insert("response".to_owned(), serde_json::json!({"status":"incomplete"}));
    let frame = EventFrame::try_from(wire).unwrap();
    assert_eq!(frame.event_type, SSEEventType::ResponseIncomplete);
    assert_eq!(frame.wire.event_type.as_deref(), Some("response.incomplete"));

    let mut wire = WireEvent::new("response.output_text.delta");
    wire.output_index = Some(u64::from(u32::MAX) + 1);
    let error = EventFrame::try_from(wire).unwrap_err();
    assert!(error.to_string().contains("output_index"));
    let error = agentic_core::executor::ExecutorError::from(error);
    assert!(matches!(error, agentic_core::executor::ExecutorError::JsonError(_)));
}
