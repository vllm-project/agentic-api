use super::*;
use crate::events::{SSEEventType, SseLine};
use crate::executor::accumulator::ResponseAccumulator;
use crate::executor::pipeline::RoundIngestion;
use crate::tool::ToolType;
use serde_json::Value;
use std::collections::HashMap;

pub(super) fn test_context(tool_types: HashMap<String, ToolType>) -> TranslationContext {
    let active = tool_types.get("tool_search") == Some(&ToolType::ToolSearch);
    TranslationContext::new(tool_types, HashSet::new(), active)
}

pub(super) fn sse(value: &Value) -> String {
    format!("data: {value}")
}

pub(super) fn translate(
    accumulator: &mut ResponseAccumulator,
    translator: &mut TranslationDispatcher,
    value: &Value,
) -> Translation {
    RoundIngestion::translate_line(accumulator, SseLine::parse(&sse(value)), translator)
        .expect("translation succeeds")
        .expect("SSE event")
}

#[test]
fn custom_function_arguments_are_emitted_incrementally() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("raw_echo".to_owned(), ToolType::Custom)]));
    let mut translator = TranslationDispatcher::new(context);
    let mut frames = Vec::new();

    for event in [
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "fc_custom",
                "type": "function_call",
                "status": "in_progress",
                "call_id": "call_custom",
                "name": "raw_echo",
                "arguments": ""
            }
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "item_id": "fc_custom",
            "call_id": "call_custom",
            "delta": "{\"in"
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "item_id": "fc_custom",
            "call_id": "call_custom",
            "delta": "put\":\"hello "
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "item_id": "fc_custom",
            "call_id": "call_custom",
            "delta": "world\"}"
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.done",
            "output_index": 0,
            "item_id": "fc_custom",
            "call_id": "call_custom",
            "name": "raw_echo",
            "arguments": "{\"input\":\"hello world\"}"
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "fc_custom",
                "type": "function_call",
                "status": "completed",
                "call_id": "call_custom",
                "name": "raw_echo",
                "arguments": "{\"input\":\"hello world\"}"
            }
        }),
    ] {
        frames.extend(translate(&mut accumulator, &mut translator, &event).frames);
    }

    assert_eq!(
        frames.iter().map(|frame| frame.event_type).collect::<Vec<_>>(),
        [
            SSEEventType::OutputItemAdded,
            SSEEventType::CustomToolCallInputDelta,
            SSEEventType::CustomToolCallInputDelta,
            SSEEventType::CustomToolCallInputDone,
            SSEEventType::OutputItemDone,
        ]
    );
    assert_eq!(frames[0].wire.rest["item"]["type"], "custom_tool_call");
    assert_eq!(frames[1].wire.rest["delta"], "hello ");
    assert_eq!(frames[2].wire.rest["delta"], "world");
    assert_eq!(frames[3].wire.rest["input"], "hello world");
    assert_eq!(frames[4].wire.rest["item"]["input"], "hello world");
}

#[test]
fn custom_input_deltas_match_authoritative_done_input() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("raw_echo".to_owned(), ToolType::Custom)]));
    let mut translator = TranslationDispatcher::new(context);
    let events = [
        serde_json::json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1",
                "name": "raw_echo", "arguments": "", "status": "in_progress"}
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta", "output_index": 0,
            "item_id": "fc_1", "call_id": "call_1", "delta": "{\"input\":\"hello\""
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.done", "output_index": 0,
            "item_id": "fc_1", "call_id": "call_1", "name": "raw_echo",
            "arguments": "{\"input\":\"hello\",\"extra\":true}"
        }),
        serde_json::json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1",
                "name": "raw_echo", "arguments": "{\"input\":\"hello\",\"extra\":true}", "status": "completed"}
        }),
    ];

    let mut frames = Vec::new();
    for event in events {
        frames.extend(translate(&mut accumulator, &mut translator, &event).frames);
    }
    let deltas = frames
        .iter()
        .filter(|frame| frame.event_type == SSEEventType::CustomToolCallInputDelta)
        .filter_map(|frame| frame.wire.rest["delta"].as_str())
        .collect::<String>();
    let done = frames
        .iter()
        .find(|frame| frame.event_type == SSEEventType::CustomToolCallInputDone)
        .and_then(|frame| frame.wire.rest["input"].as_str())
        .expect("input.done");

    assert_eq!(deltas, done);
}

#[test]
fn custom_input_rejects_authoritative_value_that_contradicts_deltas() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("raw_echo".to_owned(), ToolType::Custom)]));
    let mut translator = TranslationDispatcher::new(context);
    let events = [
        serde_json::json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1",
                "name": "raw_echo", "arguments": "", "status": "in_progress"}
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta", "output_index": 0,
            "item_id": "fc_1", "call_id": "call_1", "delta": "{\"input\":\"hello\"}"
        }),
    ];
    for event in events {
        translate(&mut accumulator, &mut translator, &event);
    }
    let done = serde_json::json!({
        "type": "response.function_call_arguments.done", "output_index": 0,
        "item_id": "fc_1", "call_id": "call_1", "name": "raw_echo",
        "arguments": "{\"input\":\"bye\"}"
    });

    let error = RoundIngestion::translate_line(&mut accumulator, SseLine::parse(&sse(&done)), &mut translator)
        .expect_err("contradictory final input must fail");
    assert!(error.to_string().contains("contradicts streamed custom tool input"));
}

