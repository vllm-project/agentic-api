use super::slot::Slot;
use super::*;
use crate::events::WireEvent;
use crate::executor::pipeline::RoundIngestion;
use crate::executor::translate::TranslationContext;
use crate::executor::translate::TranslationDispatcher;
use crate::types::event::MessageStatus;
use crate::types::io::{McpCallError, McpCallStatus, WebSearchCallStatus};

fn from_sse_lines(lines: impl IntoIterator<Item = String>, conversation_id: Option<&str>) -> ResponseAccumulator {
    ResponseAccumulator::from_sse_lines(lines, conversation_id).expect("valid SSE stream")
}

fn completed_item_late_event_cases() -> Vec<(serde_json::Value, Vec<serde_json::Value>)> {
    use serde_json::json;

    vec![
        (
            json!({"type":"message","id":"item_1","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"complete","annotations":[]}]}),
            vec![
                json!({"type":"response.output_text.delta","delta":"late","content_index":0}),
                json!({"type":"response.output_text.done","text":"late","content_index":0}),
                json!({"type":"response.content_part.added","part":{"type":"output_text","text":"late"},"content_index":0}),
            ],
        ),
        (
            json!({"type":"function_call","id":"item_1","call_id":"call_1","name":"lookup","arguments":"{}","status":"completed"}),
            vec![
                json!({"type":"response.function_call_arguments.delta","delta":"late"}),
                json!({"type":"response.function_call_arguments.done","arguments":"late"}),
            ],
        ),
        (
            json!({"type":"custom_tool_call","id":"item_1","call_id":"call_1","name":"lookup","input":"complete","status":"completed"}),
            vec![
                json!({"type":"response.custom_tool_call_input.delta","delta":"late"}),
                json!({"type":"response.custom_tool_call_input.done","input":"late"}),
            ],
        ),
        (
            json!({"type":"reasoning","id":"item_1","summary":[],"content":[],"status":"completed"}),
            vec![
                json!({"type":"response.reasoning_text.done","text":"late","content_index":0}),
                json!({"type":"response.reasoning_summary_text.delta","delta":"late","summary_index":0}),
            ],
        ),
        (
            json!({"type":"mcp_call","id":"item_1","server_label":"counter","name":"increment","arguments":"{}","output":"1","status":"completed"}),
            vec![json!({"type":"response.mcp_call_arguments.delta","delta":"late"})],
        ),
        (
            json!({"type":"web_search_call","id":"item_1","status":"completed","action":{"type":"search","query":"rust"}}),
            vec![json!({"type":"response.web_search_call.searching"})],
        ),
        (
            json!({"type":"mcp_list_tools","id":"item_1","server_label":"counter","tools":[]}),
            vec![json!({"type":"response.mcp_list_tools.in_progress"})],
        ),
    ]
}

#[test]
fn completed_items_reject_late_events_strictly_and_suppress_lenient_translation() {
    use serde_json::json;

    for (item, events) in completed_item_late_event_cases() {
        for strict in [false, true] {
            let validation = if strict {
                Validation::Strict
            } else {
                Validation::Lenient
            };
            let mut acc = ResponseAccumulator::with_validation("resp_1".to_owned(), None, validation);
            let context = TranslationContext::default();
            let mut translator = TranslationDispatcher::new(context);
            for event in [
                json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}),
                json!({"type":"response.in_progress","response":{"id":"resp_1","status":"in_progress"}}),
                json!({"type":"response.output_item.added","output_index":0,"item":item}),
                json!({"type":"response.output_item.done","output_index":0,"item":item}),
            ] {
                let line = format!("data: {event}");
                RoundIngestion::translate_line(&mut acc, SseLine::parse(&line), &mut translator)
                    .expect("valid setup event");
            }
            for mut event in events.clone() {
                event["output_index"] = json!(0);
                event["item_id"] = json!("item_1");
                let line = format!("data: {event}");
                if strict {
                    let error = RoundIngestion::translate_line(&mut acc, SseLine::parse(&line), &mut translator)
                        .expect_err("completed item is closed");
                    assert!(error.to_string().contains("no active output item"), "{error}");
                } else {
                    assert!(
                        RoundIngestion::translate_line(&mut acc, SseLine::parse(&line), &mut translator)
                            .expect("late event is ignored")
                            .is_none(),
                        "late event reached translation: {event}"
                    );
                }
            }
            acc.finalize_all();
            let expected: OutputItem = serde_json::from_value(item.clone()).expect("valid completed item");
            assert_eq!(serde_json::to_value(&acc.output).unwrap(), json!([expected]));
        }
    }
}

#[test]
fn test_accumulator_new() {
    let acc = ResponseAccumulator::new("resp_123".into(), Some("conv_456".into()));
    assert_eq!(acc.response_id, "resp_123");
    assert_eq!(acc.conversation_id, Some("conv_456".into()));
    assert_eq!(acc.status, ResponseStatus::InProgress);
}

fn push_lifecycle_event(
    acc: &mut ResponseAccumulator,
    event: &serde_json::Value,
    strict: bool,
) -> ExecutorResult<Option<EventFrame>> {
    let line = format!("data: {event}");
    assert_eq!(acc.validation == Validation::Strict, strict);
    acc.process_line(SseLine::parse(&line))
}

fn lifecycle_accumulator(strict: bool) -> ResponseAccumulator {
    let validation = if strict {
        Validation::Strict
    } else {
        Validation::Lenient
    };
    let mut acc = ResponseAccumulator::with_validation("resp_1".to_owned(), None, validation);
    for event_type in ["response.created", "response.in_progress"] {
        push_lifecycle_event(
            &mut acc,
            &serde_json::json!({"type":event_type,"response":{"id":"resp_1","status":"in_progress"}}),
            strict,
        )
        .expect("valid response lifecycle");
    }
    acc
}

#[test]
fn completed_slots_cannot_be_reopened_by_id_or_index() {
    use serde_json::json;

    for strict in [false, true] {
        for (item_id, output_index) in [("item_1", 1), ("item_2", 0), ("item_1", 0)] {
            let mut acc = lifecycle_accumulator(strict);
            let item = json!({"type":"function_call","id":"item_1","call_id":"call_1",
                "name":"lookup","arguments":"{}","status":"completed"});
            // Exercise retained done-only slots as well as strict added/done slots.
            if strict {
                push_lifecycle_event(
                    &mut acc,
                    &json!({"type":"response.output_item.added","output_index":0,"item":item}),
                    strict,
                )
                .expect("valid added item");
            }
            push_lifecycle_event(
                &mut acc,
                &json!({"type":"response.output_item.done","output_index":0,"item":item}),
                strict,
            )
            .expect("valid completion");
            let mut reopened = item.clone();
            reopened["id"] = json!(item_id);
            push_lifecycle_event(
                &mut acc,
                &json!({"type":"response.output_item.added","output_index":output_index,"item":reopened}),
                strict,
            )
            .expect_err("completed identity remains reserved");
            acc.finalize_all();
            assert_eq!(serde_json::to_value(acc.output).unwrap(), json!([item]));
        }
    }
}

#[test]
fn repeated_completion_preserves_field_fallbacks_and_rejects_strict_duplicates() {
    use serde_json::json;

    let cases = [
        (
            json!({"type":"function_call","id":"item_1","call_id":"call_1","name":"lookup","arguments":"{}","status":"completed"}),
            json!({"type":"function_call","id":"item_1","call_id":"","name":"","arguments":"","status":"completed"}),
        ),
        (
            json!({"type":"custom_tool_call","id":"item_1","call_id":"call_1","name":"lookup","input":"complete","status":"completed"}),
            json!({"type":"custom_tool_call","id":"item_1","call_id":"","name":"","input":"","status":"completed"}),
        ),
        (
            json!({"type":"reasoning","id":"item_1","content":[{"type":"reasoning_text","text":"complete"}],"summary":[],"status":"completed"}),
            json!({"type":"reasoning","id":"item_1","status":"completed"}),
        ),
    ];
    for (item, repeated) in cases {
        for strict in [false, true] {
            let mut acc = lifecycle_accumulator(strict);
            for event_type in ["response.output_item.added", "response.output_item.done"] {
                push_lifecycle_event(
                    &mut acc,
                    &json!({"type":event_type,"output_index":0,"item":item}),
                    strict,
                )
                .expect("valid item lifecycle");
            }
            let result = push_lifecycle_event(
                &mut acc,
                &json!({"type":"response.output_item.done","output_index":0,"item":repeated}),
                strict,
            );
            if strict {
                assert!(result.is_err(), "strict ingestion rejects every repeated completion");
            } else {
                assert!(result.expect("equivalent resolved completion").is_none());
            }
            acc.finalize_all();
            let expected: OutputItem = serde_json::from_value(item.clone()).unwrap();
            assert_eq!(serde_json::to_value(acc.output).unwrap(), json!([expected]));
        }
    }
}

