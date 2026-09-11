use crate::executor::pipeline::RoundIngestion;
use crate::executor::translate::TranslationDispatcher;
use serde_json::{Value, json};

use super::*;

fn push(acc: &mut ResponseAccumulator, event: &Value, strict: bool) -> ExecutorResult<Option<EventFrame>> {
    let line = format!("data: {event}");
    assert_eq!(acc.validation == Validation::Strict, strict);
    acc.process_line(SseLine::parse(&line))
}

fn accumulator(strict: bool) -> ResponseAccumulator {
    let validation = if strict {
        Validation::Strict
    } else {
        Validation::Lenient
    };
    let mut acc = ResponseAccumulator::with_validation("resp_1".to_owned(), None, validation);
    for event_type in ["response.created", "response.in_progress"] {
        push(
            &mut acc,
            &json!({"type":event_type,"response":{"id":"resp_1","status":"in_progress"}}),
            strict,
        )
        .expect("valid response lifecycle");
    }
    acc
}

fn function_item(id: &str) -> Value {
    json!({"type":"function_call","id":id,"call_id":"call_1","name":"lookup","arguments":"{}","status":"completed"})
}

#[test]
fn invalid_output_indexes_are_rejected_under_both_policies() {
    let invalid = [
        json!(-1),
        json!(1.5),
        json!(1.0),
        json!("0"),
        json!(true),
        Value::Null,
        json!(u64::from(u32::MAX) + 1),
        json!(u64::MAX),
    ];
    for strict in [false, true] {
        for index in &invalid {
            let mut acc = accumulator(strict);
            let event = json!({"type":"response.output_item.added","output_index":index,"item":function_item("fc_1")});
            let error = push(&mut acc, &event, strict).expect_err("invalid index must not be skipped or coerced");
            assert!(error.to_string().contains("output_index"), "{index}: {error}");
            acc.finalize_all();
            assert!(acc.output.is_empty());
        }
    }
}

#[test]
fn a_bound_id_cannot_change_even_when_the_index_matches() {
    for strict in [false, true] {
        let mut acc = accumulator(strict);
        push(
            &mut acc,
            &json!({"type":"response.output_item.added","output_index":3,"item":function_item("fc_1")}),
            strict,
        )
        .expect("valid added item");
        for event in [
            json!({"type":"response.function_call_arguments.delta","output_index":3,"item_id":"fc_changed","delta":"bad"}),
            json!({"type":"response.output_item.done","output_index":3,"item":function_item("fc_changed")}),
        ] {
            push(&mut acc, &event, strict).expect_err("bound identity must be stable");
        }
        push(
            &mut acc,
            &json!({"type":"response.output_item.done","output_index":3,"item":function_item("fc_1")}),
            strict,
        )
        .expect("rejected identity changes leave the original slot intact");
        acc.finalize_all();
        assert_eq!(
            serde_json::to_value(acc.output).unwrap(),
            json!([function_item("fc_1")])
        );
    }
}

#[test]
fn strict_items_require_both_index_and_canonical_id() {
    for event_type in [
        "response.output_item.added",
        "response.output_item.done",
        "response.function_call_arguments.delta",
    ] {
        for missing_index in [false, true] {
            let mut acc = accumulator(true);
            let mut event = if event_type.ends_with("delta") {
                json!({"type":event_type,"output_index":0,"item_id":"fc_1","delta":"{}"})
            } else {
                json!({"type":event_type,"output_index":0,"item":function_item("fc_1")})
            };
            if missing_index {
                event.as_object_mut().unwrap().remove("output_index");
            } else if event_type.ends_with("delta") {
                event.as_object_mut().unwrap().remove("item_id");
            } else {
                event["item"].as_object_mut().unwrap().remove("id");
            }
            let error = push(&mut acc, &event, true).expect_err("required identity field is absent");
            assert!(
                error
                    .to_string()
                    .contains(if missing_index { "output_index" } else { "id" }),
                "{error}"
            );
        }
    }
}

