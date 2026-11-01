#[path = "support/multi_agent_contract.rs"]
mod multi_agent_contract;
mod support;

use multi_agent_contract::{ComparisonPolicy, RecordedSession, assert_multi_agent_contract};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[test]
fn reference_cassettes_have_consistent_multi_agent_contract() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes/multi_agent");
    for suffix in [
        "review-gpt-5.6-sol-nonstreaming",
        "review-gpt-5.6-sol-streaming",
        "proposals-gpt-5.6-sol-nonstreaming",
        "proposals-gpt-5.6-sol-streaming",
        "mixed-tools-gpt-5.6-sol-nonstreaming",
        "mixed-tools-gpt-5.6-sol-streaming",
        "client-owned-tools-gpt-5.6-sol-nonstreaming",
        "client-owned-tools-gpt-5.6-sol-streaming",
        "code-interpreter-gpt-5.6-sol-nonstreaming",
        "code-interpreter-gpt-5.6-sol-streaming",
    ] {
        let path = directory.join(format!("multi-agent-openai-reference-{suffix}.yaml"));
        RecordedSession::load(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    }
}

fn assert_qwen_case(scenario: &str, mode: &str) {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes/multi_agent");
    let reference_path = directory.join(format!(
        "multi-agent-openai-reference-{scenario}-gpt-5.6-sol-{mode}.yaml"
    ));
    let gateway_path = directory.join(format!(
        "multi-agent-gateway-{scenario}-Qwen-Qwen3.6-35B-A3B-FP8-{mode}.yaml"
    ));
    let reference =
        RecordedSession::load(&reference_path).unwrap_or_else(|error| panic!("{}: {error}", reference_path.display()));
    let gateway =
        RecordedSession::load(&gateway_path).unwrap_or_else(|error| panic!("{}: {error}", gateway_path.display()));
    assert_multi_agent_contract(
        &reference,
        &gateway,
        &ComparisonPolicy {
            require_reference_tool_kinds: true,
            minimum_delegated_agents: Some(match scenario {
                "proposals" | "code-interpreter" => 2,
                _ => 3,
            }),
        },
    )
    .unwrap_or_else(|error| panic!("{scenario} {mode}: {error}"));
    if scenario == "code-interpreter" {
        reference.assert_code_interpreter(false).unwrap();
        gateway.assert_code_interpreter(true).unwrap();
    }
    if scenario == "client-owned-tools" {
        reference.assert_client_owned_tools().unwrap();
        gateway.assert_client_owned_tools().unwrap();
    }
}

#[test]
fn review_nonstreaming_matches_reference_contract() {
    assert_qwen_case("review", "nonstreaming");
}

#[test]
fn review_streaming_matches_reference_contract() {
    assert_qwen_case("review", "streaming");
}

#[test]
fn proposals_nonstreaming_matches_reference_contract() {
    assert_qwen_case("proposals", "nonstreaming");
}

#[test]
fn proposals_streaming_matches_reference_contract() {
    assert_qwen_case("proposals", "streaming");
}

#[test]
fn mixed_tools_nonstreaming_matches_reference_contract() {
    assert_qwen_case("mixed-tools", "nonstreaming");
}

#[test]
fn mixed_tools_streaming_matches_reference_contract() {
    assert_qwen_case("mixed-tools", "streaming");
}

#[test]
fn store_false_mixed_tools_openai_matches_stored_contract() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes");
    for mode in ["nonstreaming", "streaming"] {
        let stored = RecordedSession::load(&directory.join(format!(
            "multi_agent/multi-agent-openai-reference-mixed-tools-gpt-5.6-sol-{mode}.yaml"
        )))
        .unwrap();
        let stateless = RecordedSession::load(&directory.join(format!(
            "store_false/multi-agent-store-false-openai-reference-mixed-tools-gpt-5.6-sol-{mode}.yaml"
        )))
        .unwrap();
        assert_multi_agent_contract(
            &stored,
            &stateless,
            &ComparisonPolicy {
                require_reference_tool_kinds: true,
                minimum_delegated_agents: Some(3),
            },
        )
        .unwrap_or_else(|error| panic!("store:false {mode}: {error}"));
    }
}

#[test]
fn store_false_mixed_tools_gateway_matches_openai_contract() {
    assert_stateless_gateway_case("mixed-tools", 3);
}