#[test]
fn identical_done_only_calls_preserve_provider_status() {
    use serde_json::json;

    for status in ["in_progress", "completed"] {
        let mut acc = lifecycle_accumulator(false);
        let item = json!({"type":"function_call","id":"item_1","call_id":"call_1",
            "name":"lookup","arguments":"{}","status":status});
        let event = json!({"type":"response.output_item.done","output_index":0,"item":item});
        push_lifecycle_event(&mut acc, &event, false).expect("valid done-only call");
        assert!(
            push_lifecycle_event(&mut acc, &event, false)
                .expect("identical duplicate")
                .is_none()
        );
        acc.finalize_all();
        assert_eq!(serde_json::to_value(acc.output).unwrap(), json!([item]));
    }
}

#[test]
fn strict_terminal_requires_all_retained_slots_done_and_preserves_index_order() {
    use serde_json::json;

    let mut acc = lifecycle_accumulator(true);
    let item = |index| {
        json!({"type":"function_call","id":format!("fc_{index}"),"call_id":format!("call_{index}"),
        "name":"lookup","arguments":"{}","status":"completed"})
    };
    for index in [1, 0] {
        push_lifecycle_event(
            &mut acc,
            &json!({"type":"response.output_item.added","output_index":index,"item":item(index)}),
            true,
        )
        .expect("valid added item");
    }
    let done = |index| json!({"type":"response.output_item.done","output_index":index,"item":item(index)});
    push_lifecycle_event(&mut acc, &done(1), true).expect("first completion");
    let terminal =
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[item(0),item(1)]}});
    let error = push_lifecycle_event(&mut acc, &terminal, true).expect_err("one slot is still active");
    assert!(error.to_string().contains("unfinished output items"));
    push_lifecycle_event(&mut acc, &done(0), true).expect("second completion");
    push_lifecycle_event(&mut acc, &terminal, true).expect("all slots are now done");
    acc.finish_strict_stream().expect("valid terminal state");
    assert_eq!(serde_json::to_value(acc.output).unwrap(), json!([item(0), item(1)]));
}

#[test]
fn retained_slot_debug_omits_completed_payloads() {
    use serde_json::json;

    let mut acc = lifecycle_accumulator(false);
    let item = json!({"type":"function_call","id":"item_1","call_id":"call_1",
        "name":"lookup","arguments":"private-argument-marker","status":"completed"});
    for event_type in ["response.output_item.added", "response.output_item.done"] {
        push_lifecycle_event(
            &mut acc,
            &json!({"type":event_type,"output_index":0,"item":item}),
            false,
        )
        .expect("valid item lifecycle");
        assert!(!format!("{acc:?}").contains("private-argument-marker"));
    }
}

#[test]
fn test_accumulator_mark_incomplete() {
    let mut acc = ResponseAccumulator::new("resp_123".into(), None);
    acc.mark_incomplete("Stream interrupted");
    assert_eq!(acc.status, ResponseStatus::Incomplete);
    assert!(acc.incomplete_details.is_some());
}

#[test]
fn test_accumulator_preserves_streamed_failure_details() {
    let acc = from_sse_lines(
        [r#"data: {"type":"response.failed","response":{"id":"resp_failed","status":"failed","error":{"code":"tool_catalog_too_large","message":"Too many tools"},"incomplete_details":{"reason":"upstream_error"}}}"#.to_owned()],
        None,
    );
    let payload = acc.finalize("test-model", None, None);

    assert_eq!(payload.status, "error");
    assert_eq!(payload.error.as_ref().unwrap()["code"], "tool_catalog_too_large");
    assert_eq!(
        payload.incomplete_details.unwrap().reason.as_deref(),
        Some("upstream_error")
    );
}

#[test]
fn test_accumulator_finalize() {
    let acc = ResponseAccumulator::new("resp_123".into(), Some("conv_456".into()));
    let payload = acc.finalize("gpt-4o", Some("resp_prev"), Some("be helpful"));
    assert_eq!(payload.id, "resp_123");
    assert_eq!(payload.model, "gpt-4o");
    assert_eq!(payload.conversation_id, Some("conv_456".into()));
    assert_eq!(payload.previous_response_id, Some("resp_prev".into()));
    assert_eq!(payload.instructions, Some("be helpful".into()));
    assert_eq!(payload.status, ResponseStatus::InProgress.as_str());
}

#[test]
fn test_accumulator_from_sse_lines_empty() {
    let acc = from_sse_lines(vec![], None);
    assert_eq!(acc.status, ResponseStatus::InProgress);
    assert!(acc.output.is_empty());
}

#[test]
fn test_accumulator_text_delta_assigned_to_message() {
    let lines = vec![
        r#"data: {"type":"response.created","response":{"id":"resp_abc"}}"#.to_string(),
        r#"data: {"type":"response.output_item.added","item":{"id":"msg_1"}}"#.to_string(),
        r#"data: {"type":"response.output_text.delta","delta":"Hello","item_id":"msg_1"}"#.to_string(),
        r#"data: {"type":"response.output_text.delta","delta":" world","item_id":"msg_1"}"#.to_string(),
        r#"data: {"type":"response.done","response":{"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}"#
            .to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.status, ResponseStatus::Completed);
    assert_eq!(acc.output.len(), 1);

    if let OutputItem::Message(msg) = &acc.output[0] {
        assert_eq!(msg.content.len(), 1);
        assert_eq!(msg.content[0].text, "Hello world");
    } else {
        panic!("expected OutputItem::Message");
    }

    assert!(acc.usage.is_some());
    let usage = acc.usage.unwrap();
    assert_eq!(usage.total_tokens, 7);
}

#[test]
fn test_message_status_enum() {
    assert_eq!(MessageStatus::Completed.as_str(), "completed");
    assert_eq!(MessageStatus::InProgress.as_str(), "in_progress");
}

#[test]
fn test_process_event_response_created_sets_id() {
    let mut acc = ResponseAccumulator::new("resp_old".into(), None);
    let frame = EventFrame {
        event_type: SSEEventType::ResponseCreated,
        payload: EventPayload::Response {
            id: "resp_new".into(),
            status: "in_progress".into(),
            usage: None,
        },
        wire: WireEvent::new("test"),
    };
    acc.process_event(&frame);
    assert_eq!(acc.response_id, "resp_new");
}

#[test]
fn test_process_event_response_created_empty_id_no_overwrite() {
    let mut acc = ResponseAccumulator::new("resp_keep".into(), None);
    let frame = EventFrame {
        event_type: SSEEventType::ResponseCreated,
        payload: EventPayload::Response {
            id: String::new(),
            status: "in_progress".into(),
            usage: None,
        },
        wire: WireEvent::new("test"),
    };
    acc.process_event(&frame);
    assert_eq!(acc.response_id, "resp_keep");
}

#[test]
fn test_process_event_text_delta_accumulates() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);

    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputItemAdded,
        payload: EventPayload::OutputItemAdded {
            shell_call: None,
            item_id: "msg_1".into(),
            item_type: "message".into(),
            output_index: Some(0),
            name: None,
            namespace: None,
            call_id: None,
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputTextDelta,
        payload: EventPayload::TextDelta {
            delta: "Hello".into(),
            item_id: "msg_1".into(),
            output_index: Some(0),
            content_index: 0,
        },
        wire: WireEvent::new("test"),
    });
    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputTextDelta,
        payload: EventPayload::TextDelta {
            delta: " world".into(),
            item_id: "msg_1".into(),
            output_index: Some(0),
            content_index: 0,
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::ResponseCompleted,
        payload: EventPayload::Response {
            id: "resp_1".into(),
            status: "completed".into(),
            usage: None,
        },
        wire: WireEvent::new("test"),
    });

    assert_eq!(acc.status, ResponseStatus::Completed);
    assert_eq!(acc.output.len(), 1);
    if let OutputItem::Message(msg) = &acc.output[0] {
        assert_eq!(msg.content[0].text, "Hello world");
    } else {
        panic!("expected Message");
    }
}

