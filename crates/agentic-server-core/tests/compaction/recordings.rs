//! Public HTTP captures complement the model-facing replay tests in the parent module.
use std::borrow::Cow;

use agentic_core::types::io::OutputItem;
use serde_json::Value;

use super::cassette;
use crate::support::{Turn, recorded_named_sse_events};

fn body(turn: &Turn) -> Cow<'_, Value> {
    if let Some(body) = &turn.response.body {
        return Cow::Borrowed(body);
    }
    let mut events = recorded_named_sse_events(turn);
    let terminal = events
        .iter_mut()
        .find(|event| event["type"] == "response.completed")
        .expect("successful streamed response");
    Cow::Owned(terminal["response"].take())
}

fn kinds(items: &Value) -> Vec<&str> {
    items
        .as_array()
        .expect("item array")
        .iter()
        .map(|item| item["type"].as_str().expect("item type"))
        .filter(|kind| *kind != "reasoning")
        .collect()
}

#[test]
fn public_compaction_recordings_have_complete_turns_and_stream_lifecycles() {
    for provider in ["openai", "gateway"] {
        for (scenario, count) in [
            ("stale", 10),
            ("in-turn-chain", 5),
            ("in-turn-chain-stream", 5),
            ("tool-loop", 10),
            ("standalone", 5),
            ("multi-agent", 5),
            ("store-false", 2),
            ("mcp", 11),
        ] {
            let recording = cassette(&format!("compaction-{scenario}-{provider}.yaml"));
            assert_eq!(recording.turns.len(), count, "{scenario}/{provider}");
            for (index, turn) in recording.turns.iter().enumerate() {
                let status = if scenario == "tool-loop" && (index == 8 || (index == 9 && provider == "openai")) {
                    400
                } else if index + 1 == count {
                    match scenario {
                        "stale" | "standalone" => 400,
                        "store-false" => 404,
                        _ => 200,
                    }
                } else {
                    200
                };
                assert_eq!(
                    turn.response.status_code,
                    Some(status),
                    "{scenario}/{provider} t{index}"
                );
                let response = body(turn);
                if turn.request.path == "/v1/responses" && status == 200 {
                    assert_eq!(response["status"], "completed", "{scenario}/{provider} t{index}");
                }
                if turn.response.sse.is_none() {
                    continue;
                }
                let events = recorded_named_sse_events(turn);
                assert_eq!(events[0]["type"], "response.created");
                assert_eq!(events[1]["type"], "response.in_progress");
                for pair in events.windows(2) {
                    assert!(
                        pair[0]["sequence_number"].as_u64().expect("sequence number")
                            < pair[1]["sequence_number"].as_u64().expect("sequence number")
                    );
                }
                for (output_index, item) in response["output"].as_array().unwrap().iter().enumerate() {
                    let done = events
                        .iter()
                        .find(|event| {
                            event["type"] == "response.output_item.done"
                                && event["output_index"].as_u64() == Some(u64::try_from(output_index).unwrap())
                        })
                        .expect("every terminal output has a done event");
                    // Compare supported item fields through the shared wire type. vLLM message events also carry
                    // an unrelated `summary: []` extension that the terminal typed message does not expose.
                    let completed: OutputItem = serde_json::from_value(done["item"].clone()).unwrap();
                    let terminal: OutputItem = serde_json::from_value(item.clone()).unwrap();
                    assert_eq!(
                        serde_json::to_value(completed).unwrap(),
                        serde_json::to_value(terminal).unwrap()
                    );
                    if item["type"] == "compaction" {
                        assert!(events.iter().any(|event| {
                            event["type"] == "response.output_item.added"
                                && event["item"]["id"] == item["id"]
                                && event["output_index"] == done["output_index"]
                        }));
                    }
                }
            }
        }
    }
}

#[test]
fn mcp_discovery_is_reused_until_compaction_in_both_providers() {
    for provider in ["openai", "gateway"] {
        let recording = cassette(&format!("compaction-mcp-{provider}.yaml"));
        let turns = &recording.turns;
        for index in [0, 3, 5, 9] {
            assert_eq!(
                kinds(&body(&turns[index])["output"]),
                ["mcp_list_tools", "message"],
                "{provider} t{index}"
            );
        }
        for index in [2, 7, 8, 10] {
            assert_eq!(
                kinds(&body(&turns[index])["output"]),
                ["message"],
                "{provider} t{index}"
            );
        }
        assert_eq!(kinds(&body(&turns[1])["output"]), ["message", "compaction"]);
        assert_eq!(
            kinds(&body(&turns[4])["output"]),
            ["compaction", "message", "compaction"]
        );
        assert_eq!(kinds(&body(&turns[6])["data"]), ["compaction", "message"]);
        assert_eq!(body(&turns[9])["output"][0]["tools"], serde_json::json!([]));
    }
}

