use std::collections::{HashMap, HashSet};

use super::*;
use crate::events::SseLine;
use crate::executor::pipeline::test_support::ingest_lenient;
use crate::executor::request::RequestContext;
use crate::executor::upstream::tests::request_context;
use crate::tool::ToolType;
use crate::tool::handler::ToolError;
use crate::types::io::OutputItem;
use serde_json::{Value, json};

fn pipeline(validation: Validation, tools: &[(&str, ToolType)]) -> RoundIngestion {
    RoundIngestion::new(
        validation,
        TranslationContext::new(
            tools
                .iter()
                .map(|(name, kind)| ((*name).to_owned(), *kind))
                .collect::<HashMap<_, _>>(),
            HashSet::new(),
            tools.contains(&("tool_search", ToolType::ToolSearch)),
        ),
        None,
    )
}

/// The request whose identity `inject_ids` stamps on these rounds' payloads.
fn request_identity() -> RequestContext {
    let mut request = request_context();
    "resp_1".clone_into(&mut request.response_id);
    request.conversation_id = Some("conv_1".to_owned());
    request.original_request.previous_response_id = Some("resp_previous".to_owned());
    request
}

fn push(pipeline: &mut RoundIngestion, event: &Value) -> Translation {
    pipeline.push(SseLine::parse(&format!("data: {event}"))).unwrap()
}

fn start(pipeline: &mut RoundIngestion) {
    for event in ["response.created", "response.in_progress"] {
        push(
            pipeline,
            &json!({"type":event,"response":{"id":"resp_1","status":"in_progress"}}),
        );
    }
}

#[test]
fn ignored_lines_keep_pending_and_gateway_deferral_boundaries() {
    let mut pipeline = pipeline(Validation::Lenient, &[("lookup", ToolType::Mcp)]);
    start(&mut pipeline);
    push(
        &mut pipeline,
        &json!({"type":"response.output_item.added","output_index":2,
        "item":{"id":"fc_2","type":"function_call","call_id":"call_2","arguments":"","status":"in_progress"}}),
    );
    for resolve in [false, true] {
        if resolve {
            let done = push(
                &mut pipeline,
                &json!({"type":"response.output_item.done","output_index":2,
                "item":{"id":"fc_2","type":"function_call","call_id":"call_2","name":"lookup","arguments":"{}","status":"completed"}}),
            );
            assert!(done.frames.is_empty());
        }
        for line in [
            ": heartbeat",
            "data: [DONE]",
            "data: {",
            "data:   ",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":9,\"item_id\":\"orphan\",\"delta\":\"ignored\"}",
        ] {
            let result = pipeline.push(SseLine::parse(line)).unwrap();
            assert!(result.frames.is_empty());
            assert_eq!(result.defer_from_output_index, Some(2));
        }
    }
    assert_eq!(pipeline.finish("model", None).unwrap().output.len(), 1);
}