#[test]
fn test_process_event_mcp_call_done_accumulates_output() {
    let lines = vec![
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"mcp_call","id":"mcp_1","server_label":"counter","name":"increment","arguments":"","status":"in_progress","approval_request_id":null,"output":null,"error":null}}"#.to_string(),
        r#"data: {"type":"response.mcp_call.in_progress","item_id":"mcp_1","output_index":0}"#.to_string(),
        r#"data: {"type":"response.mcp_call_arguments.delta","delta":"{}","item_id":"mcp_1","output_index":0}"#.to_string(),
        r#"data: {"type":"response.mcp_call_arguments.done","arguments":"{}","item_id":"mcp_1","output_index":0}"#.to_string(),
        r#"data: {"type":"response.mcp_call.completed","item_id":"mcp_1","output_index":0}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"mcp_call","id":"mcp_1","server_label":"counter","name":"increment","arguments":"{}","status":"completed","approval_request_id":null,"output":"1","error":null}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.status, ResponseStatus::Completed);
    assert_eq!(acc.output.len(), 1);
    assert!(matches!(acc.output[0], OutputItem::McpCall(_)));
}

#[test]
fn test_process_event_mcp_list_tools_done_accumulates_output() {
    let added = r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"mcp_list_tools","id":"mcpl_1","server_label":"counter","tools":[]}}"#;
    let remaining = [
        r#"data: {"type":"response.mcp_list_tools.in_progress","item_id":"mcpl_1","output_index":0}"#.to_string(),
        r#"data: {"type":"response.mcp_list_tools.completed","item_id":"mcpl_1","output_index":0}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"mcp_list_tools","id":"mcpl_1","server_label":"counter","tools":[{"name":"increment","description":"Increment the counter","input_schema":{"type":"object","properties":{}},"annotations":{"read_only":false}}]}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_1","status":"completed"}}"#.to_string(),
    ];

    let mut acc = ResponseAccumulator::new("resp_1".to_owned(), None);
    let _ = acc.process_line(SseLine::parse(added)).expect("valid SSE event");
    let Some(Slot {
        state: SlotState::Active(ActiveItem::McpListTools { item }),
        ..
    }) = acc.slots.get(OutputIndex::new(0))
    else {
        panic!("expected in-flight mcp_list_tools");
    };
    assert!(item.server_label.is_empty());
    assert!(item.tools.is_empty());

    for line in remaining {
        let _ = acc.process_line(SseLine::parse(&line)).expect("valid SSE event");
    }
    acc.finalize_all();

    assert_eq!(acc.status, ResponseStatus::Completed);
    assert_eq!(acc.output.len(), 1);
    let OutputItem::McpListTools(item) = &acc.output[0] else {
        panic!("expected mcp_list_tools");
    };
    assert_eq!(item.id, "mcpl_1");
    assert_eq!(item.server_label, "counter");
    assert_eq!(item.tools.len(), 1);
    assert_eq!(item.tools[0].name, "increment");
    assert_eq!(item.tools[0].annotations, Some(serde_json::json!({"read_only": false})));
}

#[test]
fn compaction_added_and_done_accumulate_typed_output() {
    let done = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"compaction","id":"cmp_1","encrypted_content":"durable summary"}}"#;
    let lines = [
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"compaction","id":"cmp_1","encrypted_content":"durable summary"}}"#.to_owned(),
        done.to_owned(),
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#.to_owned(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_compaction_output(&acc.output);

    let done_only = from_sse_lines([done.to_owned()], None);
    assert_compaction_output(&done_only.output);
}

fn assert_compaction_output(output: &[OutputItem]) {
    assert_eq!(output.len(), 1);
    let OutputItem::Compaction(item) = &output[0] else {
        panic!("expected compaction output");
    };
    assert_eq!(item.id.as_deref(), Some("cmp_1"));
    assert_eq!(item.encrypted_content, "durable summary");
}

#[test]
fn test_accumulator_reasoning_before_mcp_call_preserves_order() {
    let lines = vec![
        r#"data: {"type":"response.created","response":{"id":"resp_abc"}}"#.to_string(),
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}"#.to_string(),
        r#"data: {"type":"response.reasoning_text.done","text":"thinking...","item_id":"rs_1"}"#.to_string(),
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"mcp_call","id":"mcp_1","server_label":"counter","name":"increment","arguments":"","status":"in_progress","approval_request_id":null,"output":null,"error":null}}"#.to_string(),
        r#"data: {"type":"response.mcp_call.completed","item_id":"mcp_1","output_index":1}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"mcp_call","id":"mcp_1","server_label":"counter","name":"increment","arguments":"{}","status":"completed","approval_request_id":null,"output":"1","error":null}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_abc","status":"completed","usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 2);
    assert!(matches!(acc.output[0], OutputItem::Reasoning(_)));
    assert!(matches!(acc.output[1], OutputItem::McpCall(_)));
}

#[test]
fn test_accumulator_reasoning_before_done_only_mcp_call_preserves_order() {
    let lines = vec![
        r#"data: {"type":"response.created","response":{"id":"resp_abc"}}"#.to_string(),
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}"#.to_string(),
        r#"data: {"type":"response.reasoning_text.done","text":"thinking...","item_id":"rs_1"}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"mcp_call","id":"mcp_1","server_label":"counter","name":"increment","arguments":"{}","status":"completed","approval_request_id":null,"output":"1","error":null}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_abc","status":"completed","usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 2);
    assert!(matches!(acc.output[0], OutputItem::Reasoning(_)));
    assert!(matches!(acc.output[1], OutputItem::McpCall(_)));
}

#[test]
fn test_accumulator_reasoning_before_web_search_call_preserves_order() {
    let lines = vec![
        r#"data: {"type":"response.created","response":{"id":"resp_abc"}}"#.to_string(),
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}"#.to_string(),
        r#"data: {"type":"response.reasoning_text.done","text":"thinking...","item_id":"rs_1"}"#.to_string(),
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress","action":{"type":"search","query":"","sources":[]}}}"#.to_string(),
        r#"data: {"type":"response.web_search_call.in_progress","item_id":"ws_1","output_index":1}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"rust","sources":[]}}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_abc","status":"completed","usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 2);
    assert!(matches!(acc.output[0], OutputItem::Reasoning(_)));
    let OutputItem::WebSearchCall(call) = &acc.output[1] else {
        panic!("expected web_search_call");
    };
    assert_eq!(call.status, WebSearchCallStatus::Completed);
    assert_eq!(call.action.as_search().unwrap().query, "rust");
}

#[test]
fn test_accumulator_preserves_open_page_web_search_action() {
    let lines = vec![
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress"}}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"open_page","url":"https://example.com"}}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_1","status":"completed"}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 1);
    let action = match &acc.output[0] {
        OutputItem::WebSearchCall(call) => serde_json::to_value(&call.action).unwrap(),
        _ => panic!("expected web_search_call"),
    };
    assert_eq!(action["type"], "open_page");
    assert_eq!(action["url"], "https://example.com");
}

#[test]
fn test_accumulator_preserves_find_in_page_web_search_action() {
    let lines = vec![
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress"}}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"find_in_page","url":"https://example.com","pattern":"needle"}}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_1","status":"completed"}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 1);
    let action = match &acc.output[0] {
        OutputItem::WebSearchCall(call) => serde_json::to_value(&call.action).unwrap(),
        _ => panic!("expected web_search_call"),
    };
    assert_eq!(action["type"], "find_in_page");
    assert_eq!(action["url"], "https://example.com");
    assert_eq!(action["pattern"], "needle");
}

#[test]
fn test_accumulator_drops_unfinished_web_search_placeholder() {
    let lines = vec![
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress"}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_1","status":"completed"}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert!(acc.output.is_empty());
}

#[test]
fn test_accumulator_empty_added_id_then_stable_done_does_not_duplicate() {
    let lines = vec![
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"","status":"in_progress"}}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"rust","sources":[]}}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_1","status":"completed"}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 1);
    let OutputItem::WebSearchCall(call) = &acc.output[0] else {
        panic!("expected web_search_call");
    };
    assert_eq!(call.id, "ws_1");
}

#[test]
fn test_accumulator_stable_added_id_survives_empty_done_id() {
    let lines = vec![
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_added","status":"in_progress"}}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"","status":"completed","action":{"type":"search","query":"rust","sources":[]}}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_1","status":"completed"}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 1);
    let OutputItem::WebSearchCall(call) = &acc.output[0] else {
        panic!("expected web_search_call");
    };
    assert_eq!(call.id, "ws_added");
}

