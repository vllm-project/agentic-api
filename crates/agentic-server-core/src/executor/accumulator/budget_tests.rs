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
    let affordable = (limit - opening) / (RETAINED_CONTAINER_OVERHEAD_BYTES + "output_text".len());

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
        3 * RETAINED_CONTAINER_OVERHEAD_BYTES + "reasoning_text".len() + "abcdef".len()
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
    assert!(
        matches!(&state.parts[&0], super::active::MessagePart::Streaming { text, .. } if text == "short"),
        "the rejected delta never entered the buffer"
    );
}

#[test]
fn message_part_and_event_attribution_are_charged_before_retention() {
    let in_progress = json!({"type": "response.in_progress", "response": {"id": "resp_1", "status": "in_progress"}});
    for validation in [Validation::Strict, Validation::Lenient] {
        for part in [
            json!({"type": "input_text", "text": "x".repeat(1000)}),
            json!({"type": "output_text", "text": "", "logprobs": [{"token": "x".repeat(1000),
                "bytes": [], "logprob": -0.1, "top_logprobs": []}]}),
        ] {
            let (mut acc, budget) = budgeted(400, validation);
            feed(&mut acc, &[created(), in_progress.clone(), message_added("msg_1")]).unwrap();
            let used = budget.used();
            let error = feed(
                &mut acc,
                &[json!({"type": "response.content_part.done", "output_index": 0,
                "item_id": "msg_1", "content_index": 0, "part": part})],
            )
            .unwrap_err();
            assert_budget_exceeded(&error);
            assert_eq!(budget.used(), used);
            let SlotState::Active(ActiveItem::Message(state)) = &acc.slots.get(OutputIndex::new(0)).unwrap().state
            else {
                panic!("message still active");
            };
            assert!(state.parts.is_empty());
        }
        let (mut acc, _) = budgeted(400, validation);
        let mut opening = message_added("msg_1");
        opening["agent"] = json!({"agent_name": "x".repeat(1000)});
        let error = feed(&mut acc, &[created(), in_progress.clone(), opening]).unwrap_err();
        assert_budget_exceeded(&error);
        assert_eq!(acc.slots.len(), 0);
    }
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

fn reject_item_on_both_paths(item: &Value) {
    let (mut streamed, _) = budgeted(4096, Validation::Lenient);
    let mut added = item.clone();
    added["status"] = json!("in_progress");
    let error = feed(
        &mut streamed,
        &[
            created(),
            json!({"type":"response.output_item.added", "output_index":0, "item":added}),
            output_item_done(item),
        ],
    )
    .expect_err("oversized retained item must fail during SSE completion");
    assert_budget_exceeded(&error);
    let (mut json_path, _) = budgeted(4096, Validation::Lenient);
    let body = json!({"id":"resp_1", "status":"completed", "output":[item]}).to_string();
    let error = json_path
        .load_json_body(&body)
        .expect_err("oversized retained item must fail during JSON ingestion");
    assert_budget_exceeded(&error);
}

#[test]
fn nested_empty_json_values_exhaust_retained_budget_on_both_paths() {
    for annotation in [json!(vec![""; 100_000]), json!({"nested":[vec![""; 100_000]]})] {
        reject_item_on_both_paths(&json!({
            "id":"msg_1", "type":"message", "role":"assistant", "status":"completed",
            "content":[text_part("small", &[annotation])]
        }));
    }
}

#[test]
fn unrestricted_retained_strings_exhaust_budget_on_both_paths() {
    let huge = "x".repeat(100_000);
    for item in [
        json!({"id":"msg_1","type":"message","role":huge,"status":"completed","content":[]}),
        json!({"id":"msg_1","type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"","annotations":[],
                "logprobs":[{"token":huge,"bytes":[],"logprob":-0.5,"top_logprobs":[]}]}]}),
        json!({"id":"msg_1","type":"message","role":"assistant","status":"completed",
            "agent":{"agent_name":huge},"content":[]}),
        json!({"id":"rs_1","type":"reasoning","status":huge,"content":[],"summary":[]}),
        json!({"id":"rs_1","type":"reasoning","content":[{"type":huge,"text":""}],"summary":[]}),
        json!({"id":"mcp_1","type":"mcp_call","server_label":"s","name":"tool","arguments":"{}",
            "error":{"type":huge,"content":[]}}),
        json!({"id":"mcp_1","type":"mcp_call","server_label":"s","name":"tool","arguments":"{}",
            "error":{"type":"mcp_tool_execution_error","content":[{"type":huge,"text":""}]}}),
    ] {
        reject_item_on_both_paths(&item);
    }
}