#[test]
fn malformed_custom_input_escape_is_rejected() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("raw_echo".to_owned(), ToolType::Custom)]));
    let mut translator = TranslationDispatcher::new(context);
    let added = serde_json::json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1",
            "name": "raw_echo", "arguments": "", "status": "in_progress"}
    });
    translate(&mut accumulator, &mut translator, &added);
    let delta = serde_json::json!({
        "type": "response.function_call_arguments.delta", "output_index": 0,
        "item_id": "fc_1", "call_id": "call_1", "delta": r#"{"input":"\q"#
    });

    let error = RoundIngestion::translate_line(&mut accumulator, SseLine::parse(&sse(&delta)), &mut translator)
        .expect_err("invalid JSON string escape must fail");
    assert!(error.to_string().contains("invalid custom tool input"));
}

#[test]
fn custom_input_waits_for_split_unicode_surrogate_pair() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("raw_echo".to_owned(), ToolType::Custom)]));
    let mut translator = TranslationDispatcher::new(context);
    let events = [
        serde_json::json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1",
                "name": "raw_echo", "arguments": "", "status": "in_progress"}
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta", "output_index": 0,
            "item_id": "fc_1", "call_id": "call_1", "delta": r#"{"input":"hi \uD83D"#
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta", "output_index": 0,
            "item_id": "fc_1", "call_id": "call_1", "delta": r#"\uDE00"}"#
        }),
    ];

    let frames = events
        .iter()
        .flat_map(|event| translate(&mut accumulator, &mut translator, event).frames)
        .collect::<Vec<_>>();
    let input = frames
        .iter()
        .filter(|frame| frame.event_type == SSEEventType::CustomToolCallInputDelta)
        .filter_map(|frame| frame.wire.rest["delta"].as_str())
        .collect::<String>();

    assert_eq!(input, "hi 😀");
}

#[test]
fn custom_input_over_limit_is_rejected() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("raw_echo".to_owned(), ToolType::Custom)]));
    let mut translator = TranslationDispatcher::new(context);
    let added = serde_json::json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1",
            "name": "raw_echo", "arguments": "", "status": "in_progress"}
    });
    translate(&mut accumulator, &mut translator, &added);
    let oversized = serde_json::json!({
        "type": "response.function_call_arguments.delta", "output_index": 0,
        "item_id": "fc_1", "call_id": "call_1",
        "delta": format!("{{\"input\":\"{}", "x".repeat(MAX_PENDING_FUNCTION_BYTES + 1))
    });

    let error = RoundIngestion::translate_line(&mut accumulator, SseLine::parse(&sse(&oversized)), &mut translator)
        .expect_err("oversized custom input must fail");
    assert!(error.to_string().contains("function-call SSE exceeded"));
}

#[test]
fn ordinary_functions_pass_through_unchanged() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("echo".to_owned(), ToolType::Function)]));
    let mut translator = TranslationDispatcher::new(context);
    let event = serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 3,
        "item": {
            "id": "fc_echo",
            "type": "function_call",
            "call_id": "call_echo",
            "name": "echo",
            "arguments": ""
        }
    });

    let translated = translate(&mut accumulator, &mut translator, &event);

    assert_eq!(translated.frames.len(), 1);
    assert_eq!(translated.frames[0].event_type, SSEEventType::OutputItemAdded);
    assert_eq!(translated.frames[0].wire.rest["item"]["type"], "function_call");
    assert_eq!(translated.defer_from_output_index, None);
}

#[test]
fn unnamed_function_frames_are_recovered_by_output_index_when_done_binds_id() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("echo".to_owned(), ToolType::Function)]));
    let mut translator = TranslationDispatcher::new(context);
    let mut frames = Vec::new();
    let mut defer_boundaries = Vec::new();

    for event in [
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {
                "id": "",
                "type": "function_call",
                "status": "in_progress",
                "call_id": "call_echo",
                "arguments": ""
            }
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 1,
            "item_id": "",
            "call_id": "call_echo",
            "delta": "{\"value\":1}"
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {
                "id": "fc_stable",
                "type": "function_call",
                "status": "completed",
                "call_id": "call_echo",
                "name": "echo",
                "arguments": "{\"value\":1}"
            }
        }),
    ] {
        let translated = translate(&mut accumulator, &mut translator, &event);
        defer_boundaries.push(translated.defer_from_output_index);
        frames.extend(translated.frames);
    }

    assert_eq!(
        frames.iter().map(|frame| frame.event_type).collect::<Vec<_>>(),
        [
            SSEEventType::OutputItemAdded,
            SSEEventType::FunctionCallArgumentsDelta,
            SSEEventType::OutputItemDone,
        ]
    );
    assert_eq!(frames[0].wire.rest["item"]["id"], "");
    assert_eq!(frames[1].wire.rest["item_id"], "");
    assert_eq!(frames[2].wire.rest["item"]["id"], "fc_stable");
    assert_eq!(defer_boundaries, [Some(1), Some(1), None]);
}