#[test]
fn test_accumulator_uses_authoritative_done_item_with_item_id_fallback() {
    let lines = vec![
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}}"#.to_string(),
        r#"data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_1","delta":"partial"}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":null,"item_id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"authoritative","annotations":[]}]}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_1","status":"completed"}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 1);
    let OutputItem::Message(message) = &acc.output[0] else {
        panic!("expected message");
    };
    assert_eq!(message.id, "msg_1");
    assert_eq!(message.content.len(), 1);
    assert_eq!(message.content[0].text, "authoritative");
}

#[test]
fn repeated_function_call_done_does_not_duplicate_lenient_output() {
    let added = r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":""}}"#;
    let done = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{}","status":"completed"}}"#;
    let terminal = r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#;

    let acc = from_sse_lines([added, done, done, terminal].map(str::to_owned), None);

    assert_eq!(acc.output.len(), 1);
    let OutputItem::FunctionCall(call) = &acc.output[0] else {
        panic!("expected function call");
    };
    assert_eq!(call.id, "fc_1");
    assert_eq!(call.call_id, "call_1");
    assert_eq!(call.arguments, "{}");
}

#[test]
fn repeated_identical_done_is_not_emitted_by_lenient_translation() {
    let added = r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":""}}"#;
    let done = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{}","status":"completed"}}"#;
    let mut acc = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = TranslationContext::default();
    let mut translator = TranslationDispatcher::new(context);

    RoundIngestion::translate_line(&mut acc, SseLine::parse(added), &mut translator).expect("added item is valid");
    RoundIngestion::translate_line(&mut acc, SseLine::parse(done), &mut translator).expect("first done item is valid");
    let repeated = RoundIngestion::translate_line(&mut acc, SseLine::parse(done), &mut translator)
        .expect("identical repeated done is valid");

    assert!(repeated.is_none());
}

#[test]
fn repeated_function_call_done_rejects_conflicting_lenient_authoritative_content() {
    let added = r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":""}}"#;
    let first = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{}","status":"completed"}}"#;
    let conflicting = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{\"query\":\"rust\"}","status":"completed"}}"#;
    let mut acc = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = TranslationContext::default();
    let mut translator = TranslationDispatcher::new(context);

    RoundIngestion::translate_line(&mut acc, SseLine::parse(added), &mut translator).expect("added item is valid");
    RoundIngestion::translate_line(&mut acc, SseLine::parse(first), &mut translator).expect("first done item is valid");
    let error = RoundIngestion::translate_line(&mut acc, SseLine::parse(conflicting), &mut translator)
        .expect_err("conflicting repeated done must be rejected");

    assert!(error.to_string().contains("conflicting repeated output item.done"));
}

#[test]
fn from_sse_lines_propagates_conflicting_lenient_authoritative_content() {
    let added = r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":""}}"#;
    let first = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{}","status":"completed"}}"#;
    let conflicting = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{\"query\":\"rust\"}","status":"completed"}}"#;

    let error = ResponseAccumulator::from_sse_lines([added, first, conflicting].map(str::to_owned), None)
        .expect_err("conflicting authoritative content must propagate from the constructor");

    assert!(error.to_string().contains("conflicting repeated output item.done"));
}

#[tokio::test]
async fn from_stream_propagates_conflicting_lenient_authoritative_content() {
    let lines = [
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":""}}"#,
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{}","status":"completed"}}"#,
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{\"query\":\"rust\"}","status":"completed"}}"#,
    ];
    let stream = futures::stream::iter(lines.into_iter().map(|line| Ok::<_, ExecutorError>(line.to_owned())));

    let error = ResponseAccumulator::from_stream(Box::pin(stream), None)
        .await
        .expect_err("conflicting authoritative content must propagate from the async constructor");

    assert!(error.to_string().contains("conflicting repeated output item.done"));
}

#[test]
fn repeated_done_rejects_a_conflicting_lenient_item_type() {
    let added = r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":""}}"#;
    let conflicting = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"custom_tool_call","id":"fc_1","call_id":"call_1","name":"lookup","input":"{}","status":"completed"}}"#;
    let mut acc = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = TranslationContext::default();
    let mut translator = TranslationDispatcher::new(context);

    RoundIngestion::translate_line(&mut acc, SseLine::parse(added), &mut translator).expect("added item is valid");
    let error = RoundIngestion::translate_line(&mut acc, SseLine::parse(conflicting), &mut translator)
        .expect_err("an authoritative done item must not change type");

    assert!(error.to_string().contains("does not match its active output item"));
}

#[test]
fn lenient_events_reject_contradictory_explicit_item_id_and_output_index() {
    let added_first = r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":""}}"#;
    let added_second = r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"lookup","arguments":""}}"#;
    let contradictory = r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_1","call_id":"call_1","delta":"{}"}"#;
    let mut acc = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = TranslationContext::default();
    let mut translator = TranslationDispatcher::new(context);

    RoundIngestion::translate_line(&mut acc, SseLine::parse(added_first), &mut translator)
        .expect("first item is valid");
    RoundIngestion::translate_line(&mut acc, SseLine::parse(added_second), &mut translator)
        .expect("second item is valid");
    let error = RoundIngestion::translate_line(&mut acc, SseLine::parse(contradictory), &mut translator)
        .expect_err("explicit item id and output index must resolve to the same item");

    assert!(error.to_string().contains("does not match its active output item"));
}

#[test]
fn test_unknown_mcp_call_error_shape_is_not_dropped() {
    let lines = vec![
        r#"data: {"type":"response.created","response":{"id":"resp_abc"}}"#.to_string(),
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"mcp_call","id":"mcp_1","server_label":"counter","name":"increment","arguments":"","status":"in_progress","approval_request_id":null,"output":null,"error":null}}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"mcp_call","id":"mcp_1","server_label":"counter","name":"increment","arguments":"{}","status":"failed","approval_request_id":null,"output":null,"error":{"type":"mcp_protocol_error","code":-32000,"message":"boom"}}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_abc","status":"completed","usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 1);
    let OutputItem::McpCall(call) = &acc.output[0] else {
        panic!("expected mcp_call");
    };
    let Some(McpCallError::Unknown(error)) = &call.error else {
        panic!("expected unknown MCP error payload");
    };
    assert_eq!(error["type"], "mcp_protocol_error");
    assert_eq!(error["code"], -32000);
    assert_eq!(error["message"], "boom");
}

#[test]
fn test_streaming_preserves_all_documented_mcp_call_statuses() {
    let lines = vec![
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"mcp_call","id":"mcp_calling","server_label":"counter","name":"increment","arguments":"{}","status":"calling","approval_request_id":null,"output":null,"error":null}}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"mcp_call","id":"mcp_incomplete","server_label":"counter","name":"increment","arguments":"{}","status":"incomplete","approval_request_id":null,"output":null,"error":null}}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":2,"item":{"type":"mcp_call","id":"mcp_omitted","server_label":"counter","name":"increment","arguments":"{}","approval_request_id":null,"output":"1","error":null}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    let statuses = acc
        .output
        .iter()
        .map(|item| match item {
            OutputItem::McpCall(call) => call.status,
            _ => panic!("expected mcp_call"),
        })
        .collect::<Vec<_>>();

    assert_eq!(
        statuses,
        vec![Some(McpCallStatus::Calling), Some(McpCallStatus::Incomplete), None]
    );
}

#[test]
fn test_process_event_web_search_done_accumulates_output() {
    let lines = vec![
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress","action":{"type":"search","query":"rust","sources":[]}}}"#.to_string(),
        r#"data: {"type":"response.web_search_call.in_progress","item_id":"ws_1","output_index":0}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"rust","sources":[]}}}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.status, ResponseStatus::Completed);
    assert_eq!(acc.output.len(), 1);
    assert!(matches!(acc.output[0], OutputItem::WebSearchCall(_)));
}

#[test]
fn test_process_event_completed_with_usage() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);
    let frame = EventFrame {
        event_type: SSEEventType::ResponseCompleted,
        payload: EventPayload::Response {
            id: "resp_1".into(),
            status: "completed".into(),
            usage: Some(ResponseUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            }),
        },
        wire: WireEvent::new("test"),
    };
    acc.process_event(&frame);
    assert_eq!(acc.status, ResponseStatus::Completed);
    assert!(acc.usage.is_some());
    assert_eq!(acc.usage.unwrap().total_tokens, 15);
}

