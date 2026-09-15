//! Retained-byte accounting through the accumulator's public transitions.
//!
//! Every test drives `ResponseAccumulator` with a budget, so the charges under
//! test are the ones synchronous ingestion actually makes, not helper output.

use serde_json::{Value, json};

use super::*;
use crate::executor::error::ResourceLimit;
use crate::executor::response_budget::{ExecutorResponseBudget, RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize};

fn budgeted(limit: usize, validation: Validation) -> (ResponseAccumulator, ExecutorResponseBudget) {
    let budget = ExecutorResponseBudget::with_limit(limit);
    let acc =
        ResponseAccumulator::with_validation_and_budget("resp_1".to_owned(), None, validation, Some(budget.clone()));
    (acc, budget)
}

fn line(event: &Value) -> ClassifiedSseLine {
    SseLine::parse(&format!("data: {event}"))
}

fn feed(acc: &mut ResponseAccumulator, events: &[Value]) -> ExecutorResult<()> {
    for event in events {
        acc.process_line(line(event))?;
    }
    Ok(())
}

fn assert_budget_exceeded(error: &ExecutorError) {
    assert!(
        matches!(
            error,
            ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::ResponseBudget,
                ..
            }
        ),
        "expected retained budget rejection, got {error}"
    );
}

fn created() -> Value {
    json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}})
}

fn completed(output: &[Value]) -> Value {
    json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "output": output}})
}

fn message_added(id: &str) -> Value {
    json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": id, "type": "message", "role": "assistant", "status": "in_progress", "content": []}
    })
}

fn text_delta(id: &str, content_index: u32, delta: &str) -> Value {
    json!({
        "type": "response.output_text.delta", "output_index": 0, "item_id": id,
        "content_index": content_index, "delta": delta
    })
}

fn text_done(id: &str, content_index: u32, text: &str) -> Value {
    json!({
        "type": "response.output_text.done", "output_index": 0, "item_id": id,
        "content_index": content_index, "text": text
    })
}

fn message_done(id: &str, parts: &[Value]) -> Value {
    json!({
        "type": "response.output_item.done", "output_index": 0,
        "item": {"id": id, "type": "message", "role": "assistant", "status": "completed", "content": parts}
    })
}

fn text_part(text: &str, annotations: &[Value]) -> Value {
    json!({"type": "output_text", "text": text, "annotations": annotations})
}

fn function_call_added(id: &str) -> Value {
    json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": id, "type": "function_call", "call_id": "", "name": "", "arguments": "", "status": "in_progress"}
    })
}

fn function_args_delta(id: &str, delta: &str) -> Value {
    json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": id, "delta": delta})
}

fn function_args_done(id: &str, arguments: &str, name: &str, call_id: &str) -> Value {
    json!({
        "type": "response.function_call_arguments.done", "output_index": 0, "item_id": id,
        "arguments": arguments, "name": name, "call_id": call_id
    })
}

fn function_call_item(id: &str, arguments: &str, name: &str, call_id: &str) -> Value {
    json!({
        "id": id, "type": "function_call", "call_id": call_id, "name": name,
        "arguments": arguments, "status": "completed"
    })
}

fn output_item_done(item: &Value) -> Value {
    json!({"type": "response.output_item.done", "output_index": 0, "item": item})
}

fn finished_output(acc: ResponseAccumulator) -> Vec<OutputItem> {
    acc.finish("test", None, None).expect("stream finishes").output
}

#[test]
fn delta_streamed_and_done_only_messages_charge_identical_bytes() {
    let text = "x".repeat(5000);
    let (mut streamed, streamed_budget) = budgeted(1 << 20, Validation::Lenient);
    feed(
        &mut streamed,
        &[
            created(),
            message_added("msg_1"),
            text_delta("msg_1", 0, &text[..2000]),
            text_delta("msg_1", 0, &text[2000..]),
            text_done("msg_1", 0, &text),
            completed(&[]),
        ],
    )
    .unwrap();
    let (mut done_only, done_only_budget) = budgeted(1 << 20, Validation::Lenient);
    feed(
        &mut done_only,
        &[
            created(),
            message_added("msg_1"),
            text_done("msg_1", 0, &text),
            completed(&[]),
        ],
    )
    .unwrap();

    assert_eq!(streamed_budget.used(), done_only_budget.used());
    let output = finished_output(streamed);
    assert_eq!(
        streamed_budget.used(),
        RETAINED_CONTAINER_OVERHEAD_BYTES + "resp_1".len() + output[0].retained_bytes(),
        "the charge equals the measured retained response"
    );
    assert_eq!(
        serde_json::to_value(&output).unwrap(),
        serde_json::to_value(finished_output(done_only)).unwrap()
    );
}