fn stateless_session(scenario: &str, provider: &str, mode: &str) -> RecordedSession {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes/store_false");
    let model = if provider == "gateway" {
        "Qwen-Qwen3.8-27B-FP8"
    } else {
        "gpt-5.6-sol"
    };
    let path = directory.join(format!(
        "multi-agent-store-false-{provider}-{scenario}-{model}-{mode}.yaml"
    ));
    let capture: serde_json::Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    for turn in capture["turns"].as_array().unwrap() {
        assert_eq!(turn["response"]["status_code"], 200, "{}", path.display());
        assert_eq!(turn["request"]["body"]["store"], false);
        assert!(turn["request"]["body"].get("previous_response_id").is_none());
        assert!(turn["request"]["body"].get("conversation").is_none());
    }
    let cassette: support::Cassette = serde_json::from_value(capture).unwrap();
    let mut declared_servers = HashMap::new();
    let mut listed_servers = HashSet::new();
    for (turn_index, turn) in cassette.turns.iter().enumerate() {
        for server in turn.request.body.tools.iter().filter(|tool| tool["type"] == "mcp") {
            declared_servers
                .entry(server["server_label"].as_str().unwrap().to_owned())
                .or_insert(turn_index);
        }
        let terminal = turn.response.body.clone().unwrap_or_else(|| {
            support::recorded_named_sse_events(turn)
                .into_iter()
                .find(|event| event["type"] == "response.completed")
                .expect("successful recorded response")["response"]
                .clone()
        });
        for item in terminal["output"].as_array().unwrap() {
            assert!(item["id"].as_str().is_some_and(|id| !id.is_empty()));
            if item["type"] == "mcp_list_tools" {
                let label = item["server_label"].as_str().unwrap();
                assert_eq!(
                    declared_servers.get(label),
                    Some(&turn_index),
                    "discovery must occur when its server is introduced"
                );
                assert!(
                    listed_servers.insert(label.to_owned()),
                    "equivalent discovery must not repeat across agents or turns"
                );
                assert!(
                    item.get("error").is_none_or(serde_json::Value::is_null),
                    "{}: unexpected MCP discovery failure: {item}",
                    path.display()
                );
            }
            if matches!(
                item["type"].as_str(),
                Some(
                    "function_call"
                        | "custom_tool_call"
                        | "shell_call"
                        | "tool_search_call"
                        | "web_search_call"
                        | "mcp_call"
                        | "code_interpreter_call"
                )
            ) {
                // Inspect the captured bytes before typed serde defaults can
                // fill an omitted or null function-call status.
                assert_eq!(
                    item.get("status"),
                    Some(&serde_json::json!("completed")),
                    "{}: {item}",
                    path.display()
                );
            }
        }
    }
    assert_eq!(listed_servers, declared_servers.into_keys().collect());
    let session = if provider == "openai-reference" && scenario == "code-interpreter" && mode == "streaming" {
        // The separate wire regression pins this recording's negative-index events
        // and strict rejection. Only terminal response semantics are compared here.
        RecordedSession::from_terminal_responses(&cassette)
    } else {
        RecordedSession::from_cassette(&cassette)
    }
    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    session
        .assert_stateless()
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    session
}

fn assert_stateless_gateway_case(scenario: &str, children: usize) {
    for mode in ["nonstreaming", "streaming"] {
        let reference = stateless_session(scenario, "openai-reference", mode);
        let gateway = stateless_session(scenario, "gateway", mode);
        assert_multi_agent_contract(
            &reference,
            &gateway,
            &ComparisonPolicy {
                require_reference_tool_kinds: true,
                minimum_delegated_agents: Some(children),
            },
        )
        .unwrap_or_else(|error| panic!("store:false gateway {scenario} {mode}: {error}"));
        if scenario == "tool-search" {
            reference.assert_client_owned_tools().unwrap();
            gateway.assert_client_owned_tools().unwrap();
            reference.assert_hosted_tool_search_continuation().unwrap();
            gateway.assert_hosted_tool_search_continuation().unwrap();
        } else if scenario == "code-interpreter" {
            reference.assert_code_interpreter(false).unwrap();
            gateway.assert_code_interpreter(true).unwrap();
        }
    }
}

#[test]
fn store_false_tool_search_gateway_matches_openai_contract() {
    assert_stateless_gateway_case("tool-search", 3);
}

#[test]
fn store_false_code_interpreter_gateway_matches_openai_terminal_contract() {
    assert_stateless_gateway_case("code-interpreter", 2);
}

#[test]
fn store_false_client_agent_message_gateway_matches_openai_contract() {
    assert_stateless_gateway_case("client-agent-message", 1);
}

#[test]
fn client_owned_tools_nonstreaming_matches_reference_contract() {
    assert_qwen_case("client-owned-tools", "nonstreaming");
}

#[test]
fn client_owned_tools_streaming_matches_reference_contract() {
    assert_qwen_case("client-owned-tools", "streaming");
}

#[test]
fn code_interpreter_nonstreaming_matches_reference_contract() {
    assert_qwen_case("code-interpreter", "nonstreaming");
}

#[test]
fn code_interpreter_streaming_matches_reference_contract() {
    assert_qwen_case("code-interpreter", "streaming");
}