#[test]
fn test_process_event_failed_sets_error_status() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);
    acc.process_event(&EventFrame {
        event_type: SSEEventType::ResponseFailed,
        payload: EventPayload::Response {
            id: "resp_1".into(),
            status: "failed".into(),
            usage: None,
        },
        wire: WireEvent::new("response.failed"),
    });
    assert_eq!(acc.status, ResponseStatus::Error);
}

#[test]
fn test_process_event_incomplete_sets_incomplete_status() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);
    acc.process_event(&EventFrame {
        event_type: SSEEventType::ResponseIncomplete,
        payload: EventPayload::Response {
            id: "resp_1".into(),
            status: "incomplete".into(),
            usage: None,
        },
        wire: WireEvent::new("test"),
    });
    assert_eq!(acc.status, ResponseStatus::Incomplete);
}

#[test]
fn test_process_event_unknown_payload_ignored() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);
    let frame = EventFrame {
        event_type: SSEEventType::ContentPartAdded,
        payload: EventPayload::Raw(serde_json::json!({"type": "response.content_part.added"})),
        wire: WireEvent::new("test"),
    };
    acc.process_event(&frame);
    assert_eq!(acc.response_id, "resp_1");
    assert_eq!(acc.status, ResponseStatus::InProgress);
    assert!(acc.output.is_empty());
}

#[test]
fn test_accumulator_reasoning_and_message_from_sse() {
    let lines = vec![
        r#"data: {"type":"response.created","response":{"id":"resp_abc"}}"#.to_string(),
        r#"data: {"type":"response.output_item.added","item":{"id":"rs_1","type":"reasoning","summary":[]}}"#.to_string(),
        r#"data: {"type":"response.reasoning_text.delta","delta":"Let me ","item_id":"rs_1"}"#.to_string(),
        r#"data: {"type":"response.reasoning_text.delta","delta":"think.","item_id":"rs_1"}"#.to_string(),
        r#"data: {"type":"response.reasoning_text.done","text":"Let me think.","item_id":"rs_1"}"#.to_string(),
        r#"data: {"type":"response.output_item.added","item":{"id":"msg_1","type":"message"}}"#.to_string(),
        r#"data: {"type":"response.output_text.delta","delta":"Hello","item_id":"msg_1"}"#.to_string(),
        r#"data: {"type":"response.done","response":{"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.status, ResponseStatus::Completed);
    assert_eq!(acc.output.len(), 2);

    if let OutputItem::Reasoning(r) = &acc.output[0] {
        assert_eq!(r.id, "rs_1");
        assert_eq!(r.content.len(), 1);
        assert_eq!(r.content[0].text, "Let me think.");
    } else {
        panic!("expected OutputItem::Reasoning, got {:?}", acc.output[0]);
    }

    if let OutputItem::Message(msg) = &acc.output[1] {
        assert_eq!(msg.id, "msg_1");
        assert_eq!(msg.content[0].text, "Hello");
    } else {
        panic!("expected OutputItem::Message");
    }
}

#[test]
fn completed_reasoning_replaces_partial_deltas_without_duplication() {
    let lines = [
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","content":[],"summary":[]}}"#.to_owned(),
        r#"data: {"type":"response.reasoning_text.delta","item_id":"rs_1","output_index":0,"content_index":0,"delta":"partial content"}"#.to_owned(),
        r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"partial summary"}"#.to_owned(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","content":[{"type":"reasoning_text","text":"complete content"},{"type":"reasoning_text","text":"second content"}],"summary":[{"type":"summary_text","text":"complete summary"}],"encrypted_content":"opaque-state","status":"completed"}}"#.to_owned(),
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#.to_owned(),
    ];

    let acc = from_sse_lines(lines, None);

    assert_eq!(acc.output.len(), 1);
    assert_eq!(
        serde_json::to_value(&acc.output[0]).unwrap(),
        serde_json::json!({
            "type": "reasoning",
            "id": "rs_1",
            "content": [
                {"type": "reasoning_text", "text": "complete content"},
                {"type": "reasoning_text", "text": "second content"},
            ],
            "summary": [{"type": "summary_text", "text": "complete summary"}],
            "encrypted_content": "opaque-state",
            "status": "completed",
        })
    );
}

#[test]
fn reasoning_done_events_keep_part_index_order() {
    let lines = [
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning"}}"#.to_owned(),
        r#"data: {"type":"response.reasoning_text.done","item_id":"rs_1","output_index":0,"content_index":1,"text":"second content"}"#.to_owned(),
        r#"data: {"type":"response.reasoning_text.done","item_id":"rs_1","output_index":0,"content_index":0,"text":"first content"}"#.to_owned(),
        r#"data: {"type":"response.reasoning_summary_text.done","item_id":"rs_1","output_index":0,"summary_index":1,"text":"second summary"}"#.to_owned(),
        r#"data: {"type":"response.reasoning_summary_text.done","item_id":"rs_1","output_index":0,"summary_index":0,"text":"first summary"}"#.to_owned(),
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#.to_owned(),
    ];

    let acc = from_sse_lines(lines, None);
    let OutputItem::Reasoning(reasoning) = &acc.output[0] else {
        panic!("expected reasoning output");
    };

    assert_eq!(
        reasoning
            .content
            .iter()
            .map(|part| part.text.as_str())
            .collect::<Vec<_>>(),
        ["first content", "second content"]
    );
    assert_eq!(
        reasoning.summary,
        [
            serde_json::json!({"type": "summary_text", "text": "first summary"}),
            serde_json::json!({"type": "summary_text", "text": "second summary"}),
        ]
    );
}

#[test]
fn completed_reasoning_preserves_done_fields_when_omitted() {
    let lines = [
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning"}}"#.to_owned(),
        r#"data: {"type":"response.reasoning_text.done","item_id":"rs_1","output_index":0,"content_index":0,"text":"completed content"}"#.to_owned(),
        r#"data: {"type":"response.reasoning_summary_text.done","item_id":"rs_1","output_index":0,"summary_index":0,"text":"completed summary"}"#.to_owned(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","encrypted_content":{"token":"opaque"},"status":"completed"}}"#.to_owned(),
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#.to_owned(),
    ];

    let acc = from_sse_lines(lines, None);
    let OutputItem::Reasoning(reasoning) = &acc.output[0] else {
        panic!("expected reasoning output");
    };

    assert_eq!(reasoning.content[0].text, "completed content");
    assert_eq!(
        reasoning.summary,
        [serde_json::json!({"type": "summary_text", "text": "completed summary"})]
    );
    assert_eq!(
        reasoning.encrypted_content,
        Some(serde_json::json!({"token": "opaque"}))
    );
    assert_eq!(reasoning.status.as_deref(), Some("completed"));
}

#[test]
fn completed_reasoning_null_and_empty_fields_are_authoritative_independently() {
    let content_null = [
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_content_null","type":"reasoning"}}"#.to_owned(),
        r#"data: {"type":"response.reasoning_text.done","item_id":"rs_content_null","output_index":0,"content_index":0,"text":"discarded content"}"#.to_owned(),
        r#"data: {"type":"response.reasoning_summary_text.done","item_id":"rs_content_null","output_index":0,"summary_index":0,"text":"kept summary"}"#.to_owned(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_content_null","type":"reasoning","content":null}}"#.to_owned(),
    ];
    let summary_empty = [
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_summary_empty","type":"reasoning"}}"#.to_owned(),
        r#"data: {"type":"response.reasoning_text.done","item_id":"rs_summary_empty","output_index":0,"content_index":0,"text":"kept content"}"#.to_owned(),
        r#"data: {"type":"response.reasoning_summary_text.done","item_id":"rs_summary_empty","output_index":0,"summary_index":0,"text":"discarded summary"}"#.to_owned(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_summary_empty","type":"reasoning","summary":[]}}"#.to_owned(),
    ];

    let content_null = from_sse_lines(content_null, None);
    let OutputItem::Reasoning(content_null) = &content_null.output[0] else {
        panic!("expected reasoning output");
    };
    assert!(content_null.content.is_empty());
    assert_eq!(content_null.summary[0]["text"], "kept summary");

    let summary_empty = from_sse_lines(summary_empty, None);
    let OutputItem::Reasoning(summary_empty) = &summary_empty.output[0] else {
        panic!("expected reasoning output");
    };
    assert_eq!(summary_empty.content[0].text, "kept content");
    assert!(summary_empty.summary.is_empty());
}