#[test]
fn unnamed_custom_function_is_recovered_by_output_index_when_done_binds_id() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("raw_echo".to_owned(), ToolType::Custom)]));
    let mut translator = TranslationDispatcher::new(context);
    let events = [
        serde_json::json!({
            "type": "response.output_item.added", "output_index": 1,
            "item": {"id": "", "type": "function_call", "status": "in_progress",
                "call_id": "call_echo", "arguments": ""}
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta", "output_index": 1,
            "item_id": "", "call_id": "call_echo", "delta": "{\"input\":\"hello\"}"
        }),
        serde_json::json!({
            "type": "response.output_item.done", "output_index": 1,
            "item": {"id": "fc_stable", "type": "function_call", "status": "completed",
                "call_id": "call_echo", "name": "raw_echo", "arguments": "{\"input\":\"hello\"}"}
        }),
    ];

    let frames = events
        .iter()
        .flat_map(|event| translate(&mut accumulator, &mut translator, event).frames)
        .collect::<Vec<_>>();

    assert_eq!(
        frames.iter().map(|frame| frame.event_type).collect::<Vec<_>>(),
        [
            SSEEventType::OutputItemAdded,
            SSEEventType::CustomToolCallInputDelta,
            SSEEventType::CustomToolCallInputDone,
            SSEEventType::OutputItemDone,
        ]
    );
    assert_eq!(frames[0].wire.rest["item"]["id"], "ctc_stable");
    assert_eq!(frames[1].wire.rest["item_id"], "ctc_stable");
    assert_eq!(frames[2].wire.rest["item_id"], "ctc_stable");
    assert_eq!(frames[3].wire.rest["item"]["id"], "ctc_stable");
}

#[test]
fn parallel_unnamed_functions_with_empty_ids_remain_distinct() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([
        ("first".to_owned(), ToolType::Function),
        ("second".to_owned(), ToolType::Function),
    ]));
    let mut translator = TranslationDispatcher::new(context);
    let events = [
        serde_json::json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "", "type": "function_call", "arguments": ""}
        }),
        serde_json::json!({
            "type": "response.output_item.added", "output_index": 1,
            "item": {"id": "", "type": "function_call", "arguments": ""}
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta", "output_index": 0,
            "item_id": "", "delta": "{\"value\":\"a\"}"
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta", "output_index": 1,
            "item_id": "", "delta": "{\"value\":\"b\"}"
        }),
        serde_json::json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "fc_first", "type": "function_call", "call_id": "call_first",
                "name": "first", "arguments": "{\"value\":\"a\"}", "status": "completed"}
        }),
        serde_json::json!({
            "type": "response.output_item.done", "output_index": 1,
            "item": {"id": "fc_second", "type": "function_call", "call_id": "call_second",
                "name": "second", "arguments": "{\"value\":\"b\"}", "status": "completed"}
        }),
    ];

    let mut frames = Vec::new();
    for event in events {
        frames.extend(translate(&mut accumulator, &mut translator, &event).frames);
    }

    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.event_type == SSEEventType::OutputItemAdded)
            .count(),
        2
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.event_type == SSEEventType::OutputItemDone)
            .count(),
        2
    );
}

#[test]
fn parallel_named_custom_functions_with_empty_ids_remain_distinct() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([
        ("first".to_owned(), ToolType::Custom),
        ("second".to_owned(), ToolType::Custom),
    ]));
    let mut translator = TranslationDispatcher::new(context);
    let events = [
        serde_json::json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "", "type": "function_call", "call_id": "call_first",
                "name": "first", "arguments": "", "status": "in_progress"}
        }),
        serde_json::json!({
            "type": "response.output_item.added", "output_index": 1,
            "item": {"id": "", "type": "function_call", "call_id": "call_second",
                "name": "second", "arguments": "", "status": "in_progress"}
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta", "output_index": 0,
            "item_id": "", "call_id": "call_first", "delta": "{\"input\":\"a\"}"
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta", "output_index": 1,
            "item_id": "", "call_id": "call_second", "delta": "{\"input\":\"b\"}"
        }),
    ];

    let frames = events
        .iter()
        .flat_map(|event| translate(&mut accumulator, &mut translator, event).frames)
        .collect::<Vec<_>>();
    let deltas = frames
        .iter()
        .filter(|frame| frame.event_type == SSEEventType::CustomToolCallInputDelta)
        .map(|frame| (frame.wire.output_index, frame.wire.rest["delta"].as_str()))
        .collect::<Vec<_>>();

    assert_eq!(deltas, [(Some(0), Some("a")), (Some(1), Some("b"))]);
}

#[test]
fn unnamed_custom_function_with_empty_id_uses_one_public_id() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("raw_echo".to_owned(), ToolType::Custom)]));
    let mut translator = TranslationDispatcher::new(context);
    let events = [
        serde_json::json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "", "type": "function_call", "call_id": "call_1", "arguments": ""}
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta", "output_index": 0,
            "item_id": "", "call_id": "call_1", "delta": "{\"input\":\"hello\"}"
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.done", "output_index": 0,
            "item_id": "", "call_id": "call_1", "name": "raw_echo",
            "arguments": "{\"input\":\"hello\"}"
        }),
    ];

    let frames = events
        .iter()
        .flat_map(|event| translate(&mut accumulator, &mut translator, event).frames)
        .collect::<Vec<_>>();
    let added_id = frames
        .iter()
        .find(|frame| frame.event_type == SSEEventType::OutputItemAdded)
        .and_then(|frame| frame.wire.rest["item"]["id"].as_str())
        .expect("custom item id");
    let lifecycle_ids = frames.iter().filter_map(|frame| {
        matches!(
            frame.event_type,
            SSEEventType::CustomToolCallInputDelta | SSEEventType::CustomToolCallInputDone
        )
        .then(|| frame.wire.rest["item_id"].as_str())
        .flatten()
    });

    assert!(lifecycle_ids.eq(std::iter::repeat_n(added_id, 2)));
}