#[test]
#[ignore = "requires independently recorded OpenAI and gateway cassettes"]
fn compare_recorded_multi_agent_contract() {
    let reference = std::env::var("MULTI_AGENT_REFERENCE_CASSETTE").expect("set reference cassette path");
    let gateway = std::env::var("MULTI_AGENT_GATEWAY_CASSETTE").expect("set gateway cassette path");
    let reference = RecordedSession::load(Path::new(&reference)).unwrap();
    let gateway = RecordedSession::load(Path::new(&gateway)).unwrap();
    assert_multi_agent_contract(
        &reference,
        &gateway,
        &ComparisonPolicy {
            require_reference_tool_kinds: true,
            minimum_delegated_agents: None,
        },
    )
    .unwrap();
}

#[test]
fn recorded_client_agent_message_openai_requests_are_accepted() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes/store_false");
    for mode in ["nonstreaming", "streaming"] {
        let path = directory.join(format!(
            "multi-agent-store-false-openai-reference-client-agent-message-gpt-5.6-sol-{mode}.yaml"
        ));
        let cassette = support::load_cassette(path.to_str().unwrap());
        let body = &cassette.turns[0].request.body;
        assert!(!body.store);
        assert_eq!(body.extra["multi_agent"]["enabled"], true);
        assert!(
            body.input
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "agent_message"
                    && item["author"] == "/root"
                    && item["recipient"] == "/root")
        );
        RecordedSession::from_cassette(&cassette).unwrap();
    }
}

#[test]
fn store_false_tool_search_matches_stored_openai_contract() {
    assert_store_false_tool_case("tool-search", 3);
}

#[test]
fn store_false_code_interpreter_matches_stored_openai_contract() {
    assert_store_false_tool_case("code-interpreter", 2);
}

#[test]
fn recorded_openai_interpreter_stream_keeps_its_wire_incompatibility_visible() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "tests/cassettes/store_false/multi-agent-store-false-openai-reference-code-interpreter-gpt-5.6-sol-streaming.yaml",
    );
    let cassette = support::load_cassette(path.to_str().unwrap());
    let events = support::recorded_named_sse_events(&cassette.turns[0]);
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(
            event["sequence_number"].as_u64(),
            Some(u64::try_from(sequence).unwrap())
        );
    }
    let negative: Vec<_> = events
        .iter()
        .filter(|event| event["output_index"].as_i64() == Some(-1))
        .collect();
    assert_eq!(negative.len(), 22);
    assert_eq!(
        negative
            .iter()
            .filter(|event| event["type"] == "response.output_text.delta")
            .count(),
        19
    );
    assert_eq!(
        negative
            .iter()
            .filter(|event| event["type"] == "response.output_text.annotation.added")
            .count(),
        3
    );
    match RecordedSession::from_cassette(&cassette) {
        Ok(_) => panic!("recorded negative-index events must remain rejected by the strict reader"),
        Err(error) => assert_eq!(
            error.to_string(),
            "multi-agent contract mismatch: non-contiguous SSE sequence"
        ),
    }
    RecordedSession::from_terminal_responses(&cassette)
        .unwrap()
        .assert_code_interpreter(false)
        .unwrap();
}

fn assert_store_false_tool_case(scenario: &str, children: usize) {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes");
    let stored_scenario = if scenario == "tool-search" {
        "client-owned-tools"
    } else {
        scenario
    };
    for mode in ["nonstreaming", "streaming"] {
        let stored = RecordedSession::load(&directory.join(format!(
            "multi_agent/multi-agent-openai-reference-{stored_scenario}-gpt-5.6-sol-{mode}.yaml"
        )))
        .unwrap();
        let stateless = stateless_session(scenario, "openai-reference", mode);
        assert_multi_agent_contract(
            &stored,
            &stateless,
            &ComparisonPolicy {
                require_reference_tool_kinds: true,
                minimum_delegated_agents: Some(children),
            },
        )
        .unwrap();
        if scenario == "tool-search" {
            stateless.assert_client_owned_tools().unwrap();
            stateless.assert_hosted_tool_search_continuation().unwrap();
        } else {
            stateless.assert_code_interpreter(false).unwrap();
        }
    }
}

#[test]
fn tool_search_session_still_requires_a_root_final_answer() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "tests/cassettes/store_false/multi-agent-store-false-openai-reference-tool-search-gpt-5.6-sol-nonstreaming.yaml");
    let mut recording: serde_json::Value = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for turn in recording["turns"].as_array_mut().unwrap() {
        for side in ["request", "response"] {
            let field = if side == "request" { "input" } else { "output" };
            if let Some(items) = turn[side]["body"][field].as_array_mut() {
                for item in items {
                    if item["type"] == "message" && item["agent"]["agent_name"] == "/root" {
                        item["phase"] = serde_json::json!("commentary");
                    }
                }
            }
        }
    }
    let cassette: support::Cassette = serde_json::from_value(recording).unwrap();
    let session = RecordedSession::from_cassette(&cassette).unwrap();
    let error = assert_multi_agent_contract(
        &session,
        &session,
        &ComparisonPolicy {
            require_reference_tool_kinds: true,
            minimum_delegated_agents: Some(3),
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("no root final answer"));
}