#[test]
fn late_bound_id_recovers_subsequent_missing_indexes() {
    let mut acc = accumulator(false);
    for (index, id) in [(0, "fc_zero"), (7, "")] {
        push(
            &mut acc,
            &json!({"type":"response.output_item.added","output_index":index,"item":function_item(id)}),
            false,
        )
        .expect("valid added item");
    }
    push(&mut acc, &json!({"type":"response.function_call_arguments.delta","output_index":7,"item_id":"fc_late","delta":"{\"q\":"}), false)
        .expect("bind ID using the supplied index");
    let frame = push(
        &mut acc,
        &json!({"type":"response.function_call_arguments.delta","item_id":"fc_late","delta":"1}"}),
        false,
    )
    .expect("recover by bound ID")
    .expect("accepted delta");
    assert_eq!(frame.wire.output_index, Some(7));
    assert_eq!(frame.output_index(), Some(7));
    let mut item = function_item("fc_late");
    item["arguments"] = json!("");
    let done = json!({"type":"response.output_item.done","item":item});
    let frame = push(&mut acc, &done, false)
        .expect("done resolves by ID")
        .expect("first completion");
    assert_eq!(frame.wire.output_index, Some(7));
    assert!(push(&mut acc, &done, false).expect("equivalent duplicate").is_none());
    acc.finalize_all();
    let output = serde_json::to_value(acc.output).unwrap();
    assert_eq!(output[0]["id"], "fc_zero");
    assert_eq!(output[0]["arguments"], "");
    assert_eq!(output[1]["id"], "fc_late");
    assert_eq!(output[1]["arguments"], "{\"q\":1}");
}

#[test]
fn recovered_indexes_reach_translation_and_skip_occupied_indexes() {
    let mut acc = accumulator(false);
    let context = crate::executor::translate::TranslationContext::default();
    let mut translator = TranslationDispatcher::new(context);
    for (supplied, expected, id) in [
        (Some(0), 0, "fc_zero"),
        (Some(u32::MAX), u32::MAX, "fc_max"),
        (None, 1, "fc_one"),
        (None, 2, "fc_two"),
    ] {
        let mut event = json!({"type":"response.output_item.added","item":function_item(id)});
        if let Some(index) = supplied {
            event["output_index"] = json!(index);
        }
        let translated =
            RoundIngestion::translate_line(&mut acc, SseLine::parse(&format!("data: {event}")), &mut translator)
                .expect("valid item")
                .expect("item is emitted");
        assert_eq!(translated.frames.len(), 1);
        assert_eq!(translated.frames[0].wire.output_index, Some(u64::from(expected)));
        let delta = json!({"type":"response.function_call_arguments.delta","item_id":id,"delta":"{}"});
        let translated =
            RoundIngestion::translate_line(&mut acc, SseLine::parse(&format!("data: {delta}")), &mut translator)
                .expect("recover known index")
                .expect("delta is emitted");
        assert_eq!(translated.frames[0].wire.output_index, Some(u64::from(expected)));
    }
    acc.finalize_all();
    assert_eq!(
        acc.output.iter().filter_map(OutputItem::id).collect::<Vec<_>>(),
        ["fc_zero", "fc_one", "fc_two", "fc_max"]
    );
}

#[test]
fn active_and_completed_indexes_and_ids_cannot_be_reused() {
    for strict in [false, true] {
        for complete in [false, true] {
            for (index, id) in [(0, "fc_other"), (1, "fc_1"), (0, "fc_1")] {
                let mut acc = accumulator(strict);
                let item = function_item("fc_1");
                push(
                    &mut acc,
                    &json!({"type":"response.output_item.added","output_index":0,"item":item}),
                    strict,
                )
                .unwrap();
                if complete {
                    push(
                        &mut acc,
                        &json!({"type":"response.output_item.done","output_index":0,"item":item}),
                        strict,
                    )
                    .unwrap();
                }
                push(
                    &mut acc,
                    &json!({"type":"response.output_item.added","output_index":index,"item":function_item(id)}),
                    strict,
                )
                .expect_err("an occupied index or bound ID cannot open another slot");
                assert_eq!(acc.slots.len(), 1);
            }
        }
    }
}

#[test]
fn ambiguous_missing_index_does_not_guess_an_anonymous_slot() {
    for count in [1, 2] {
        let mut acc = accumulator(false);
        for index in 0..count {
            push(
                &mut acc,
                &json!({"type":"response.output_item.added","output_index":index,"item":function_item("")}),
                false,
            )
            .unwrap();
        }
        for event in [
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_unknown","delta":"bad"}),
            json!({"type":"response.output_item.done","item":function_item("fc_unknown")}),
        ] {
            let error = push(&mut acc, &event, false).expect_err("no known ID identifies the target");
            assert!(error.to_string().contains("ambiguous"), "{error}");
        }
        assert_eq!(acc.slots.len(), count);
    }
}