#[test]
fn recorded_response_chains_continue_from_the_last_compaction() {
    for provider in ["openai", "gateway"] {
        for scenario in ["in-turn-chain", "in-turn-chain-stream"] {
            let recording = cassette(&format!("compaction-{scenario}-{provider}.yaml"));
            let turns = &recording.turns;
            let compacting = body(&turns[1]);
            assert_eq!(kinds(&compacting["output"]), ["compaction", "message", "compaction"]);
            assert_eq!(kinds(&body(&turns[2])["data"]), ["message", "message", "message"]);
            let continued = body(&turns[4]);
            assert_eq!(kinds(&continued["data"]), ["compaction", "message"]);
            assert_eq!(
                continued["data"][0]["id"],
                compacting["output"].as_array().unwrap().last().unwrap()["id"]
            );
            assert_eq!(continued["data"][1]["content"][0]["text"], turns[3].request.body.input);
        }
    }
}

#[test]
fn recorded_compaction_preserves_pending_tool_call_identity() {
    for provider in ["openai", "gateway"] {
        let recording = cassette(&format!("compaction-tool-loop-{provider}.yaml"));
        let calling = body(&recording.turns[1]);
        assert_eq!(kinds(&calling["output"]), ["compaction", "function_call"]);
        let call = calling["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap();
        let inputs = body(&recording.turns[3]);
        assert_eq!(
            kinds(&inputs["data"]),
            ["compaction", "function_call", "function_call_output"]
        );
        let items = inputs["data"].as_array().unwrap();
        let stored_call = items.iter().find(|item| item["type"] == "function_call").unwrap();
        let result = items.last().unwrap();
        assert_eq!(stored_call["id"], call["id"]);
        assert_eq!(result["call_id"], call["call_id"]);
        assert_eq!(result["output"], "18 degrees and sunny");
    }
}

#[test]
fn recorded_async_results_after_compaction_cover_immediate_and_delayed_continuations() {
    for provider in ["openai", "gateway"] {
        let recording = cassette(&format!("compaction-tool-loop-{provider}.yaml"));
        let turns = &recording.turns;
        let pending = body(&turns[4]);
        let call = pending["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call" && item["async"] == true)
            .unwrap();
        assert!(kinds(&body(&turns[5])["output"]).contains(&"compaction"));
        assert_eq!(kinds(&body(&turns[7])["data"]), ["compaction", "message"]);
        for index in [8, 9] {
            assert_eq!(turns[index].request.body.input[0]["call_id"], call["call_id"]);
        }
        let delayed = body(&turns[8]);
        assert_eq!(turns[8].response.status_code, Some(400));
        assert_eq!(delayed["error"]["type"], "invalid_request_error");
        assert_eq!(delayed["error"]["param"], "input");
        assert!(
            delayed["error"]["message"]
                .as_str()
                .unwrap()
                .contains(call["call_id"].as_str().unwrap())
        );
        // Observed difference: OpenAI already rejects the result on the first post-compaction continuation.
        let immediate = body(&turns[9]);
        if provider == "openai" {
            assert_eq!(turns[9].response.status_code, Some(400));
            assert_eq!(immediate["error"], delayed["error"]);
        } else {
            assert_eq!(turns[9].response.status_code, Some(200));
            assert_eq!(immediate["status"], "completed");
        }
    }
}

#[test]
fn recorded_stale_compaction_keeps_concurrent_turns_in_public_history() {
    for provider in ["openai", "gateway"] {
        let recording = cassette(&format!("compaction-stale-{provider}.yaml"));
        let turns = &recording.turns;
        let compacting = body(&turns[2]);
        let side = body(&turns[3]);
        let conversation = body(&turns[4]);
        let items = conversation["data"].as_array().unwrap();
        for response in [&compacting, &side] {
            for output in response["output"].as_array().unwrap() {
                assert!(items.iter().any(|item| item["id"] == output["id"]));
            }
        }
        for turn in [&turns[5], &turns[6]] {
            assert_eq!(kinds(&body(turn)["data"]), ["message", "message", "message"]);
        }
        let continued = body(&turns[8]);
        assert_eq!(kinds(&continued["data"]), ["compaction", "message"]);
        assert_eq!(
            continued["data"][0]["id"],
            compacting["output"].as_array().unwrap().last().unwrap()["id"]
        );
        assert_eq!(body(&turns[9])["error"]["type"], "invalid_request_error");
    }
}

#[test]
fn recorded_standalone_windows_and_unstored_compaction_match_the_public_contract() {
    for provider in ["openai", "gateway"] {
        let standalone = cassette(&format!("compaction-standalone-{provider}.yaml"));
        for index in [0, 3] {
            let compacted = body(&standalone.turns[index]);
            assert_eq!(compacted["object"], "response.compaction");
            assert_eq!(kinds(&compacted["output"]), ["message", "compaction"]);
        }
        let compacted = body(&standalone.turns[0]);
        let replay = standalone.turns[1].request.body.input.as_array().unwrap();
        assert_eq!(&replay[..2], compacted["output"].as_array().unwrap());
        let unstored = cassette(&format!("compaction-store-false-{provider}.yaml"));
        assert!(!unstored.turns[0].request.body.store);
        assert_eq!(
            kinds(&body(&unstored.turns[0])["output"]),
            ["compaction", "message", "compaction"]
        );
        assert_eq!(unstored.turns[1].response.status_code, Some(404));
        assert_eq!(body(&unstored.turns[1])["error"]["type"], "invalid_request_error");
    }
}