#[test]
fn gateway_owned_functions_are_suppressed_and_mark_the_defer_boundary() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("web_search".to_owned(), ToolType::WebSearch)]));
    let mut translator = TranslationDispatcher::new(context);
    let added = serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 2,
        "item": {
            "id": "fc_search",
            "type": "function_call",
            "call_id": "call_search",
            "name": "web_search",
            "arguments": ""
        }
    });
    let delta = serde_json::json!({
        "type": "response.function_call_arguments.delta",
        "output_index": 2,
        "item_id": "fc_search",
        "call_id": "call_search",
        "delta": "{}"
    });

    let added = translate(&mut accumulator, &mut translator, &added);
    let delta = translate(&mut accumulator, &mut translator, &delta);

    assert!(added.frames.is_empty());
    assert_eq!(added.defer_from_output_index, Some(2));
    assert!(delta.frames.is_empty());
    assert_eq!(delta.defer_from_output_index, Some(2));
}

#[test]
fn synthetic_tool_search_emits_public_frames_but_accumulates_function_call() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("tool_search".to_owned(), ToolType::ToolSearch)]));
    let mut translator = TranslationDispatcher::new(context);
    let events = [
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "fc_search",
                "type": "function_call",
                "call_id": "call_search",
                "name": "tool_search",
                "arguments": "",
                "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "item_id": "fc_search",
            "call_id": "call_search",
            "delta": "[\"weather\",\"timezone\"]"
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.done",
            "output_index": 0,
            "item_id": "fc_search",
            "call_id": "call_search",
            "name": "tool_search",
            "arguments": "[\"weather\",\"timezone\"]"
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "fc_search",
                "type": "function_call",
                "call_id": "call_search",
                "name": "tool_search",
                "arguments": "[\"weather\",\"timezone\"]",
                "status": "completed"
            }
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "status": "completed", "output": []}
        }),
    ];

    let frames = events
        .iter()
        .flat_map(|event| translate(&mut accumulator, &mut translator, event).frames)
        .collect::<Vec<_>>();
    let outcome = translator.finish().expect("completed lifecycle");
    assert!(outcome.unfinished_tool_search_item_ids.is_empty());
    assert_eq!(
        frames
            .iter()
            .filter(|frame| {
                matches!(
                    frame.event_type,
                    SSEEventType::OutputItemAdded | SSEEventType::OutputItemDone
                ) && frame.wire.rest["item"]["type"] == "tool_search_call"
            })
            .count(),
        2
    );
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(frame.event_type, SSEEventType::FunctionCallArgumentsDelta))
    );
    let completed = frames
        .iter()
        .find(|frame| frame.event_type == SSEEventType::OutputItemDone)
        .expect("synthetic search emits a completed public item");
    assert_eq!(
        completed.wire.rest["item"]["arguments"],
        serde_json::json!(["weather", "timezone"])
    );

    let payload = accumulator.finalize("test", None, None);
    assert!(matches!(payload.output.as_slice(), [OutputItem::FunctionCall(_)]));
}

#[test]
fn second_tool_search_call_is_rejected_across_native_and_synthetic_shapes() {
    let synthetic = |output_index: u32, suffix: &str| {
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "id": format!("fc_{suffix}"),
                "type": "function_call",
                "call_id": format!("call_{suffix}"),
                "name": "tool_search",
                "arguments": "",
                "status": "in_progress"
            }
        })
    };
    let native = |output_index: u32, suffix: &str| {
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "id": format!("tsc_{suffix}"),
                "type": "tool_search_call",
                "call_id": format!("call_{suffix}"),
                "execution": "client",
                "arguments": {},
                "status": "in_progress"
            }
        })
    };
    let cases = [
        (
            "synthetic then synthetic",
            synthetic(0, "first"),
            synthetic(1, "second"),
        ),
        ("native then native", native(0, "first"), native(1, "second")),
        ("synthetic then native", synthetic(0, "first"), native(1, "second")),
        ("native then synthetic", native(0, "first"), synthetic(1, "second")),
    ];

    for (case, first, second) in cases {
        let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
        let context = test_context(HashMap::from([("tool_search".to_owned(), ToolType::ToolSearch)]));
        let mut translator = TranslationDispatcher::new(context);
        let first = RoundIngestion::translate_line(&mut accumulator, SseLine::parse(&sse(&first)), &mut translator)
            .expect(case)
            .expect("first search event");
        assert_eq!(first.frames.len(), 1, "{case}: first added frame remains public");

        let error = RoundIngestion::translate_line(&mut accumulator, SseLine::parse(&sse(&second)), &mut translator)
            .expect_err(case);
        assert!(
            matches!(
                error,
                ExecutorError::Tool(crate::tool::ToolError::InvalidUpstreamToolSearch)
            ),
            "{case}"
        );
    }
}

#[test]
fn terminal_output_rejects_multiple_tool_search_calls() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("tool_search".to_owned(), ToolType::ToolSearch)]));
    let mut translator = TranslationDispatcher::new(context);
    let terminal = serde_json::json!({
        "type": "response.incomplete",
        "response": {
            "id": "resp_1",
            "status": "incomplete",
            "output": [
                {
                    "id": "tsc_native",
                    "type": "tool_search_call",
                    "call_id": "call_native",
                    "execution": "client",
                    "arguments": {},
                    "status": "incomplete"
                },
                {
                    "id": "fc_synthetic",
                    "type": "function_call",
                    "call_id": "call_synthetic",
                    "name": "tool_search",
                    "arguments": "",
                    "status": "in_progress"
                }
            ]
        }
    });

    let error = RoundIngestion::translate_line(&mut accumulator, SseLine::parse(&sse(&terminal)), &mut translator)
        .expect_err("terminal response must not contain two search calls");
    assert!(matches!(
        error,
        ExecutorError::Tool(crate::tool::ToolError::InvalidUpstreamToolSearch)
    ));
}