#[test]
fn rejected_or_ignored_events_cannot_bind_an_id() {
    let mut acc = accumulator(false);
    for (index, id) in [(0, "fc_reserved"), (3, "")] {
        push(
            &mut acc,
            &json!({"type":"response.output_item.added","output_index":index,"item":function_item(id)}),
            false,
        )
        .unwrap();
    }
    push(&mut acc, &json!({"type":"response.function_call_arguments.delta","output_index":3,"item_id":"fc_reserved","delta":"bad"}), false)
        .expect_err("ID belongs to another index");
    assert!(push(&mut acc, &json!({"type":"response.output_text.delta","output_index":3,"item_id":"wrong_kind_id","delta":"bad","content_index":0}), false)
        .expect("wrong kind is ignored").is_none());
    push(
        &mut acc,
        &json!({"type":"response.function_call_arguments.delta","output_index":3,"item_id":"fc_bound","delta":"{}"}),
        false,
    )
    .expect("slot remains unbound until an accepted event");
    let frame = push(
        &mut acc,
        &json!({"type":"response.function_call_arguments.delta","item_id":"fc_bound","delta":" "}),
        false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(frame.wire.output_index, Some(3));
}

#[test]
fn generated_public_ids_are_not_upstream_lookup_keys() {
    let mut acc = accumulator(false);
    push(
        &mut acc,
        &json!({"type":"response.output_item.added","output_index":7,"item":function_item("")}),
        false,
    )
    .unwrap();
    let generated = acc.accumulated_function_call(7).unwrap().item.id.clone();
    let error = push(
        &mut acc,
        &json!({"type":"response.function_call_arguments.delta","item_id":generated,"delta":"bad"}),
        false,
    )
    .expect_err("a generated public ID cannot recover an upstream index");
    assert!(error.to_string().contains("ambiguous"));
    push(
        &mut acc,
        &json!({"type":"response.output_item.done","output_index":7,"item":function_item("fc_bound")}),
        false,
    )
    .expect("a supplied index permits canonical ID binding");
    acc.finalize_all();
    assert_eq!(acc.output[0].id(), Some("fc_bound"));
}

#[test]
fn done_only_items_allocate_indexes_and_raw_events_recover_known_indexes() {
    let mut acc = accumulator(false);
    for (index, id) in [(0, "fc_first"), (1, "fc_second")] {
        let event = json!({"type":"response.output_item.done","item":function_item(id)});
        let frame = push(&mut acc, &event, false).unwrap().unwrap();
        assert_eq!(frame.wire.output_index, Some(index));
        assert!(push(&mut acc, &event, false).unwrap().is_none());
    }
    push(
        &mut acc,
        &json!({"type":"response.output_item.added","output_index":8,"item":{"type":"mcp_call","id":"mcp_1"}}),
        false,
    )
    .unwrap();
    let frame = push(
        &mut acc,
        &json!({"type":"response.mcp_call.in_progress","item_id":"mcp_1"}),
        false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(frame.wire.output_index, Some(8));
    let EventPayload::Raw(raw) = frame.payload else {
        panic!("expected raw status payload");
    };
    assert_eq!(raw["output_index"], 8);
}

#[test]
fn unnamed_translated_calls_reject_changes_to_bound_ids() {
    use crate::ToolType;

    for tool_type in [ToolType::Function, ToolType::Custom] {
        let mut acc = accumulator(false);
        let context = crate::executor::translate::TranslationContext::new(
            HashMap::from([("lookup".to_owned(), tool_type)]),
            std::collections::HashSet::new(),
            false,
        );
        let mut translator = TranslationDispatcher::new(context);
        let mut item = function_item("fc_transient");
        item.as_object_mut().unwrap().remove("name");
        let added = json!({"type":"response.output_item.added","output_index":1,"item":item});
        RoundIngestion::translate_line(&mut acc, SseLine::parse(&format!("data: {added}")), &mut translator).unwrap();
        let done = json!({"type":"response.output_item.done","output_index":1,"item":function_item("fc_stable")});
        let error = RoundIngestion::translate_line(&mut acc, SseLine::parse(&format!("data: {done}")), &mut translator)
            .expect_err("changing a bound ID must fail before translation");
        assert!(error.to_string().contains("does not match"));
    }
}
