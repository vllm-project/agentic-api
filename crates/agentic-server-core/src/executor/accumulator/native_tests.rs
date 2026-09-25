use super::ResponseAccumulator;
use crate::types::io::OutputItem;
use vllm_responses::{InferenceEvent, InferenceFailure, InferenceUsage, OutputIndex};

fn response(events: impl IntoIterator<Item = InferenceEvent>) -> ResponseAccumulator {
    ResponseAccumulator::from_inference_events("resp_1".to_owned(), events, Some("conv_1"))
        .expect("valid native inference event stream")
}

#[test]
fn native_events_fold_text_reasoning_and_usage() {
    let accumulator = response([
        InferenceEvent::Started,
        InferenceEvent::InProgress,
        InferenceEvent::TextStarted {
            item_id: "msg_1".to_owned(),
            output_index: OutputIndex(0),
        },
        InferenceEvent::TextDelta {
            item_id: "msg_1".to_owned(),
            output_index: OutputIndex(0),
            delta: "hello".to_owned(),
        },
        InferenceEvent::TextCompleted {
            item_id: "msg_1".to_owned(),
            output_index: OutputIndex(0),
            text: "hello".to_owned(),
        },
        InferenceEvent::ReasoningStarted {
            item_id: "rs_1".to_owned(),
            output_index: OutputIndex(1),
        },
        InferenceEvent::ReasoningTextDelta {
            item_id: "rs_1".to_owned(),
            output_index: OutputIndex(1),
            delta: "think".to_owned(),
        },
        InferenceEvent::ReasoningCompleted {
            item_id: "rs_1".to_owned(),
            output_index: OutputIndex(1),
            text: "think".to_owned(),
        },
        InferenceEvent::Completed {
            usage: Some(InferenceUsage::new(3, 2, 1).with_cached_tokens(1)),
        },
    ]);

    let payload = accumulator.finalize("model", None, None);
    assert_eq!(payload.id, "resp_1");
    assert_eq!(payload.usage.expect("usage").total_tokens, 5);
    assert_eq!(payload.usage.expect("usage").input_tokens_details.cached_tokens, 1);
    let [OutputItem::Message(message), OutputItem::Reasoning(reasoning)] = payload.output.as_slice() else {
        panic!("text followed by reasoning output");
    };
    assert_eq!(message.content[0].text, "hello");
    assert_eq!(
        serde_json::to_value(reasoning).expect("reasoning serializes")["content"][0]["text"],
        "think"
    );
}

#[test]
fn native_terminal_outcomes_require_completed_output_items() {
    let incomplete = ResponseAccumulator::from_inference_events(
        "resp_1".to_owned(),
        [
            InferenceEvent::Started,
            InferenceEvent::InProgress,
            InferenceEvent::TextStarted {
                item_id: "msg_1".to_owned(),
                output_index: OutputIndex(0),
            },
            InferenceEvent::TextDelta {
                item_id: "msg_1".to_owned(),
                output_index: OutputIndex(0),
                delta: "partial".to_owned(),
            },
            InferenceEvent::Incomplete {
                usage: Some(InferenceUsage::new(4, 1, 0)),
                reason: Some("max_output_tokens".to_owned()),
            },
        ],
        None,
    )
    .expect_err("incomplete responses cannot silently discard partial output");
    assert!(incomplete.to_string().contains("unfinished output items"));

    let failed = ResponseAccumulator::from_inference_events(
        "resp_1".to_owned(),
        [
            InferenceEvent::Started,
            InferenceEvent::InProgress,
            InferenceEvent::FunctionCallStarted {
                item_id: "fc_1".to_owned(),
                output_index: OutputIndex(0),
                call_id: "call_1".to_owned(),
                name: "lookup".to_owned(),
            },
            InferenceEvent::Failed {
                usage: None,
                failure: InferenceFailure::new("engine stopped").with_code("engine_aborted"),
            },
        ],
        None,
    )
    .expect_err("failed responses cannot silently discard partial output");
    assert!(failed.to_string().contains("unfinished output items"));
}

#[test]
fn native_completed_items_survive_incomplete_and_failed_responses() {
    let incomplete = response([
        InferenceEvent::Started,
        InferenceEvent::InProgress,
        InferenceEvent::TextStarted {
            item_id: "msg_1".to_owned(),
            output_index: OutputIndex(0),
        },
        InferenceEvent::TextDelta {
            item_id: "msg_1".to_owned(),
            output_index: OutputIndex(0),
            delta: "partial".to_owned(),
        },
        InferenceEvent::TextCompleted {
            item_id: "msg_1".to_owned(),
            output_index: OutputIndex(0),
            text: "partial".to_owned(),
        },
        InferenceEvent::Incomplete {
            usage: Some(InferenceUsage::new(4, 1, 0)),
            reason: Some("max_output_tokens".to_owned()),
        },
    ])
    .finalize("model", None, None);
    assert_eq!(incomplete.status, "incomplete");
    assert_eq!(incomplete.output.len(), 1);
    assert_eq!(
        incomplete.incomplete_details.expect("reason").reason.as_deref(),
        Some("max_output_tokens")
    );

    let failed = response([
        InferenceEvent::Started,
        InferenceEvent::InProgress,
        InferenceEvent::FunctionCallStarted {
            item_id: "fc_1".to_owned(),
            output_index: OutputIndex(0),
            call_id: "call_1".to_owned(),
            name: "lookup".to_owned(),
        },
        InferenceEvent::FunctionCallCompleted {
            item_id: "fc_1".to_owned(),
            output_index: OutputIndex(0),
            call_id: "call_1".to_owned(),
            name: "lookup".to_owned(),
            arguments: "{}".to_owned(),
        },
        InferenceEvent::Failed {
            usage: None,
            failure: InferenceFailure::new("engine stopped"),
        },
    ])
    .finalize("model", None, None);
    assert_eq!(failed.status, "error");
    assert_eq!(failed.output.len(), 1);
    assert_eq!(failed.error.expect("failure")["code"], "server_error");
}