#[test]
fn delta_streamed_and_done_only_function_arguments_charge_identical_bytes() {
    let arguments = format!("{{\"q\":\"{}\"}}", "y".repeat(3000));
    let (mut streamed, streamed_budget) = budgeted(1 << 20, Validation::Lenient);
    feed(
        &mut streamed,
        &[
            created(),
            function_call_added("fc_1"),
            function_args_delta("fc_1", &arguments[..1000]),
            function_args_delta("fc_1", &arguments[1000..]),
            function_args_done("fc_1", &arguments, "lookup", "call_1"),
            output_item_done(&function_call_item("fc_1", &arguments, "lookup", "call_1")),
            completed(&[]),
        ],
    )
    .unwrap();
    let (mut done_only, done_only_budget) = budgeted(1 << 20, Validation::Lenient);
    feed(
        &mut done_only,
        &[
            created(),
            function_call_added("fc_1"),
            function_args_done("fc_1", &arguments, "lookup", "call_1"),
            output_item_done(&function_call_item("fc_1", &arguments, "lookup", "call_1")),
            completed(&[]),
        ],
    )
    .unwrap();

    assert_eq!(streamed_budget.used(), done_only_budget.used());
    let output = finished_output(streamed);
    assert_eq!(
        streamed_budget.used(),
        RETAINED_CONTAINER_OVERHEAD_BYTES + "resp_1".len() + output[0].retained_bytes()
    );
}

#[test]
fn repeated_completion_snapshots_charge_once() {
    let text = "z".repeat(4000);
    let done = message_done("msg_1", &[text_part(&text, &[])]);
    let (mut acc, budget) = budgeted(1 << 20, Validation::Lenient);
    feed(
        &mut acc,
        &[created(), message_added("msg_1"), text_delta("msg_1", 0, &text)],
    )
    .unwrap();
    let after_deltas = budget.used();
    feed(&mut acc, std::slice::from_ref(&done)).unwrap();
    let after_first_done = budget.used();
    assert_eq!(
        after_first_done - after_deltas,
        0,
        "the completion snapshot repeats streamed text and charges nothing new"
    );
    feed(&mut acc, &[done.clone(), done]).unwrap();
    assert_eq!(budget.used(), after_first_done, "repeated identical snapshots are free");
    feed(&mut acc, &[completed(&[])]).unwrap();
    let output = finished_output(acc);
    assert_eq!(
        budget.used(),
        RETAINED_CONTAINER_OVERHEAD_BYTES + "resp_1".len() + output[0].retained_bytes()
    );
}

#[test]
fn empty_multipart_entries_are_charged_and_exhaust_the_budget_promptly() {
    let limit = 4096;
    let (mut acc, budget) = budgeted(limit, Validation::Lenient);
    feed(&mut acc, &[created(), message_added("msg_1")]).unwrap();
    let opening = budget.used();
    let affordable = (limit - opening) / RETAINED_CONTAINER_OVERHEAD_BYTES;

    let mut rejected_at = None;
    for content_index in 0..100_000_u32 {
        if let Err(error) = acc.process_line(line(&text_delta("msg_1", content_index, ""))) {
            assert_budget_exceeded(&error);
            rejected_at = Some(content_index);
            break;
        }
    }
    let rejected_at = rejected_at.expect("100,000 empty parts must exceed a 4 KiB budget");
    assert_eq!(
        usize::try_from(rejected_at).unwrap(),
        affordable,
        "every empty part charges one container before it is inserted"
    );
    assert!(budget.used() <= limit);
}

#[test]
fn reasoning_streamed_indexes_charge_containers_and_reconcile_with_done_text() {
    let (mut acc, budget) = budgeted(1 << 20, Validation::Lenient);
    let added = json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "rs_1", "type": "reasoning", "summary": [], "content": [], "status": "in_progress"}
    });
    feed(&mut acc, &[created(), added]).unwrap();
    let opening = budget.used();
    for content_index in 0..3_u32 {
        acc.process_line(line(&json!({
            "type": "response.reasoning_text.delta", "output_index": 0, "item_id": "rs_1",
            "content_index": content_index, "delta": ""
        })))
        .unwrap();
    }
    assert_eq!(budget.used() - opening, 3 * RETAINED_CONTAINER_OVERHEAD_BYTES);

    acc.process_line(line(&json!({
        "type": "response.reasoning_text.delta", "output_index": 0, "item_id": "rs_1",
        "content_index": 0, "delta": "abc"
    })))
    .unwrap();
    acc.process_line(line(&json!({
        "type": "response.reasoning_text.done", "output_index": 0, "item_id": "rs_1",
        "content_index": 0, "text": "abcdef"
    })))
    .unwrap();
    // Container already charged with the first delta; done grows the text by 3.
    assert_eq!(
        budget.used() - opening,
        3 * RETAINED_CONTAINER_OVERHEAD_BYTES + "abcdef".len()
    );
}

