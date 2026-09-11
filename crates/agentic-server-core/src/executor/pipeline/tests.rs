use std::collections::{HashMap, HashSet};

use super::*;
use crate::events::SseLine;
use crate::tool::ToolType;
use crate::types::io::OutputItem;
use serde_json::{Value, json};

fn pipeline(validation: Validation, tools: &[(&str, ToolType)]) -> RoundIngestion {
    RoundIngestion::new(
        "resp_1".to_owned(),
        Some("conv_1".to_owned()),
        validation,
        TranslationContext::new(
            tools
                .iter()
                .map(|(name, kind)| ((*name).to_owned(), *kind))
                .collect::<HashMap<_, _>>(),
            HashSet::new(),
            tools.contains(&("tool_search", ToolType::ToolSearch)),
        ),
    )
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
    assert_eq!(pipeline.finish("model", None, None).unwrap().output.len(), 1);
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
        let result = pipeline.finish("model", Some("resp_previous"), Some("instructions"));
        match validation {
            Validation::Strict => assert!(result.unwrap_err().to_string().contains("without a terminal event")),
            Validation::Lenient => {
                let payload = result.unwrap();
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
            let payload = pipeline.finish("model", None, None).unwrap();
            assert_eq!(payload.status, if status == "failed" { "error" } else { status });
            assert_eq!(payload.id, "resp_1");
            assert_eq!(payload.conversation_id.as_deref(), Some("conv_1"));
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
            let result = pipeline.finish("model", None, None);
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