#[test]
fn native_done_cannot_reuse_synthetic_identity_after_terminal_event() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("tool_search".to_owned(), ToolType::ToolSearch)]));
    let mut translator = TranslationDispatcher::new(context);
    for event in [
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "fc_search", "type": "function_call", "call_id": "call_search",
                "name": "tool_search", "arguments": "", "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_1", "status": "incomplete",
                "output": [{
                    "id": "fc_search", "type": "function_call", "call_id": "call_search",
                    "name": "tool_search", "arguments": "", "status": "in_progress"
                }]
            }
        }),
    ] {
        translate(&mut accumulator, &mut translator, &event);
    }
    let native_done = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "id": "tsc_search", "type": "tool_search_call", "call_id": "call_search",
            "execution": "client", "arguments": {}, "status": "completed"
        }
    });

    let error = RoundIngestion::translate_line(&mut accumulator, SseLine::parse(&sse(&native_done)), &mut translator)
        .expect_err("native done must not reuse a synthetic call identity");
    assert!(matches!(
        error,
        ExecutorError::Tool(crate::tool::ToolError::InvalidUpstreamToolSearch)
    ));
}

#[test]
fn synthetic_tool_search_rejects_non_string_name_in_buffered_added_item() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("tool_search".to_owned(), ToolType::ToolSearch)]));
    let mut translator = TranslationDispatcher::new(context);
    translate(
        &mut accumulator,
        &mut translator,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "fc_search", "type": "function_call", "call_id": "call_search",
                "name": 7, "arguments": "", "status": "in_progress"
            }
        }),
    );
    let arguments_done = serde_json::json!({
        "type": "response.function_call_arguments.done",
        "output_index": 0,
        "item_id": "fc_search",
        "call_id": "call_search",
        "name": "tool_search",
        "arguments": "{}"
    });

    let error =
        RoundIngestion::translate_line(&mut accumulator, SseLine::parse(&sse(&arguments_done)), &mut translator)
            .expect_err("a malformed buffered name must not be overwritten");
    assert!(matches!(
        error,
        ExecutorError::Tool(crate::tool::ToolError::InvalidUpstreamToolSearch)
    ));
}

#[test]
fn native_tool_search_frames_pass_through_and_accumulate_natively() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::new());
    let mut translator = TranslationDispatcher::new(context);
    let events = [
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "tsc_native",
                "type": "tool_search_call",
                "call_id": "call_search",
                "execution": "client",
                "arguments": {},
                "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "tsc_native",
                "type": "tool_search_call",
                "call_id": "call_search",
                "execution": "client",
                "arguments": ["weather", "timezone"],
                "status": "completed"
            }
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "status": "completed", "output": []}
        }),
    ];

    let frames = events
        .iter()
        .flat_map(|event| translate(&mut accumulator, &mut translator, event).frames)
        .collect::<Vec<_>>();
    translator.finish().expect("completed native lifecycle");
    assert_eq!(frames[0].wire.rest["item"]["type"], "tool_search_call");
    assert_eq!(frames[1].wire.rest["item"]["type"], "tool_search_call");

    let payload = accumulator.finalize("test", None, None);
    let [OutputItem::ToolSearchCall(call)] = payload.output.as_slice() else {
        panic!("native tool_search_call must remain typed");
    };
    assert_eq!(call.arguments, serde_json::json!(["weather", "timezone"]));
}

#[test]
fn oversized_native_tool_search_done_arguments_are_rejected() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::new());
    let mut translator = TranslationDispatcher::new(context);
    let done = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "id": "tsc_native",
            "type": "tool_search_call",
            "call_id": "call_search",
            "execution": "client",
            "arguments": {"query": "x".repeat(MAX_PENDING_FUNCTION_BYTES)},
            "status": "completed"
        }
    });

    let error = RoundIngestion::translate_line(&mut accumulator, SseLine::parse(&sse(&done)), &mut translator)
        .expect_err("oversized native arguments must fail");
    assert!(error.to_string().contains("function-call SSE exceeded"));
}

#[test]
fn oversized_synthetic_tool_search_done_arguments_are_rejected() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("tool_search".to_owned(), ToolType::ToolSearch)]));
    let mut translator = TranslationDispatcher::new(context);
    let done = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "id": "fc_search",
            "type": "function_call",
            "call_id": "call_search",
            "name": "tool_search",
            "arguments": format!("{{\"query\":\"{}\"}}", "x".repeat(MAX_PENDING_FUNCTION_BYTES)),
            "status": "completed"
        }
    });

    let error = RoundIngestion::translate_line(&mut accumulator, SseLine::parse(&sse(&done)), &mut translator)
        .expect_err("oversized synthetic arguments must fail");
    assert!(error.to_string().contains("function-call SSE exceeded"));
}