#[test]
fn metadata_supplied_only_at_completion_is_charged_at_completion() {
    let (mut acc, budget) = budgeted(1 << 20, Validation::Lenient);
    feed(
        &mut acc,
        &[
            created(),
            function_call_added("fc_1"),
            function_args_delta("fc_1", "{}"),
        ],
    )
    .unwrap();
    let before = budget.used();
    feed(&mut acc, &[function_args_done("fc_1", "{}", "lookup_tool", "call_abc")]).unwrap();
    assert_eq!(
        budget.used() - before,
        "lookup_tool".len() + "call_abc".len(),
        "name and call_id arrive with the arguments completion and are charged there"
    );

    let annotation = json!({"type": "url_citation", "url": "https://example.com/a", "title": "A"});
    let (mut acc, budget) = budgeted(1 << 20, Validation::Lenient);
    feed(
        &mut acc,
        &[created(), message_added("msg_1"), text_delta("msg_1", 0, "cited")],
    )
    .unwrap();
    let before = budget.used();
    feed(
        &mut acc,
        &[message_done(
            "msg_1",
            &[text_part("cited", std::slice::from_ref(&annotation))],
        )],
    )
    .unwrap();
    assert_eq!(
        budget.used() - before,
        annotation.retained_bytes(),
        "annotations appear only in the completed snapshot and are charged there"
    );
}

#[test]
fn a_delta_is_rejected_before_the_retained_state_grows() {
    let (mut acc, budget) = budgeted(200, Validation::Lenient);
    feed(
        &mut acc,
        &[created(), message_added("msg_1"), text_delta("msg_1", 0, "short")],
    )
    .unwrap();
    let used = budget.used();
    let error = acc
        .process_line(line(&text_delta("msg_1", 0, &"y".repeat(500))))
        .expect_err("a delta past the budget is rejected");
    assert_budget_exceeded(&error);
    assert_eq!(budget.used(), used, "a rejected delta charges nothing");
    let Some(slot) = acc.slots.get(OutputIndex::new(0)) else {
        panic!("slot survives for inspection");
    };
    let SlotState::Active(ActiveItem::Message(state)) = &slot.state else {
        panic!("message still active");
    };
    assert_eq!(
        state.parts[&0].text, "short",
        "the rejected delta never entered the buffer"
    );
}

#[test]
fn oversized_annotations_are_rejected_on_both_ingestion_paths() {
    let annotation = json!({"type": "url_citation", "url": "https://example.com", "title": "x".repeat(100_000)});
    let item = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
        "content": [text_part("small", &[annotation])]
    });

    let (mut streamed, _) = budgeted(4096, Validation::Lenient);
    feed(
        &mut streamed,
        &[created(), message_added("msg_1"), text_delta("msg_1", 0, "small")],
    )
    .unwrap();
    let error = streamed
        .process_line(line(&output_item_done(&item)))
        .expect_err("streamed annotations past the budget are rejected at completion");
    assert_budget_exceeded(&error);

    let (mut json_path, _) = budgeted(4096, Validation::Strict);
    let body = json!({"id": "resp_1", "status": "completed", "output": [item]}).to_string();
    let error = json_path
        .load_json_body(&body)
        .expect_err("JSON annotations past the budget are rejected");
    assert_budget_exceeded(&error);
}

#[test]
fn oversized_compaction_content_is_rejected_on_both_ingestion_paths() {
    let item = json!({"id": "cmp_1", "type": "compaction", "encrypted_content": "c".repeat(100_000)});

    // The opening event carries only the identity; the content arrives with the
    // completion snapshot and is charged there.
    let (mut streamed, _) = budgeted(4096, Validation::Lenient);
    feed(
        &mut streamed,
        &[
            created(),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "cmp_1", "type": "compaction"}}),
        ],
    )
    .unwrap();
    let error = streamed
        .process_line(line(&output_item_done(&item)))
        .expect_err("streamed compaction content past the budget is rejected at completion");
    assert_budget_exceeded(&error);

    let (mut json_path, _) = budgeted(4096, Validation::Strict);
    let body = json!({"id": "resp_1", "status": "completed", "output": [item]}).to_string();
    let error = json_path
        .load_json_body(&body)
        .expect_err("JSON compaction content past the budget is rejected");
    assert_budget_exceeded(&error);
}

#[test]
fn oversized_tool_search_arguments_are_rejected_recursively() {
    let arguments = json!({"filters": {"terms": ["t".repeat(100_000)]}});
    let item = json!({
        "id": "ts_1", "type": "tool_search_call", "call_id": "call_1", "execution": "client",
        "status": "completed", "arguments": arguments
    });
    let (mut json_path, _) = budgeted(4096, Validation::Strict);
    let body = json!({"id": "resp_1", "status": "completed", "output": [item]}).to_string();
    let error = json_path
        .load_json_body(&body)
        .expect_err("nested tool-search arguments past the budget are rejected");
    assert_budget_exceeded(&error);
}