#[test]
fn finish_applies_strict_and_lenient_eof_policy_and_folds_unfinished_items() {
    for validation in [Validation::Strict, Validation::Lenient] {
        let mut pipeline = pipeline(validation, &[]);
        start(&mut pipeline);
        push(
            &mut pipeline,
            &json!({"type":"response.output_item.added","output_index":0,
            "item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"echo","arguments":"","status":"in_progress"}}),
        );
        push(
            &mut pipeline,
            &json!({"type":"response.function_call_arguments.delta","output_index":0,
            "item_id":"fc_1","delta":"{\"value\":1}"}),
        );
        pipeline.push(ClassifiedSseLine::Done).unwrap();
        let result = pipeline.finish("model", Some("instructions"));
        match validation {
            Validation::Strict => assert!(result.unwrap_err().to_string().contains("without a terminal event")),
            Validation::Lenient => {
                let mut payload = result.unwrap();
                request_identity().inject_ids(&mut payload);
                assert_eq!(payload.status, "completed");
                assert_eq!(payload.model, "model");
                assert_eq!(payload.previous_response_id.as_deref(), Some("resp_previous"));
                assert_eq!(payload.instructions.as_deref(), Some("instructions"));
                let [OutputItem::FunctionCall(call)] = payload.output.as_slice() else {
                    panic!("folded function call");
                };
                assert_eq!(call.arguments, "{\"value\":1}");
            }
        }
    }
}

#[test]
fn finish_preserves_terminal_metadata_under_both_policies() {
    for validation in [Validation::Strict, Validation::Lenient] {
        for (event, status) in [
            ("response.completed", "completed"),
            ("response.failed", "failed"),
            ("response.incomplete", "incomplete"),
        ] {
            let mut pipeline = pipeline(validation, &[]);
            start(&mut pipeline);
            let error = json!({"code":"upstream_error","message":"diagnostic"});
            push(
                &mut pipeline,
                &json!({"type":event,"response":{
                    "id":"resp_1","status":status,"output":[],"error":error,
                    "incomplete_details":{"reason":"max_output_tokens"},
                    "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}
                }}),
            );
            let mut payload = pipeline.finish("model", None).unwrap();
            request_identity().inject_ids(&mut payload);
            assert_eq!(payload.status, if status == "failed" { "error" } else { status });
            assert_eq!(payload.id, "resp_1");
            assert_eq!(payload.conversation.as_deref(), Some("conv_1"));
            assert_eq!(payload.error, Some(error));
            assert_eq!(
                payload.incomplete_details.unwrap().reason.as_deref(),
                Some("max_output_tokens")
            );
            assert_eq!(payload.usage.unwrap().total_tokens, 5);
        }
    }
}

#[test]
fn native_code_interpreter_stream_passes_through_and_assembles() {
    let mut pipeline = pipeline(Validation::Strict, &[]);
    start(&mut pipeline);

    let added_item = json!({
        "type": "code_interpreter_call",
        "id": "ci_1",
        "container_id": "cntr_1",
        "code": "",
        "status": "in_progress",
        "outputs": null
    });
    let completed_item = json!({
        "type": "code_interpreter_call",
        "id": "ci_1",
        "container_id": "cntr_1",
        "code": "print(42)",
        "status": "completed",
        "outputs": [{"type": "logs", "logs": "42\n"}]
    });
    let events = [
        json!({"type": "response.output_item.added", "output_index": 0, "item": added_item}),
        json!({"type": "response.code_interpreter_call.in_progress", "output_index": 0, "item_id": "ci_1"}),
        json!({"type": "response.code_interpreter_call_code.delta", "output_index": 0, "item_id": "ci_1", "delta": "print("}),
        json!({"type": "response.code_interpreter_call_code.delta", "output_index": 0, "item_id": "ci_1", "delta": "42)"}),
        json!({"type": "response.code_interpreter_call_code.done", "output_index": 0, "item_id": "ci_1", "code": "print(42)"}),
        json!({"type": "response.code_interpreter_call.interpreting", "output_index": 0, "item_id": "ci_1"}),
        json!({"type": "response.code_interpreter_call.completed", "output_index": 0, "item_id": "ci_1"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": completed_item.clone()}),
    ];

    for event in events {
        let event_type = event["type"].as_str().expect("event type");
        let translated = push(&mut pipeline, &event);
        assert_eq!(translated.frames.len(), 1, "{event_type} must pass through");
        assert_eq!(translated.frames[0].wire.event_type.as_deref(), Some(event_type));
    }

    push(
        &mut pipeline,
        &json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "status": "completed", "output": [completed_item.clone()]}
        }),
    );
    let payload = pipeline.finish("model", None).expect("valid native stream");
    assert_eq!(
        serde_json::to_value(payload.output).expect("serialize output"),
        json!([completed_item])
    );
}

#[test]
fn unfinished_native_and_synthetic_search_require_an_aborted_response() {
    for native in [false, true] {
        for terminal in [None, Some("response.incomplete"), Some("response.failed")] {
            let mut pipeline = pipeline(Validation::Lenient, &[("tool_search", ToolType::ToolSearch)]);
            start(&mut pipeline);
            let item = if native {
                json!({"id":"tsc_1","type":"tool_search_call","call_id":"call_1","execution":"client","arguments":{},"status":"in_progress"})
            } else {
                json!({"id":"fc_1","type":"function_call","call_id":"call_1","name":"tool_search","arguments":"","status":"in_progress"})
            };
            let added = push(
                &mut pipeline,
                &json!({"type":"response.output_item.added","output_index":0,"item":item}),
            );
            if let Some(terminal) = terminal {
                let status = if terminal == "response.failed" {
                    "failed"
                } else {
                    "incomplete"
                };
                push(
                    &mut pipeline,
                    &json!({"type":terminal,"response":{"id":"resp_1","status":status,"output":[]}}),
                );
            }
            let result = pipeline.finish("model", None);
            match terminal {
                None => assert!(result.unwrap_err().to_string().contains("invalid tool-search call")),
                Some("response.failed") => assert!(result.unwrap().output.is_empty()),
                Some(_) => {
                    let payload = result.unwrap();
                    let [OutputItem::ToolSearchCall(call)] = payload.output.as_slice() else {
                        panic!("public incomplete search");
                    };
                    assert_eq!(call.id, added.frames[0].wire.rest["item"]["id"]);
                    assert_eq!(call.status, crate::types::tools::ToolSearchStatus::Incomplete);
                    assert_eq!(call.arguments, json!({}));
                }
            }
        }
    }
}

/// Synthetic single-turn stream: the text "GLOBE" split across three deltas.
// Same fixture as `tests/event_normalizer/cassette_flows.rs`, whose other tests still read it there.
const SIMULATED_SSE: &[&str] = &[
    r#"data: {"type":"response.created","response":{"id":"resp_abc","status":"in_progress","usage":null},"sequence_number":0}"#,
    r#"data: {"type":"response.in_progress","response":{"id":"resp_abc","status":"in_progress","usage":null},"sequence_number":1}"#,
    r#"data: {"type":"response.output_item.added","item":{"id":"msg_1","type":"message","status":"in_progress","content":[]},"output_index":0,"sequence_number":2}"#,
    r#"data: {"type":"response.content_part.added","content_index":0,"item_id":"msg_1","output_index":0,"part":{"type":"output_text","text":""},"sequence_number":3}"#,
    r#"data: {"type":"response.output_text.delta","content_index":0,"delta":"G","item_id":"msg_1","output_index":0,"sequence_number":4}"#,
    r#"data: {"type":"response.output_text.delta","content_index":0,"delta":"LO","item_id":"msg_1","output_index":0,"sequence_number":5}"#,
    r#"data: {"type":"response.output_text.delta","content_index":0,"delta":"BE","item_id":"msg_1","output_index":0,"sequence_number":6}"#,
    r#"data: {"type":"response.output_text.done","content_index":0,"item_id":"msg_1","output_index":0,"text":"GLOBE","sequence_number":7}"#,
    r#"data: {"type":"response.content_part.done","content_index":0,"item_id":"msg_1","output_index":0,"part":{"type":"output_text","text":"GLOBE"},"sequence_number":8}"#,
    r#"data: {"type":"response.output_item.done","item":{"id":"msg_1","type":"message","status":"completed","content":[{"type":"output_text","text":"GLOBE"}],"role":"assistant"},"output_index":0,"sequence_number":9}"#,
    r#"data: {"type":"response.completed","response":{"id":"resp_abc","status":"completed","usage":{"input_tokens":14,"output_tokens":4,"total_tokens":18}},"sequence_number":10}"#,
];

#[test]
fn sentinel_sequence_stream_preserves_terminal_usage_and_text() {
    // Synthetic protocol regression data, not a captured provider recording.
    let lines = SIMULATED_SSE.iter().map(|line| {
        let mut event: serde_json::Value = serde_json::from_str(line.strip_prefix("data: ").unwrap()).unwrap();
        event["sequence_number"] = (-1).into();
        format!("data: {event}")
    });
    let payload = ingest_lenient(lines, "test-model").expect("sentinel frames remain valid");
    assert_eq!(payload.status, "completed");
    assert_eq!(payload.output.len(), 1);
    assert!(matches!(&payload.output[0], OutputItem::Message(message) if message.id == "msg_1"));
    let serialized = serde_json::to_value(&payload).unwrap();
    assert_eq!(serialized["output"][0]["content"][0]["text"], "GLOBE");
    assert_eq!(serialized["usage"]["total_tokens"], 18);
}

/// A hosted search is the gateway's own call, so neither transport holds it to the client search's
/// rules; a client search is held to them on both.
#[test]
fn json_and_stream_classify_a_search_call_the_same_way() {
    let search = |call_id: &str, status: &str| {
        json!({
            "type": "function_call",
            "id": format!("fc_{call_id}"),
            "call_id": call_id,
            "name": "tool_search",
            "arguments": "{}",
            "status": status
        })
    };
    let outputs = [
        ("unfinished call", vec![search("call_1", "in_progress")]),
        (
            "repeated call_id",
            vec![search("call_1", "completed"), search("call_1", "completed")],
        ),
    ];
    for (case, output) in outputs {
        let response = json!({"id": "resp_1", "object": "response", "status": "completed", "output": output});
        for hosted in [true, false] {
            let context = || {
                let context = TranslationContext::new(
                    HashMap::from([("tool_search".to_owned(), ToolType::ToolSearch)]),
                    HashSet::new(),
                    true,
                );
                if hosted {
                    context.with_gateway_owned_names(HashSet::from(["tool_search".to_owned()]))
                } else {
                    context
                }
            };

            let json_body =
                RoundIngestion::new(Validation::Lenient, context(), None).load_json_body(&response.to_string());
            let mut stream = RoundIngestion::new(Validation::Lenient, context(), None);
            let streamed = stream
                .push(SseLine::parse(&format!(
                    "data: {}",
                    json!({"type": "response.completed", "response": response})
                )))
                .map(|_| ());

            for (transport, result) in [("JSON body", json_body), ("stream", streamed)] {
                if hosted {
                    assert!(result.is_ok(), "{case}, {transport}, hosted search: {result:?}");
                } else {
                    assert!(
                        matches!(result, Err(ExecutorError::Tool(ToolError::InvalidUpstreamToolSearch))),
                        "{case}, {transport}, client search: {result:?}"
                    );
                }
            }
        }
    }
}