#[test]
fn native_events_match_equivalent_sse_response_assembly() {
    let native = response([
        InferenceEvent::Started,
        InferenceEvent::InProgress,
        InferenceEvent::TextStarted {
            item_id: "msg_1".to_owned(),
            output_index: OutputIndex(0),
        },
        InferenceEvent::TextDelta {
            item_id: "msg_1".to_owned(),
            output_index: OutputIndex(0),
            delta: "hello".to_owned(),
        },
        InferenceEvent::TextCompleted {
            item_id: "msg_1".to_owned(),
            output_index: OutputIndex(0),
            text: "hello".to_owned(),
        },
        InferenceEvent::Completed {
            usage: Some(InferenceUsage::new(3, 2, 0).with_cached_tokens(1)),
        },
    ])
    .finalize("model", None, None);
    let sse = ResponseAccumulator::from_sse_lines(
        [
            r#"data: {"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}"#,
            r#"data: {"type":"response.in_progress","response":{"id":"resp_1","status":"in_progress"}}"#,
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[],"status":"in_progress"}}"#,
            r#"data: {"type":"response.output_text.delta","output_index":0,"item_id":"msg_1","content_index":0,"delta":"hello"}"#,
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}],"status":"completed"}}"#,
            r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5,"input_tokens_details":{"cached_tokens":1},"output_tokens_details":{"reasoning_tokens":0}}}}"#,
        ]
        .into_iter()
        .map(str::to_owned),
        Some("conv_1"),
    )
    .expect("equivalent SSE response")
    .finalize("model", None, None);

    assert_eq!(
        serde_json::to_value(native).expect("native response serializes"),
        serde_json::to_value(sse).expect("SSE response serializes")
    );
}

#[test]
fn native_events_reject_identity_mismatches_and_conflicting_completion() {
    let mismatched = ResponseAccumulator::from_inference_events(
        "resp_1".to_owned(),
        [
            InferenceEvent::Started,
            InferenceEvent::InProgress,
            InferenceEvent::TextStarted {
                item_id: "msg_1".to_owned(),
                output_index: OutputIndex(0),
            },
            InferenceEvent::TextDelta {
                item_id: "msg_2".to_owned(),
                output_index: OutputIndex(0),
                delta: "wrong item".to_owned(),
            },
        ],
        None,
    )
    .expect_err("mismatched item ID is rejected");
    assert!(mismatched.to_string().contains("inconsistent item ID"));

    let duplicate = ResponseAccumulator::from_inference_events(
        "resp_1".to_owned(),
        [
            InferenceEvent::Started,
            InferenceEvent::InProgress,
            InferenceEvent::TextStarted {
                item_id: "msg_1".to_owned(),
                output_index: OutputIndex(0),
            },
            InferenceEvent::TextCompleted {
                item_id: "msg_1".to_owned(),
                output_index: OutputIndex(0),
                text: "first".to_owned(),
            },
            InferenceEvent::TextCompleted {
                item_id: "msg_1".to_owned(),
                output_index: OutputIndex(0),
                text: "second".to_owned(),
            },
        ],
        None,
    )
    .expect_err("conflicting completion is rejected");
    assert!(!duplicate.to_string().is_empty());
}

#[test]
fn native_events_reject_duplicate_starts_and_events_after_terminal() {
    let duplicate = ResponseAccumulator::from_inference_events(
        "resp_1".to_owned(),
        [
            InferenceEvent::Started,
            InferenceEvent::InProgress,
            InferenceEvent::TextStarted {
                item_id: "msg_1".to_owned(),
                output_index: OutputIndex(0),
            },
            InferenceEvent::TextStarted {
                item_id: "msg_2".to_owned(),
                output_index: OutputIndex(0),
            },
        ],
        None,
    )
    .expect_err("reused output index is rejected");
    assert!(duplicate.to_string().contains("repeats output item"));

    let after_terminal = ResponseAccumulator::from_inference_events(
        "resp_1".to_owned(),
        [
            InferenceEvent::Started,
            InferenceEvent::InProgress,
            InferenceEvent::Completed { usage: None },
            InferenceEvent::TextStarted {
                item_id: "msg_1".to_owned(),
                output_index: OutputIndex(0),
            },
        ],
        None,
    )
    .expect_err("events after the terminal response are rejected");
    assert!(after_terminal.to_string().contains("after its terminal"));
}