#[test]
fn successful_eof_rejects_unfinished_synthetic_and_native_searches() {
    let synthetic_context = test_context(HashMap::from([("tool_search".to_owned(), ToolType::ToolSearch)]));
    let mut synthetic_accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let mut synthetic = TranslationDispatcher::new(synthetic_context);
    translate(
        &mut synthetic_accumulator,
        &mut synthetic,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "fc_search", "type": "function_call", "call_id": "call_search",
                "name": "tool_search", "arguments": "", "status": "in_progress"
            }
        }),
    );
    assert!(synthetic.finish().is_err());

    let native_context = test_context(HashMap::new());
    let mut native_accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let mut native = TranslationDispatcher::new(native_context);
    translate(
        &mut native_accumulator,
        &mut native,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "tsc_native", "type": "tool_search_call", "call_id": "call_search",
                "execution": "client", "arguments": {}, "status": "in_progress"
            }
        }),
    );
    assert!(native.finish().is_err());
}

#[test]
fn incomplete_stream_preserves_unfinished_synthetic_search() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("tool_search".to_owned(), ToolType::ToolSearch)]));
    let mut translator = TranslationDispatcher::new(context);
    let mut public_frames = Vec::new();
    for event in [
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "fc_search", "type": "function_call", "call_id": "call_search",
                "name": "tool_search", "arguments": "", "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "fc_search", "type": "function_call", "call_id": "call_search",
                "name": "tool_search", "arguments": "{\"query\":", "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.incomplete",
            "response": {"id": "resp_1", "status": "incomplete", "output": []}
        }),
    ] {
        public_frames.extend(translate(&mut accumulator, &mut translator, &event).frames);
    }

    let outcome = translator.finish().expect("aborted lifecycle may remain unfinished");
    assert_eq!(
        outcome.unfinished_tool_search_item_ids,
        HashSet::from(["fc_search".to_owned()])
    );
    let mut payload = accumulator.finalize("test", None, None);
    outcome
        .normalize_response_output(&mut payload.output, crate::types::event::ResponseStatus::Incomplete)
        .expect("unfinished synthetic call is preserved as incomplete");
    let [OutputItem::ToolSearchCall(call)] = payload.output.as_slice() else {
        panic!("unfinished synthetic call must use the public tool-search shape");
    };
    let added = public_frames
        .iter()
        .find(|frame| frame.event_type == SSEEventType::OutputItemAdded)
        .expect("public added frame");
    assert_eq!(call.id, added.wire.rest["item"]["id"]);
    assert_eq!(call.call_id, added.wire.rest["item"]["call_id"]);
    assert_eq!(call.arguments, serde_json::json!({}));
    assert_eq!(call.status, crate::types::tools::ToolSearchStatus::Incomplete);
    assert!(payload.output[0].to_input_item().is_none());
}

#[test]
fn incomplete_stream_preserves_native_search_and_terminal_item_parity() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::new());
    let mut translator = TranslationDispatcher::new(context);
    let mut public_frames = Vec::new();
    for event in [
        serde_json::json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {
                "id": "tsc_native", "type": "tool_search_call", "call_id": "call_search",
                "execution": "client", "arguments": {}, "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {
                "id": "tsc_native", "type": "tool_search_call", "call_id": "call_search",
                "execution": "client", "arguments": {"query": "weather"}, "status": "incomplete"
            }
        }),
        serde_json::json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_1", "status": "incomplete",
                "output": [{
                    "id": "tsc_native", "type": "tool_search_call", "call_id": "call_search",
                    "execution": "client", "arguments": {"query": "weather"}, "status": "incomplete"
                }]
            }
        }),
    ] {
        public_frames.extend(translate(&mut accumulator, &mut translator, &event).frames);
    }

    let outcome = translator.finish().expect("incomplete native lifecycle");
    let mut payload = accumulator.finalize("test", None, None);
    outcome
        .normalize_response_output(&mut payload.output, crate::types::event::ResponseStatus::Incomplete)
        .expect("incomplete native call remains public");
    let [OutputItem::ToolSearchCall(call)] = payload.output.as_slice() else {
        panic!("native call must remain typed");
    };
    let done = public_frames
        .iter()
        .find(|frame| frame.event_type == SSEEventType::OutputItemDone)
        .expect("public done frame");
    assert_eq!(
        serde_json::to_value(&payload.output[0]).unwrap(),
        done.wire.rest["item"]
    );
    assert_eq!(call.status, crate::types::tools::ToolSearchStatus::Incomplete);
    assert!(payload.output[0].to_input_item().is_none());
}

#[test]
fn aborted_stream_discards_unnamed_search_candidate_with_empty_raw_id() {
    for (terminal_event, terminal_status, response_status) in [
        (
            "response.incomplete",
            "incomplete",
            crate::types::event::ResponseStatus::Incomplete,
        ),
        ("response.failed", "failed", crate::types::event::ResponseStatus::Error),
    ] {
        let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
        let context = test_context(HashMap::from([("tool_search".to_owned(), ToolType::ToolSearch)]));
        let mut translator = TranslationDispatcher::new(context);
        for event in [
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "", "type": "function_call", "call_id": "call_search",
                    "arguments": "", "status": "in_progress"
                }
            }),
            serde_json::json!({
                "type": terminal_event,
                "response": {"id": "resp_1", "status": terminal_status, "output": []}
            }),
        ] {
            translate(&mut accumulator, &mut translator, &event);
        }

        let outcome = translator
            .finish()
            .expect("aborted lifecycle may leave the call unnamed");
        assert_eq!(outcome.unfinished_tool_search_item_ids.len(), 1);
        let internal_item_id = outcome
            .unfinished_tool_search_item_ids
            .iter()
            .next()
            .expect("accumulator-generated item id");
        assert!(internal_item_id.starts_with("fc_"));

        let mut payload = accumulator.finalize("test", None, None);
        let [OutputItem::FunctionCall(call)] = payload.output.as_slice() else {
            panic!("unfinished unnamed call must be accumulated as a function call");
        };
        assert!(call.name.is_empty());
        assert_eq!(&call.id, internal_item_id);
        outcome
            .normalize_response_output(&mut payload.output, response_status)
            .expect("unfinished unnamed search candidate is discarded");
        assert!(payload.output.is_empty());
    }
}