#[test]
#[ignore = "manual scaling measurement: run with --ignored --nocapture"]
fn reasoning_completion_accounting_scaling() {
    for count in [4_000, 8_000, 16_000] {
        let (mut acc, _) = budgeted(16 << 20, Validation::Lenient);
        feed(
            &mut acc,
            &[
                created(),
                json!({"type":"response.output_item.added","output_index":0,
            "item":{"id":"rs_1","type":"reasoning","content":[],"summary":[]}}),
            ],
        )
        .unwrap();
        let started = std::time::Instant::now();
        for index in 0..count {
            acc.process_line(line(&json!({"type":"response.reasoning_text.done","output_index":0,
                "item_id":"rs_1","content_index":index,"text":"x"})))
                .unwrap();
        }
        eprintln!("reasoning completion: {count} parts in {:?}", started.elapsed());
        let output = finished_output(acc);
        let OutputItem::Reasoning(item) = &output[0] else {
            panic!("reasoning item")
        };
        assert_eq!(item.content.len(), usize::try_from(count).unwrap());
    }
}

#[test]
fn empty_web_search_queries_exhaust_budget_on_both_paths() {
    reject_item_on_both_paths(&json!({"id":"ws_1", "type":"web_search_call", "status":"completed",
        "action":{"type":"search", "query":"", "queries":vec![""; 100_000]}}));
}

#[test]
fn reasoning_completion_accounts_for_sparse_repeated_and_empty_parts() {
    for (delta_kind, done_kind, index_field) in [
        (
            "response.reasoning_text.delta",
            "response.reasoning_text.done",
            "content_index",
        ),
        (
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
            "summary_index",
        ),
    ] {
        let (mut acc, budget) = budgeted(1 << 20, Validation::Lenient);
        feed(
            &mut acc,
            &[
                created(),
                json!({"type":"response.output_item.added","output_index":0,
            "item":{"id":"rs_1","type":"reasoning","content":[],"summary":[]}}),
            ],
        )
        .unwrap();
        for index in [0, 2, 1, 1, u32::MAX] {
            let delta = json!({"type":delta_kind,"output_index":0,"item_id":"rs_1",index_field:index,"delta":"x"});
            let done = json!({"type":done_kind,"output_index":0,"item_id":"rs_1",index_field:index,"text":"xyz"});
            feed(&mut acc, &[delta, done]).unwrap();
            let slot = acc.slots.get(OutputIndex::new(0)).unwrap();
            let SlotState::Active(state) = &slot.state else {
                panic!("active reasoning")
            };
            assert_eq!(
                budget.used(),
                RETAINED_CONTAINER_OVERHEAD_BYTES + "resp_1".len() + state.retained_bytes(),
                "completion measures only its actual inserted part, including sparse and repeated indexes"
            );
        }
        let before_empty = budget.used();
        let delta = json!({"type":delta_kind,"output_index":0,"item_id":"rs_1",index_field:1,"delta":""});
        let done = json!({"type":done_kind,"output_index":0,"item_id":"rs_1",index_field:1,"text":""});
        feed(&mut acc, &[delta, done.clone(), done]).unwrap();
        assert_eq!(
            budget.used(),
            before_empty + RETAINED_CONTAINER_OVERHEAD_BYTES,
            "empty completion does not add a part or refund its already charged counter"
        );
        let output = finished_output(acc);
        let OutputItem::Reasoning(item) = &output[0] else {
            panic!("reasoning item")
        };
        assert_eq!(item.content.len() + item.summary.len(), 5);
    }
}

#[test]
fn pending_web_search_identity_exhausts_budget_before_insertion() {
    let (mut acc, _) = budgeted(4096, Validation::Lenient);
    feed(&mut acc, &[created()]).unwrap();
    let error = acc
        .process_line(line(&json!({"type":"response.output_item.added","output_index":0,
        "item":{"id":"w".repeat(100_000),"type":"web_search_call","status":"in_progress"}})))
        .expect_err("pending identity must be charged even before the web search action arrives");
    assert_budget_exceeded(&error);
    assert_eq!(acc.slots.len(), 0, "the oversized identity was never retained");
}