#[test]
fn streaming_and_nonstreaming_nullable_reasoning_fields_are_equivalent() {
    let streaming = from_sse_lines(
        [r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","content":null,"summary":null,"encrypted_content":null,"status":"completed"}}"#.to_owned()],
        None,
    );
    let nonstreaming = ResponseAccumulator::from_json(
        r#"{"id":"resp_1","status":"completed","output":[{"id":"rs_1","type":"reasoning","content":null,"summary":null,"encrypted_content":null,"status":"completed"}]}"#,
        None,
    )
    .unwrap();

    assert_eq!(
        serde_json::to_value(&streaming.output).unwrap(),
        serde_json::to_value(&nonstreaming.output).unwrap()
    );
}

#[test]
fn done_only_reasoning_uses_output_index_order() {
    let lines = [
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"id":"msg_1","type":"message"}}"#.to_owned(),
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"content_index":0,"delta":"answer"}"#.to_owned(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","content":[{"type":"reasoning_text","text":"thinking"}],"summary":[],"encrypted_content":null,"status":"completed"}}"#.to_owned(),
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#.to_owned(),
    ];

    let acc = from_sse_lines(lines, None);

    assert_eq!(acc.output.len(), 2);
    assert!(matches!(acc.output[0], OutputItem::Reasoning(_)));
    assert!(matches!(acc.output[1], OutputItem::Message(_)));
}

#[test]
fn malformed_completed_reasoning_retains_done_fields() {
    let lines = [
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning"}}"#.to_owned(),
        r#"data: {"type":"response.reasoning_text.done","item_id":"rs_1","output_index":0,"content_index":0,"text":"completed content"}"#.to_owned(),
        r#"data: {"type":"response.reasoning_summary_text.done","item_id":"rs_1","output_index":0,"summary_index":0,"text":"completed summary"}"#.to_owned(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","content":"malformed","summary":[{"type":"summary_text","text":"ignored completion"}],"encrypted_content":"ignored"}}"#.to_owned(),
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#.to_owned(),
    ];

    let acc = from_sse_lines(lines, None);
    let OutputItem::Reasoning(reasoning) = &acc.output[0] else {
        panic!("expected reasoning output");
    };

    assert_eq!(reasoning.content[0].text, "completed content");
    assert_eq!(reasoning.summary[0]["text"], "completed summary");
    assert!(reasoning.encrypted_content.is_none());
}

#[test]
fn test_accumulator_message_then_reasoning_preserves_order() {
    let lines = vec![
        r#"data: {"type":"response.created","response":{"id":"resp_abc"}}"#.to_string(),
        r#"data: {"type":"response.output_item.added","item":{"id":"msg_1","type":"message"}}"#.to_string(),
        r#"data: {"type":"response.output_text.delta","delta":"Hello","item_id":"msg_1"}"#.to_string(),
        r#"data: {"type":"response.output_item.added","item":{"id":"rs_1","type":"reasoning","summary":[]}}"#.to_string(),
        r#"data: {"type":"response.reasoning_text.done","text":"thinking...","item_id":"rs_1"}"#.to_string(),
        r#"data: {"type":"response.done","response":{"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 2);
    assert!(matches!(acc.output[0], OutputItem::Message(_)));
    assert!(matches!(acc.output[1], OutputItem::Reasoning(_)));
}

#[test]
fn test_accumulator_reasoning_done_without_delta_uses_text() {
    let lines = vec![
        r#"data: {"type":"response.output_item.added","item":{"id":"rs_1","type":"reasoning","summary":[]}}"#
            .to_string(),
        r#"data: {"type":"response.reasoning_text.done","text":"done only","item_id":"rs_1"}"#.to_string(),
        r#"data: {"type":"response.done","response":{"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#
            .to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    if let OutputItem::Reasoning(reasoning) = &acc.output[0] {
        assert_eq!(reasoning.content.len(), 1);
        assert_eq!(reasoning.content[0].text, "done only");
    } else {
        panic!("expected reasoning output");
    }
}

#[test]
fn test_accumulator_reasoning_from_json() {
    let body = serde_json::json!({
        "id": "resp_xyz",
        "status": "completed",
        "output": [
            {
                "id": "rs_1",
                "type": "reasoning",
                "summary": [],
                "content": [{"text": "thinking...", "type": "reasoning_text"}],
                "encrypted_content": null,
                "status": null
            },
            {
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "answer", "annotations": []}]
            }
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
    });

    let acc = ResponseAccumulator::from_json(&body.to_string(), None).unwrap();
    assert_eq!(acc.output.len(), 2);
    assert!(matches!(acc.output[0], OutputItem::Reasoning(_)));
    assert!(matches!(acc.output[1], OutputItem::Message(_)));
}

#[test]
fn test_blocking_preserves_all_documented_mcp_call_statuses() {
    let cases: [(Option<&str>, Option<McpCallStatus>); 3] = [
        (Some("calling"), Some(McpCallStatus::Calling)),
        (Some("incomplete"), Some(McpCallStatus::Incomplete)),
        (None, None),
    ];

    for (status, expected) in cases {
        let mut item = serde_json::json!({
            "type": "mcp_call",
            "id": "mcp_1",
            "server_label": "counter",
            "name": "increment",
            "arguments": "{}",
            "approval_request_id": null,
            "output": null,
            "error": null
        });
        if let Some(status) = status {
            item["status"] = serde_json::json!(status);
        }
        let body = serde_json::json!({
            "id": "resp_1",
            "status": "completed",
            "output": [item],
            "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7}
        });

        let acc = ResponseAccumulator::from_json(&body.to_string(), None).unwrap();
        assert_eq!(acc.output.len(), 1);
        let OutputItem::McpCall(call) = &acc.output[0] else {
            panic!("expected mcp_call");
        };
        assert_eq!(call.status, expected);
    }
}

#[test]
fn test_function_call_accumulation_basic() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);

    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputItemAdded,
        payload: EventPayload::OutputItemAdded {
            shell_call: None,
            item_id: "fc_1".into(),
            item_type: "function_call".into(),
            output_index: Some(0),
            name: Some("get_weather".into()),
            namespace: Some("mcp__weather".into()),
            call_id: Some("call_abc".into()),
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::FunctionCallArgumentsDelta,
        payload: EventPayload::FunctionCallArgsDelta {
            delta: r#"{"location""#.into(),
            call_id: Some("call_abc".into()),
            item_id: "fc_1".into(),
            output_index: Some(0),
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::FunctionCallArgumentsDelta,
        payload: EventPayload::FunctionCallArgsDelta {
            delta: r#":"Paris"}"#.into(),
            call_id: Some("call_abc".into()),
            item_id: "fc_1".into(),
            output_index: Some(0),
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::FunctionCallArgumentsDone,
        payload: EventPayload::FunctionCallArgsDone {
            arguments: r#"{"location":"Paris"}"#.into(),
            call_id: Some("call_abc".into()),
            item_id: "fc_1".into(),
            name: "get_weather".into(),
            output_index: Some(0),
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::ResponseCompleted,
        payload: EventPayload::Response {
            id: "resp_1".into(),
            status: "completed".into(),
            usage: None,
        },
        wire: WireEvent::new("test"),
    });

    assert_eq!(acc.status, ResponseStatus::Completed);
    assert_eq!(acc.output.len(), 1);
    if let OutputItem::FunctionCall(fc) = &acc.output[0] {
        assert_eq!(fc.id, "fc_1");
        assert_eq!(fc.call_id, "call_abc");
        assert_eq!(fc.name, "get_weather");
        assert_eq!(fc.namespace.as_deref(), Some("mcp__weather"));
        assert_eq!(fc.arguments, r#"{"location":"Paris"}"#);
        assert_eq!(fc.status, MessageStatus::Completed);
    } else {
        panic!("expected FunctionCall");
    }
}

#[test]
fn test_function_call_done_uses_deltas_when_arguments_empty() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);

    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputItemAdded,
        payload: EventPayload::OutputItemAdded {
            shell_call: None,
            item_id: "fc_1".into(),
            item_type: "function_call".into(),
            output_index: Some(0),
            name: Some("search".into()),
            namespace: None,
            call_id: Some("call_1".into()),
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::FunctionCallArgumentsDelta,
        payload: EventPayload::FunctionCallArgsDelta {
            delta: r#"{"q":"rust"}"#.into(),
            call_id: Some("call_1".into()),
            item_id: "fc_1".into(),
            output_index: Some(0),
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::FunctionCallArgumentsDone,
        payload: EventPayload::FunctionCallArgsDone {
            arguments: String::new(),
            call_id: Some("call_1".into()),
            item_id: "fc_1".into(),
            name: "search".into(),
            output_index: Some(0),
        },
        wire: WireEvent::new("test"),
    });

    acc.finalize_all();
    assert_eq!(acc.output.len(), 1);
    if let OutputItem::FunctionCall(fc) = &acc.output[0] {
        assert_eq!(fc.arguments, r#"{"q":"rust"}"#);
    } else {
        panic!("expected FunctionCall");
    }
}

#[test]
fn test_function_call_multiple_parallel() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);

    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputItemAdded,
        payload: EventPayload::OutputItemAdded {
            shell_call: None,
            item_id: "fc_1".into(),
            item_type: "function_call".into(),
            output_index: Some(0),
            name: Some("get_weather".into()),
            namespace: None,
            call_id: Some("call_1".into()),
        },
        wire: WireEvent::new("test"),
    });
    acc.process_event(&EventFrame {
        event_type: SSEEventType::FunctionCallArgumentsDone,
        payload: EventPayload::FunctionCallArgsDone {
            arguments: r#"{"city":"NYC"}"#.into(),
            call_id: Some("call_1".into()),
            item_id: "fc_1".into(),
            name: "get_weather".into(),
            output_index: Some(0),
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputItemAdded,
        payload: EventPayload::OutputItemAdded {
            shell_call: None,
            item_id: "fc_2".into(),
            item_type: "function_call".into(),
            output_index: Some(1),
            name: Some("get_time".into()),
            namespace: None,
            call_id: Some("call_2".into()),
        },
        wire: WireEvent::new("test"),
    });
    acc.process_event(&EventFrame {
        event_type: SSEEventType::FunctionCallArgumentsDone,
        payload: EventPayload::FunctionCallArgsDone {
            arguments: r#"{"tz":"EST"}"#.into(),
            call_id: Some("call_2".into()),
            item_id: "fc_2".into(),
            name: "get_time".into(),
            output_index: Some(1),
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::ResponseCompleted,
        payload: EventPayload::Response {
            id: "resp_1".into(),
            status: "completed".into(),
            usage: None,
        },
        wire: WireEvent::new("test"),
    });

    assert_eq!(acc.output.len(), 2);
    assert!(matches!(&acc.output[0], OutputItem::FunctionCall(fc) if fc.name == "get_weather"));
    assert!(matches!(&acc.output[1], OutputItem::FunctionCall(fc) if fc.name == "get_time"));
}

#[test]
fn test_function_call_interleaved_with_message() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);

    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputItemAdded,
        payload: EventPayload::OutputItemAdded {
            shell_call: None,
            item_id: "msg_1".into(),
            item_type: "message".into(),
            output_index: Some(0),
            name: None,
            namespace: None,
            call_id: None,
        },
        wire: WireEvent::new("test"),
    });
    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputTextDelta,
        payload: EventPayload::TextDelta {
            delta: "Let me check".into(),
            item_id: "msg_1".into(),
            output_index: Some(0),
            content_index: 0,
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputItemAdded,
        payload: EventPayload::OutputItemAdded {
            shell_call: None,
            item_id: "fc_1".into(),
            item_type: "function_call".into(),
            output_index: Some(1),
            name: Some("lookup".into()),
            namespace: None,
            call_id: Some("call_x".into()),
        },
        wire: WireEvent::new("test"),
    });
    acc.process_event(&EventFrame {
        event_type: SSEEventType::FunctionCallArgumentsDone,
        payload: EventPayload::FunctionCallArgsDone {
            arguments: "{}".into(),
            call_id: Some("call_x".into()),
            item_id: "fc_1".into(),
            name: "lookup".into(),
            output_index: Some(1),
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::ResponseCompleted,
        payload: EventPayload::Response {
            id: "resp_1".into(),
            status: "completed".into(),
            usage: None,
        },
        wire: WireEvent::new("test"),
    });

    assert_eq!(acc.output.len(), 2);
    assert!(matches!(&acc.output[0], OutputItem::Message(m) if m.content[0].text == "Let me check"));
    assert!(matches!(&acc.output[1], OutputItem::FunctionCall(fc) if fc.name == "lookup"));
}