#[test]
fn aborted_stream_discards_unfinished_native_search() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::new());
    let mut translator = TranslationDispatcher::new(context);
    for event in [
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "tsc_native", "type": "tool_search_call", "call_id": "call_search",
                "execution": "client", "arguments": {}, "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "tsc_native", "type": "tool_search_call", "call_id": "call_search",
                "execution": "client", "arguments": {}, "status": "incomplete"
            }
        }),
        serde_json::json!({
            "type": "response.failed",
            "response": {"id": "resp_1", "status": "failed", "output": []}
        }),
    ] {
        translate(&mut accumulator, &mut translator, &event);
    }

    let outcome = translator
        .finish()
        .expect("aborted native lifecycle may remain unfinished");
    assert!(outcome.unfinished_tool_search_item_ids.is_empty());
    let mut payload = accumulator.finalize("test", None, None);
    outcome
        .normalize_response_output(&mut payload.output, crate::types::event::ResponseStatus::Error)
        .expect("unfinished native call is discarded");
    assert!(payload.output.is_empty());
}

#[test]
fn unresolved_ordinary_function_does_not_become_tool_search_failure() {
    let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
    let context = test_context(HashMap::from([("echo".to_owned(), ToolType::Function)]));
    let mut translator = TranslationDispatcher::new(context);
    translate(
        &mut accumulator,
        &mut translator,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "fc_ordinary", "type": "function_call", "call_id": "call_ordinary",
                "arguments": "", "status": "in_progress"
            }
        }),
    );

    translator
        .finish()
        .expect("ordinary unnamed call is not a search candidate");
}

#[test]
fn snapshot_preserves_unknown_names_and_active_search_override() {
    for active in [false, true] {
        let context = TranslationContext::new(
            HashMap::from([("tool_search".to_owned(), ToolType::Function)]),
            HashSet::new(),
            active,
        );
        assert_eq!(context.tool_type("unknown"), ToolType::Function);
        assert_eq!(
            context.tool_type("tool_search"),
            if active {
                ToolType::ToolSearch
            } else {
                ToolType::Function
            }
        );
    }
}

#[test]
fn unnamed_gateway_calls_release_pending_frames_without_losing_deferral() {
    for kind in [
        ToolType::Mcp,
        ToolType::WebSearch,
        ToolType::FileSearch,
        ToolType::CodeInterpreter,
    ] {
        for resolve_at_arguments_done in [false, true] {
            let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
            let mut dispatcher = TranslationDispatcher::new(test_context(HashMap::from([("lookup".to_owned(), kind)])));
            for (index, id) in [(1, "fc_gateway"), (0, "fc_client")] {
                for event in [
                    serde_json::json!({"type":"response.output_item.added", "output_index":index,
                        "item":{"id":id,"type":"function_call","call_id":id,"arguments":"","status":"in_progress"}}),
                    serde_json::json!({"type":"response.function_call_arguments.delta", "output_index":index,"item_id":id,"delta":"{}"}),
                ] {
                    let result = translate(&mut accumulator, &mut dispatcher, &event);
                    assert!(result.frames.is_empty());
                    assert_eq!(result.defer_from_output_index, Some(index));
                }
            }
            if resolve_at_arguments_done {
                let result = translate(
                    &mut accumulator,
                    &mut dispatcher,
                    &serde_json::json!({
                        "type":"response.function_call_arguments.done", "output_index":1,
                        "item_id":"fc_gateway", "name":"lookup", "arguments":"{}"
                    }),
                );
                assert!(result.frames.is_empty());
                assert_eq!(result.defer_from_output_index, Some(0));
            }
            let gateway_done = translate(
                &mut accumulator,
                &mut dispatcher,
                &serde_json::json!({
                    "type":"response.output_item.done", "output_index":1,
                    "item":{"id":"fc_gateway","type":"function_call","name":"lookup","call_id":"fc_gateway","arguments":"{}","status":"completed"}
                }),
            );
            assert!(gateway_done.frames.is_empty());
            assert_eq!(gateway_done.defer_from_output_index, Some(0));

            let client_done = translate(
                &mut accumulator,
                &mut dispatcher,
                &serde_json::json!({
                    "type":"response.output_item.done", "output_index":0,
                    "item":{"id":"fc_client","type":"function_call","name":"echo","call_id":"fc_client","arguments":"{}","status":"completed"}
                }),
            );
            assert_eq!(client_done.frames.len(), 3);
            assert!(
                client_done
                    .frames
                    .iter()
                    .all(|frame| frame.wire.output_index == Some(0))
            );
            // The gateway's pending buffer is gone, but execution still owns its public lifecycle.
            assert_eq!(client_done.defer_from_output_index, Some(1));
            dispatcher.finish().expect("resolved gateway and client calls");
        }
    }
}