#[test]
fn late_bound_identity_exhausts_budget_before_binding() {
    let (mut acc, _) = budgeted(4096, Validation::Lenient);
    feed(&mut acc, &[created(), message_added("")]).unwrap();
    let error = acc
        .process_line(line(&text_delta(&"m".repeat(100_000), 0, "")))
        .expect_err("a late-bound identity must be charged");
    assert_budget_exceeded(&error);
    assert!(acc.slots.get(OutputIndex::new(0)).unwrap().item_id.is_none());
}

#[test]
fn terminal_details_exhaust_retained_budget_on_json_and_sse_paths() {
    for details in [
        json!({"error":{"message":"e".repeat(100_000)}}),
        json!({"error":{"extra":vec!["";100_000]}}),
        json!({"incomplete_details":{"reason":"r".repeat(100_000)}}),
    ] {
        let mut response = json!({"id":"resp_1","status":"incomplete","output":[]});
        response
            .as_object_mut()
            .unwrap()
            .extend(details.as_object().unwrap().clone());
        let (mut json_path, _) = budgeted(4096, Validation::Lenient);
        let json_result = json_path.load_json_body(&response.to_string());
        let (mut acc, _) = budgeted(4096, Validation::Lenient);
        feed(&mut acc, &[created()]).unwrap();
        let stream_result = acc.process_line(line(&json!({"type":"response.incomplete","response":response})));
        assert!(
            json_result.is_err() && stream_result.is_err(),
            "both paths must reject terminal details: JSON={json_result:?}, SSE={stream_result:?}"
        );
        assert_budget_exceeded(&json_result.unwrap_err());
        assert_budget_exceeded(&stream_result.unwrap_err());
    }
}

fn shell_command(kind: &str, index: u32, command: &str) -> Value {
    json!({"type":kind,"output_index":0,"item_id":"sh_1","command_index":index,"command":command})
}

fn shell_added() -> Value {
    json!({"type":"response.output_item.added","output_index":0,
        "item":{"id":"sh_1","type":"shell_call","call_id":"call_1","action":{"commands":[],"extra_schema":{"nested":vec!["extra";1000]}}}})
}

#[test]
fn shell_completion_accounts_once_and_keeps_lifecycle_validation() {
    let (mut acc, budget) = budgeted(1 << 20, Validation::Lenient);
    feed(&mut acc, &[created(), shell_added()]).unwrap();
    for (index, command) in [(0, ""), (1, "echo hi"), (2, "")] {
        let added = shell_command("response.shell_call_command.added", index, command);
        let done = shell_command("response.shell_call_command.done", index, command);
        feed(&mut acc, &[added]).unwrap();
        let before = budget.used();
        feed(&mut acc, std::slice::from_ref(&done)).unwrap();
        assert_eq!(budget.used(), before, "completion moves already charged command text");
        assert!(
            acc.process_line(line(&done)).is_err(),
            "duplicate completion stays invalid"
        );
        assert!(
            acc.process_line(line(&shell_command("response.shell_call_command.done", 99, "")))
                .is_err(),
            "out-of-range completion stays invalid"
        );
    }
    let output = finished_output(acc);
    assert_eq!(
        budget.used(),
        RETAINED_CONTAINER_OVERHEAD_BYTES + "resp_1".len() + output[0].retained_bytes()
    );
}

#[test]
#[ignore = "manual scaling measurement: run with --ignored --nocapture"]
fn shell_completion_accounting_scaling() {
    for count in [4_000, 8_000, 16_000] {
        let (mut acc, _) = budgeted(16 << 20, Validation::Lenient);
        feed(&mut acc, &[created(), shell_added()]).unwrap();
        let started = std::time::Instant::now();
        for index in 0..count {
            feed(
                &mut acc,
                &[
                    shell_command("response.shell_call_command.added", index, "x"),
                    shell_command("response.shell_call_command.done", index, "x"),
                ],
            )
            .unwrap();
        }
        eprintln!("shell completion: {count} commands in {:?}", started.elapsed());
        let output = finished_output(acc);
        let OutputItem::ShellCall(item) = &output[0] else {
            panic!("shell item")
        };
        assert_eq!(item.action.commands.len(), usize::try_from(count).unwrap());
    }
}