#[test]
fn test_function_call_done_updates_metadata() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);

    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputItemAdded,
        payload: EventPayload::OutputItemAdded {
            shell_call: None,
            item_id: "fc_1".into(),
            item_type: "function_call".into(),
            output_index: Some(0),
            name: Some("old_name".into()),
            namespace: None,
            call_id: Some("old_call".into()),
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::FunctionCallArgumentsDone,
        payload: EventPayload::FunctionCallArgsDone {
            arguments: "{}".into(),
            call_id: Some("new_call".into()),
            item_id: "fc_1".into(),
            name: "new_name".into(),
            output_index: Some(0),
        },
        wire: WireEvent::new("test"),
    });

    acc.finalize_all();
    if let OutputItem::FunctionCall(fc) = &acc.output[0] {
        assert_eq!(fc.call_id, "new_call");
        assert_eq!(fc.name, "new_name");
    } else {
        panic!("expected FunctionCall");
    }
}

#[test]
fn test_output_item_done_restores_initially_unnamed_function_call() {
    let lines = vec![
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"","name":"","arguments":"","status":"in_progress"}}"#.to_string(),
        r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"{\"input\":\"hello\"}"}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"raw_echo","arguments":"","status":"completed"}}"#.to_string(),
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":null}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 1);
    let OutputItem::FunctionCall(call) = &acc.output[0] else {
        panic!("expected function_call");
    };
    assert_eq!(call.id, "fc_1");
    assert_eq!(call.call_id, "call_1");
    assert_eq!(call.name, "raw_echo");
    assert_eq!(call.arguments, r#"{"input":"hello"}"#);
    assert_eq!(call.status, MessageStatus::Completed);
}

#[test]
fn test_function_call_done_matches_empty_added_id_by_output_index() {
    let lines = vec![
        r#"data: {"type":"response.output_item.added","output_index":3,"item":{"type":"function_call","id":"","call_id":"","name":"","arguments":"","status":"in_progress"}}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":3,"item":{"type":"function_call","id":"fc_done","call_id":"call_done","name":"raw_echo","arguments":"{}","status":"completed"}}"#.to_string(),
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":null}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 1);
    let OutputItem::FunctionCall(call) = &acc.output[0] else {
        panic!("expected function_call");
    };
    assert_eq!(call.id, "fc_done");
    assert_eq!(call.call_id, "call_done");
    assert_eq!(call.name, "raw_echo");
}

#[test]
fn test_done_only_function_call_is_completed() {
    let lines = vec![
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"Paris\"}","status":"completed"}}"#.to_string(),
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":null}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 1);
    let OutputItem::FunctionCall(call) = &acc.output[0] else {
        panic!("expected function_call");
    };
    assert_eq!(call.id, "fc_1");
    assert_eq!(call.call_id, "call_1");
    assert_eq!(call.name, "get_weather");
    assert_eq!(call.arguments, r#"{"city":"Paris"}"#);
}

#[test]
fn test_function_call_empty_item_id_generates_uuid() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);

    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputItemAdded,
        payload: EventPayload::OutputItemAdded {
            shell_call: None,
            item_id: String::new(),
            item_type: "function_call".into(),
            output_index: Some(0),
            name: Some("tool".into()),
            namespace: None,
            call_id: Some("c1".into()),
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::FunctionCallArgumentsDone,
        payload: EventPayload::FunctionCallArgsDone {
            arguments: "{}".into(),
            call_id: Some("c1".into()),
            item_id: String::new(),
            name: "tool".into(),
            output_index: Some(0),
        },
        wire: WireEvent::new("test"),
    });

    acc.finalize_all();
    if let OutputItem::FunctionCall(fc) = &acc.output[0] {
        assert!(fc.id.starts_with("fc_"), "expected fc_ prefix, got: {}", fc.id);
    } else {
        panic!("expected FunctionCall");
    }
}

/// Orphaned delta (no active function call for this `item_id`) is silently dropped.
#[test]
fn test_function_call_orphaned_delta_safe() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);

    acc.process_event(&EventFrame {
        event_type: SSEEventType::FunctionCallArgumentsDelta,
        payload: EventPayload::FunctionCallArgsDelta {
            delta: "orphan".into(),
            call_id: None,
            item_id: String::new(),
            output_index: Some(0),
        },
        wire: WireEvent::new("test"),
    });

    assert!(acc.output.is_empty());
    assert_eq!(acc.slots.len(), 0);
}