#[test]
fn every_gateway_tool_suppresses_private_events_and_retains_deferral() {
    for kind in [
        ToolType::Mcp,
        ToolType::WebSearch,
        ToolType::FileSearch,
        ToolType::CodeInterpreter,
    ] {
        let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
        let mut dispatcher = TranslationDispatcher::new(test_context(HashMap::from([("lookup".to_owned(), kind)])));
        for event in [
            serde_json::json!({"type":"response.output_item.added", "output_index":3,
                "item":{"id":"fc_1","type":"function_call","name":"lookup","call_id":"call_1","arguments":"","status":"in_progress"}}),
            serde_json::json!({"type":"response.function_call_arguments.delta", "output_index":3,"item_id":"fc_1","delta":"{}"}),
            serde_json::json!({"type":"response.function_call_arguments.done", "output_index":3,"item_id":"fc_1","name":"lookup","arguments":"{}"}),
            serde_json::json!({"type":"response.output_item.done", "output_index":3,
                "item":{"id":"fc_1","type":"function_call","name":"lookup","call_id":"call_1","arguments":"{}","status":"completed"}}),
        ] {
            let result = translate(&mut accumulator, &mut dispatcher, &event);
            assert!(result.frames.is_empty(), "{kind:?} leaked a private event");
            assert_eq!(result.defer_from_output_index, Some(3));
        }
        dispatcher.finish().expect("gateway translation completes");
    }
}

#[test]
fn ordinary_and_namespace_function_translators_preserve_wire_events() {
    for (name, types) in [
        ("unknown", HashMap::new()),
        (
            "tool_search",
            HashMap::from([("tool_search".to_owned(), ToolType::Function)]),
        ),
        (
            "travel.lookup",
            HashMap::from([("travel.lookup".to_owned(), ToolType::CodexNamespace)]),
        ),
    ] {
        let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
        let mut dispatcher = TranslationDispatcher::new(test_context(types));
        for event in [
            serde_json::json!({"type":"response.output_item.added", "output_index":0,
                "item":{"id":"fc_1","type":"function_call","name":name,"call_id":"call_1","arguments":"","status":"in_progress"}}),
            serde_json::json!({"type":"response.function_call_arguments.delta", "output_index":0,"item_id":"fc_1","delta":"{}"}),
            serde_json::json!({"type":"response.function_call_arguments.done", "output_index":0,"item_id":"fc_1","name":name,"arguments":"{}"}),
            serde_json::json!({"type":"response.output_item.done", "output_index":0,
                "item":{"id":"fc_1","type":"function_call","name":name,"call_id":"call_1","arguments":"{}","status":"completed"}}),
        ] {
            let result = translate(&mut accumulator, &mut dispatcher, &event);
            assert_eq!(result.frames.len(), 1);
            assert_eq!(serde_json::to_value(&result.frames[0].wire).unwrap(), event);
            assert_eq!(result.defer_from_output_index, None);
        }
        dispatcher.finish().expect("public function translation completes");
    }
}

#[test]
fn withheld_names_are_rejected_at_every_translation_entry_and_final_projection() {
    let item = serde_json::json!({"id":"fc_1","type":"function_call","name":"hidden","call_id":"call_1","arguments":"{}","status":"completed"});
    for event in [
        serde_json::json!({"type":"response.output_item.added", "output_index":0,"item":item}),
        serde_json::json!({"type":"response.function_call_arguments.done", "output_index":0,"item_id":"fc_1","name":"hidden","arguments":"{}"}),
        serde_json::json!({"type":"response.output_item.done", "output_index":0,"item":item}),
        serde_json::json!({"type":"response.completed", "response":{"id":"resp_1","status":"completed","output":[item]}}),
    ] {
        let context = TranslationContext::new(HashMap::new(), HashSet::from(["hidden".to_owned()]), true);
        let mut dispatcher = TranslationDispatcher::new(context);
        let frame = crate::events::normalize_sse_line(&sse(&event)).expect("normalized event");
        let error = dispatcher
            .translate(frame, None)
            .expect_err("withheld name must be rejected before dispatch");
        assert!(matches!(error, ExecutorError::Tool(_)));
    }
    let context = TranslationContext::new(HashMap::new(), HashSet::from(["hidden".to_owned()]), true);
    let mut output = vec![serde_json::from_value(item).unwrap()];
    assert!(
        context
            .normalize_response_output(
                &mut output,
                crate::types::event::ResponseStatus::Completed,
                &HashSet::new()
            )
            .is_err()
    );
}

#[test]
fn public_catalog_distinguishes_inactive_search_from_an_empty_active_catalog() {
    for catalog in [None, Some(Vec::new())] {
        let active = catalog.is_some();
        let context = TranslationContext::default().with_response_metadata(None, None, catalog, None);
        let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
        let mut dispatcher = TranslationDispatcher::new(context);
        let line = r#"data: {"type":"response.created","response":{"id":"upstream","status":"in_progress","tools":[{"type":"function","name":"ordinary"}]}}"#;
        let translated = RoundIngestion::translate_line(&mut accumulator, SseLine::parse(line), &mut dispatcher)
            .unwrap()
            .unwrap();
        let tools = &translated.frames[0].wire.rest["response"]["tools"];
        if active {
            assert_eq!(tools, &serde_json::json!([]));
        } else {
            assert_eq!(tools[0]["name"], "ordinary");
        }
        let mut payload = accumulator.finalize("test", None, None);
        dispatcher
            .finish()
            .unwrap()
            .normalize_response_payload(&mut payload)
            .unwrap();
        assert_eq!(payload.tools.is_some(), active);
    }
}