#[test]
fn terminal_details_snapshots_charge_once_and_match_json() {
    let response = json!({"id":"resp_1","status":"incomplete","output":[],
        "error":{"message":"failed","nested":["",[]]},"incomplete_details":{"reason":"upstream"}});
    let (mut json_path, json_budget) = budgeted(4096, Validation::Lenient);
    json_path.load_json_body(&response.to_string()).unwrap();
    let (mut streamed, streamed_budget) = budgeted(4096, Validation::Lenient);
    let terminal = json!({"type":"response.incomplete","response":response});
    feed(&mut streamed, &[created(), terminal.clone(), terminal]).unwrap();
    assert_eq!(streamed_budget.used(), json_budget.used());
}

#[test]
fn pending_identity_is_not_charged_again_at_completion() {
    let item = json!({"id":"ws_1","type":"web_search_call","status":"completed",
        "action":{"type":"search","query":"q"}});
    let (mut acc, budget) = budgeted(4096, Validation::Lenient);
    feed(
        &mut acc,
        &[
            created(),
            json!({"type":"response.output_item.added","output_index":0,
        "item":{"id":"ws_1","type":"web_search_call","status":"in_progress"}}),
            output_item_done(&item),
        ],
    )
    .unwrap();
    let output = finished_output(acc);
    assert_eq!(
        budget.used(),
        RETAINED_CONTAINER_OVERHEAD_BYTES + "resp_1".len() + output[0].retained_bytes()
    );
}

#[test]
fn collaboration_snapshots_and_encrypted_parts_respect_the_response_budget() {
    let huge = "x".repeat(100_000);
    for item in [
        json!({"type":"multi_agent_call","id":"mac_1","call_id":"call_1","action":"spawn_agent","arguments":huge}),
        json!({"type":"multi_agent_call_output","id":"maco_1","call_id":"call_1","action":"spawn_agent",
            "output":[{"type":"output_text","text":huge}]}),
        json!({"type":"agent_message","id":"amsg_1","author":"/root","recipient":"/root/review",
            "content":[{"type":"encrypted_content","encrypted_content":huge}]}),
    ] {
        // A large opening snapshot is charged before its slot is inserted.
        let (mut acc, _) = budgeted(4096, Validation::Lenient);
        let error = acc
            .process_line(line(
                &json!({"type":"response.output_item.added","output_index":0,"item":item}),
            ))
            .unwrap_err();
        assert_budget_exceeded(&error);
        assert_eq!(acc.slots.len(), 0);

        // A small opening snapshot cannot conceal oversized completion content.
        let mut opening = item.clone();
        match item["type"].as_str().unwrap() {
            "multi_agent_call" => opening["arguments"] = json!(""),
            "multi_agent_call_output" => opening["output"] = json!([]),
            "agent_message" => opening["content"] = json!([]),
            _ => unreachable!(),
        }
        let (mut acc, _) = budgeted(4096, Validation::Lenient);
        acc.process_line(line(
            &json!({"type":"response.output_item.added","output_index":0,"item":opening}),
        ))
        .unwrap();
        let error = acc.process_line(line(&output_item_done(&item))).unwrap_err();
        assert_budget_exceeded(&error);
        assert!(acc.slots.has_active());

        let (mut json_acc, _) = budgeted(4096, Validation::Strict);
        let error = json_acc
            .load_json_body(&json!({"id":"resp_1","status":"completed","output":[item]}).to_string())
            .unwrap_err();
        assert_budget_exceeded(&error);
    }
    let (mut acc, budget) = budgeted(4096, Validation::Lenient);
    let opening =
        json!({"type":"agent_message","id":"amsg_1","author":"/root","recipient":"/root/review","content":[]});
    acc.process_line(line(
        &json!({"type":"response.output_item.added","output_index":0,"item":opening}),
    ))
    .unwrap();
    let used = budget.used();
    let error = acc
        .process_line(line(
            &json!({"type":"response.content_part.done","output_index":0,"item_id":"amsg_1","content_index":0,
        "part":{"type":"encrypted_content","encrypted_content":huge}}),
        ))
        .unwrap_err();
    assert_budget_exceeded(&error);
    assert_eq!(budget.used(), used);
    // Rejection did not retain the oversized part or poison completion.
    acc.process_line(line(&output_item_done(&opening))).unwrap();
    assert!(!acc.slots.has_active());
}