#[test]
fn test_function_call_finalized_on_response_completed() {
    let mut acc = ResponseAccumulator::new("resp_1".into(), None);

    acc.process_event(&EventFrame {
        event_type: SSEEventType::OutputItemAdded,
        payload: EventPayload::OutputItemAdded {
            shell_call: None,
            item_id: "fc_1".into(),
            item_type: "function_call".into(),
            output_index: Some(0),
            name: Some("partial".into()),
            namespace: None,
            call_id: Some("c1".into()),
        },
        wire: WireEvent::new("test"),
    });
    acc.process_event(&EventFrame {
        event_type: SSEEventType::FunctionCallArgumentsDelta,
        payload: EventPayload::FunctionCallArgsDelta {
            delta: r#"{"x":1}"#.into(),
            call_id: Some("c1".into()),
            item_id: "fc_1".into(),
            output_index: Some(0),
        },
        wire: WireEvent::new("test"),
    });

    acc.process_event(&EventFrame {
        event_type: SSEEventType::ResponseCompleted,
        payload: EventPayload::Response {
            id: "resp_1".into(),
            status: "completed".into(),
            usage: None,
        },
        wire: WireEvent::new("test"),
    });

    assert_eq!(acc.output.len(), 1);
    if let OutputItem::FunctionCall(fc) = &acc.output[0] {
        assert_eq!(fc.arguments, r#"{"x":1}"#);
        assert_eq!(fc.status, MessageStatus::Completed);
    } else {
        panic!("expected FunctionCall");
    }
}

#[test]
fn test_function_call_from_sse_lines() {
    let lines = vec![
        r#"data: {"type":"response.created","response":{"id":"resp_fc"}}"#.to_string(),
        r#"data: {"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","name":"get_weather","call_id":"call_abc"}}"#.to_string(),
        r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"city\":","item_id":"fc_1"}"#.to_string(),
        r#"data: {"type":"response.function_call_arguments.delta","delta":"\"SF\"}}","item_id":"fc_1"}"#.to_string(),
        r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"city\":\"SF\"}","call_id":"call_abc","name":"get_weather","item_id":"fc_1"}"#.to_string(),
        r#"data: {"type":"response.done","response":{"id":"resp_fc","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, Some("conv_1"));
    assert_eq!(acc.status, ResponseStatus::Completed);
    assert_eq!(acc.output.len(), 1);

    if let OutputItem::FunctionCall(fc) = &acc.output[0] {
        assert_eq!(fc.name, "get_weather");
        assert_eq!(fc.arguments, r#"{"city":"SF"}"#);
        assert_eq!(fc.call_id, "call_abc");
    } else {
        panic!("expected FunctionCall");
    }

    assert_eq!(acc.usage.unwrap().total_tokens, 15);
}

#[test]
fn test_custom_tool_call_accumulates_freeform_input() {
    let lines = vec![
        r#"data: {"type":"response.created","response":{"id":"resp_custom"}}"#.to_string(),
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"","name":"","input":"","status":"in_progress"}}"#.to_string(),
        r#"data: {"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","output_index":0,"delta":"*** Begin"}"#.to_string(),
        r#"data: {"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","output_index":0,"delta":" Patch"}"#.to_string(),
        r#"data: {"type":"response.custom_tool_call_input.done","item_id":"ctc_1","output_index":0,"input":""}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_1","name":"apply_patch","input":"","status":"completed"}}"#.to_string(),
        r#"data: {"type":"response.completed","response":{"id":"resp_custom","status":"completed","usage":null}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 1);
    let OutputItem::CustomToolCall(call) = &acc.output[0] else {
        panic!("expected CustomToolCall");
    };
    assert_eq!(call.call_id, "call_1");
    assert_eq!(call.name, "apply_patch");
    assert_eq!(call.input, "*** Begin Patch");
    assert_eq!(call.status, Some(MessageStatus::Completed));
}

#[test]
fn test_reasoning_before_done_only_custom_tool_call_preserves_order() {
    let lines = vec![
        r#"data: {"type":"response.created","response":{"id":"resp_custom"}}"#.to_string(),
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}"#.to_string(),
        r#"data: {"type":"response.reasoning_text.done","text":"thinking...","item_id":"rs_1"}"#.to_string(),
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_1","name":"raw_echo","input":"hello","status":"completed"}}"#.to_string(),
        r#"data: {"type":"response.completed","response":{"id":"resp_custom","status":"completed","usage":null}}"#.to_string(),
    ];

    let acc = from_sse_lines(lines, None);
    assert_eq!(acc.output.len(), 2);
    assert!(matches!(acc.output[0], OutputItem::Reasoning(_)));
    let OutputItem::CustomToolCall(call) = &acc.output[1] else {
        panic!("expected CustomToolCall");
    };
    assert_eq!(call.call_id, "call_1");
    assert_eq!(call.name, "raw_echo");
    assert_eq!(call.input, "hello");
}

#[test]
fn file_search_lifecycle_preserves_typed_output() {
    let call = serde_json::json!({"type":"file_search_call","id":"fs_1","status":"completed","queries":["policy"],"results":[]});
    let mut acc = ResponseAccumulator::with_validation("resp_1".to_owned(), None, Validation::Strict);
    let frames = [
        serde_json::json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}),
        serde_json::json!({"type":"response.in_progress","response":{"id":"resp_1","status":"in_progress"}}),
        serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"file_search_call","id":"fs_1","status":"in_progress","queries":["policy"]}}),
        serde_json::json!({"type":"response.file_search_call.in_progress","output_index":0,"item_id":"fs_1"}),
        serde_json::json!({"type":"response.file_search_call.searching","output_index":0,"item_id":"fs_1"}),
        serde_json::json!({"type":"response.file_search_call.completed","output_index":0,"item_id":"fs_1"}),
        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":call}),
        serde_json::json!({"type":"response.completed","response":{"id":"resp_1","status":"completed"}}),
    ];
    for frame in frames {
        acc.process_line(SseLine::parse(&format!("data: {frame}"))).unwrap();
    }
    let output = serde_json::to_value(acc.finalize("test", None, None).output).unwrap();
    assert_eq!(output, serde_json::json!([call]));
}

#[test]
fn file_search_done_only_is_preserved_leniently_and_rejected_strictly() {
    let call = serde_json::json!({"type":"file_search_call","id":"fs_1","status":"completed","queries":["policy"],"results":[]});
    let lines = [
        serde_json::json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}),
        serde_json::json!({"type":"response.in_progress","response":{"id":"resp_1","status":"in_progress"}}),
        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":call}),
        serde_json::json!({"type":"response.completed","response":{"id":"resp_1","status":"completed"}}),
    ]
    .map(|event| format!("data: {event}"));
    let payload = from_sse_lines(lines.clone(), None).finalize("test", None, None);
    assert_eq!(serde_json::to_value(payload.output).unwrap(), serde_json::json!([call]));

    let mut strict = ResponseAccumulator::with_validation("resp_1".to_owned(), None, Validation::Strict);
    for line in &lines[..2] {
        strict.process_line(SseLine::parse(line)).unwrap();
    }
    assert!(strict.process_line(SseLine::parse(&lines[2])).is_err());
}

#[test]
fn file_search_progress_rejects_wrong_identity_and_completion_order() {
    for invalid in [
        serde_json::json!({"type":"response.file_search_call.searching","output_index":1,"item_id":"fs_1"}),
        serde_json::json!({"type":"response.file_search_call.completed","output_index":0,"item_id":"other"}),
    ] {
        let mut acc = ResponseAccumulator::with_validation("resp_1".to_owned(), None, Validation::Strict);
        for frame in [
            serde_json::json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}),
            serde_json::json!({"type":"response.in_progress","response":{"id":"resp_1","status":"in_progress"}}),
            serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"file_search_call","id":"fs_1","status":"in_progress","queries":[]}}),
        ] {
            acc.process_line(SseLine::parse(&format!("data: {frame}"))).unwrap();
        }
        assert!(acc.process_line(SseLine::parse(&format!("data: {invalid}"))).is_err());
    }
    let mut acc = ResponseAccumulator::with_validation("resp_1".to_owned(), None, Validation::Strict);
    let event = r#"data: {"type":"response.file_search_call.completed","output_index":0,"item_id":"fs_1"}"#;
    assert!(acc.process_line(SseLine::parse(event)).is_err());
}
