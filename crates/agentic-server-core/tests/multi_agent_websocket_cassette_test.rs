//! Read-only audit of real duplex captures, distinct from model task-quality grading.
use std::collections::{HashMap, HashSet};
use std::path::Path;

use agentic_core::types::injection::ResponseInjectRequest;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Capture {
    format: String,
    sessions: Vec<Session>,
}
#[derive(Deserialize)]
struct Session {
    frames: Vec<Frame>,
    close_outcome: CloseOutcome,
}
#[derive(Deserialize)]
struct CloseOutcome {
    kind: String,
    peer_close_received: bool,
}
#[derive(Deserialize)]
struct Frame {
    direction: String,
    opcode: u8,
    fin: bool,
    text: Option<String>,
}

struct Audit {
    prompt: Value,
    agents: HashSet<String>,
    root_final: usize,
    completed: usize,
    accepted: Vec<String>,
    late_rejected: usize,
    errors: Vec<String>,
}

fn audit(provider: &str, scenario: &str) -> Audit {
    let model = if provider == "openai-reference" {
        "gpt-5.6-sol"
    } else {
        "Qwen-Qwen3.6-35B-A3B-FP8"
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "tests/cassettes/multi_agent/multi-agent-{provider}-{scenario}-{model}-websocket.yaml"
    ));
    let capture: Capture = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(capture.format, "responses-websocket-v1");
    let mut result = Audit {
        prompt: Value::Null,
        agents: HashSet::new(),
        root_final: 0,
        completed: 0,
        accepted: Vec::new(),
        late_rejected: 0,
        errors: Vec::new(),
    };
    for session in capture.sessions {
        assert_eq!(session.close_outcome.kind, "completed");
        assert!(session.close_outcome.peer_close_received);
        let mut response_id = String::new();
        let mut sequences = HashMap::new();
        let mut pending: Option<Vec<String>> = None;
        for frame in session.frames {
            if frame.opcode != 1 {
                continue;
            }
            // These particular captures contain complete text frames.
            assert!(frame.fin);
            let event: Value = serde_json::from_str(frame.text.as_deref().unwrap()).unwrap();
            let kind = event["type"].as_str().unwrap();
            if frame.direction == "client" {
                if kind == "response.create" && result.prompt.is_null() {
                    result.prompt = event["input"].clone();
                    assert_eq!(event["store"], true);
                }
                if kind == "response.inject" {
                    let typed: ResponseInjectRequest = serde_json::from_value(event.clone()).unwrap();
                    assert_eq!(serde_json::to_value(&typed.input).unwrap(), event["input"]);
                    assert!(pending.is_none(), "recorder admits only one unacknowledged injection");
                    pending = Some(
                        event["input"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|item| item["type"].as_str().unwrap().to_owned())
                            .collect(),
                    );
                }
                continue;
            }
            if kind == "response.created" {
                event["response"]["id"].as_str().unwrap().clone_into(&mut response_id);
            }
            let id = event["response_id"].as_str().unwrap_or(&response_id);
            if let Some(sequence) = event["sequence_number"].as_u64() {
                if let Some(previous) = sequences.insert(id.to_owned(), sequence) {
                    assert!(previous < sequence, "{}: {event}", path.display());
                }
            }
            match kind {
                "response.inject.created" => result.accepted.extend(pending.take().unwrap()),
                "response.inject.failed" => {
                    assert!(pending.take().is_some());
                    assert_eq!(event["error"]["code"], "response_already_completed");
                    result.late_rejected += 1;
                }
                "error" => result
                    .errors
                    .push(event["error"]["message"].as_str().unwrap().to_owned()),
                "response.completed" => result.completed += 1,
                "response.output_item.done" => {
                    let item = &event["item"];
                    if let Some(agent) = item["agent"]["agent_name"].as_str() {
                        if agent.starts_with("/root/") {
                            result.agents.insert(agent.to_owned());
                        }
                        if agent == "/root"
                            && item["type"] == "message"
                            && item["role"] == "assistant"
                            && item["phase"] == "final_answer"
                        {
                            result.root_final += 1;
                        }
                    }
                }
                _ => {}
            }
        }
        assert!(
            pending.is_none(),
            "all admitted injections must have an acknowledgement"
        );
    }
    result
}

#[test]
fn completed_workflows_preserve_prompt_delegation_and_response_ordering() {
    for (scenario, agents) in [("review", 3), ("proposals", 2), ("code-interpreter", 2)] {
        let reference = audit("openai-reference", scenario);
        let gateway = audit("gateway", scenario);
        assert_eq!(reference.prompt, gateway.prompt);
        for session in [&reference, &gateway] {
            assert!(session.errors.is_empty());
            assert_eq!(session.completed, 1);
            assert!(session.root_final > 0);
            assert_eq!(session.agents.len(), agents);
        }
        assert_eq!(reference.accepted, gateway.accepted);
    }
    // This checks transport structure, not whether the code-interpreter model
    // satisfied the prompt's four-distinct-problems requirement.
}

#[test]
fn shell_injection_completes_on_both_providers() {
    let reference = audit("openai-reference", "mixed-tools");
    let gateway = audit("gateway", "mixed-tools");
    assert_eq!(reference.prompt, gateway.prompt);
    for session in [&reference, &gateway] {
        assert_eq!(session.accepted, ["shell_call_output"]);
        assert_eq!(session.root_final, 1);
        assert_eq!(session.completed, 1);
        assert_eq!(session.agents.len(), 3);
        assert_eq!(session.late_rejected, 0);
        assert!(session.errors.is_empty());
    }
}

#[test]
fn discovery_and_custom_injections_complete_gateway_task() {
    let reference = audit("openai-reference", "client-owned-tools");
    let gateway = audit("gateway", "client-owned-tools");
    assert_eq!(reference.prompt, gateway.prompt);
    assert!(reference.errors.is_empty());
    assert_eq!(reference.agents.len(), 3);
    assert_eq!(reference.accepted.len(), 3);
    assert!(reference.accepted.iter().any(|kind| kind == "tool_search_output"));
    assert_eq!(reference.completed, 2);
    assert_eq!(reference.late_rejected, 2);
    assert_eq!(
        reference.root_final, 0,
        "recording completion is not root task completion"
    );
    assert!(gateway.errors.is_empty());
    assert_eq!(gateway.completed, 1);
    assert_eq!(gateway.root_final, 1);
    assert_eq!(gateway.agents.len(), 3);
    assert_eq!(gateway.late_rejected, 0);
    assert_eq!(gateway.accepted.len(), 6);
    for (kind, expected) in [
        ("tool_search_output", 3),
        ("function_call_output", 2),
        ("custom_tool_call_output", 1),
    ] {
        assert_eq!(gateway.accepted.iter().filter(|value| *value == kind).count(), expected);
    }
}

mod edge_cases {
    //! Characterization of recorded behavior, including observed gateway differences.
    use std::collections::{HashSet, VecDeque};
    use std::path::Path;

    use serde::Deserialize;
    use serde_json::Value;

    #[derive(Deserialize)]
    struct Capture {
        format: String,
        sessions: Vec<Session>,
    }

    #[derive(Deserialize)]
    struct Session {
        handshake: Value,
        frames: Vec<Frame>,
        close_outcome: Value,
    }

    #[derive(Deserialize)]
    struct Frame {
        direction: String,
        opcode: u8,
        text: Option<String>,
    }

    fn capture(provider: &str, model: &str) -> Capture {
        capture_scenario(provider, model, "ws-edge-cases")
    }

    fn capture_scenario(provider: &str, model: &str, scenario: &str) -> Capture {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "tests/cassettes/multi_agent/multi-agent-{provider}-{scenario}-{model}-websocket.yaml"
        ));
        let capture: Capture = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(capture.format, "responses-websocket-v1");
        assert_eq!(capture.sessions.len(), 4);
        capture
    }

    fn events(session: &Session) -> Vec<Value> {
        session
            .frames
            .iter()
            .filter(|frame| frame.opcode == 1)
            .map(|frame| serde_json::from_str(frame.text.as_deref().unwrap()).unwrap())
            .collect()
    }

    fn audit(session: &Session) -> (Vec<String>, usize) {
        assert_eq!(session.close_outcome["kind"], "completed");
        assert!(session.close_outcome["peer_close_received"].as_bool().unwrap());
        audit_frames(session)
    }

    fn audit_frames(session: &Session) -> (Vec<String>, usize) {
        let mut outcomes = Vec::new();
        let mut pending = VecDeque::new();
        let mut last_sequence = None;
        let mut completed = HashSet::new();
        for frame in &session.frames {
            if frame.opcode != 1 {
                continue;
            }
            let event: Value = serde_json::from_str(frame.text.as_deref().unwrap()).unwrap();
            let kind = event["type"].as_str().unwrap();
            if frame.direction == "client" {
                if kind == "response.inject" {
                    pending.push_back((event["response_id"].clone(), event["input"].clone()));
                }
                continue;
            }
            if kind == "response.created" {
                last_sequence = None;
            }
            if let Some(sequence) = event["sequence_number"].as_u64() {
                assert!(last_sequence.is_none_or(|previous| previous < sequence));
                last_sequence = Some(sequence);
            }
            match kind {
                "response.inject.created" => {
                    assert_eq!(pending.pop_front().unwrap().0, event["response_id"]);
                    outcomes.push("accepted".to_owned());
                }
                "response.inject.failed" => {
                    assert_eq!(
                        Some((event["response_id"].clone(), event["input"].clone())),
                        pending.pop_front()
                    );
                    let code = event["error"]["code"].as_str().unwrap();
                    if code == "response_already_completed" {
                        assert!(
                            completed.contains(event["response_id"].as_str().unwrap()),
                            "late rejection must follow the referenced response's terminal event"
                        );
                    }
                    outcomes.push(code.to_owned());
                }
                "error" | "response.failed" | "response.incomplete" => panic!("unexpected failure: {event}"),
                "response.completed" => {
                    assert!(completed.insert(event["response"]["id"].as_str().unwrap().to_owned()));
                }
                _ => {}
            }
        }
        assert!(pending.is_empty(), "every injection must have an acknowledgement");
        (outcomes, completed.len())
    }

    fn assert_late_continuation(events: &[Value]) {
        let rejected = events
            .iter()
            .find(|event| event["type"] == "response.inject.failed")
            .unwrap();
        let creates = events
            .iter()
            .filter(|event| event["type"] == "response.create")
            .collect::<Vec<_>>();
        assert_eq!(creates.len(), 2);
        assert_eq!(creates[1]["input"], rejected["input"]);
        assert_eq!(creates[1]["previous_response_id"], rejected["response_id"]);
        assert!(events.iter().any(|event| {
            event["type"] == "response.output_item.done"
                && event["item"]["phase"] == "final_answer"
                && event["item"]["content"][0]["text"]
                    .as_str()
                    .is_some_and(|text| text.trim() == "EDGE_OK")
        }));
    }

    #[test]
    fn edge_recordings_confirm_late_continuation_and_distinguish_duplicate_races() {
        let reference = capture("openai-reference", "gpt-5.6-sol");
        let gateway = capture("gateway", "Qwen-Qwen3.6-35B-A3B-FP8");
        let cases = [
            "late-continuation",
            "duplicate-in-batch",
            "mixed-valid-invalid",
            "duplicate-injection",
        ];
        for ((reference, gateway), case) in reference.sessions.iter().zip(&gateway.sessions).zip(cases) {
            assert_eq!(case, reference.handshake["probe"]["case"]);
            assert_eq!(case, gateway.handshake["probe"]["case"]);
            let reference_events = events(reference);
            let gateway_events = events(gateway);
            assert_eq!(reference_events[0]["input"], gateway_events[0]["input"]);
            assert_eq!(reference_events[0]["tools"], gateway_events[0]["tools"]);
            let (reference_outcomes, reference_completed) = audit(reference);
            let (gateway_outcomes, gateway_completed) = audit(gateway);
            assert_eq!(reference_completed, gateway_completed);
            match case {
                "late-continuation" => {
                    assert_eq!(reference_completed, 2);
                    assert_eq!(reference_outcomes, ["response_already_completed"]);
                    assert_eq!(gateway_outcomes, reference_outcomes);
                    assert_late_continuation(&reference_events);
                    assert_late_continuation(&gateway_events);
                }
                "duplicate-injection" => {
                    assert_eq!(reference_completed, 1);
                    assert_eq!(reference_outcomes, ["accepted", "invalid_input"]);
                    // Gateway finalized first. This capture does not exercise live
                    // AlreadyResolved rejection; the gated server test covers it.
                    assert_eq!(
                        gateway_outcomes,
                        ["response_already_completed", "response_already_completed"]
                    );
                }
                "duplicate-in-batch" | "mixed-valid-invalid" => {
                    assert_eq!(reference_completed, 1);
                    // These quiescent captures exercise finalization precedence, not active validation.
                    assert_eq!(reference_outcomes, ["response_already_completed"]);
                    assert_eq!(gateway_outcomes, reference_outcomes);
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn active_reference_acknowledgements_and_responses_complete() {
        let reference = capture_scenario("openai-reference", "gpt-5.6-sol", "ws-active-text-edge-cases");
        for session in &reference.sessions {
            assert_active_session(session);
            // Preserve the observed punctuation difference as model output, not
            // a transport failure or a silently normalized exact-marker match.
            let expected = if session.handshake["probe"]["case"] == "mixed-valid-invalid" {
                "EDGE_OK."
            } else {
                "EDGE_OK"
            };
            assert!(events(session).iter().any(|event| {
                event["type"] == "response.output_item.done"
                    && event["item"]["agent"]["agent_name"] == "/root"
                    && event["item"]["phase"] == "final_answer"
                    && event["item"]["content"][0]["text"].as_str() == Some(expected)
            }));
        }
    }

    #[test]
    fn active_gateway_matches_reference_injection_decisions_and_completes() {
        let reference = capture_scenario("openai-reference", "gpt-5.6-sol", "ws-active-text-edge-cases");
        let gateway = capture_scenario("gateway", "Qwen-Qwen3.6-35B-A3B-FP8", "ws-active-text-edge-cases");
        for (reference, gateway) in reference.sessions.iter().zip(&gateway.sessions) {
            assert_eq!(reference.handshake["probe"]["case"], gateway.handshake["probe"]["case"]);
            let reference_events = events(reference);
            let gateway_events = events(gateway);
            for field in ["input", "tools", "max_output_tokens", "multi_agent"] {
                assert_eq!(reference_events[0][field], gateway_events[0][field]);
            }
            assert_active_session(gateway);
            assert_eq!(audit(reference), audit(gateway));
            assert!(gateway_events.iter().any(|event| {
                event["type"] == "response.output_item.done"
                    && event["item"]["agent"]["agent_name"] == "/root"
                    && event["item"]["phase"] == "final_answer"
                    && event["item"]["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.trim() == "EDGE_OK")
            }));
        }
    }

    fn assert_active_session(session: &Session) {
        let case = session.handshake["probe"]["case"].as_str().unwrap();
        let (outcomes, completed) = audit(session);
        if case == "late-continuation" {
            assert_eq!(outcomes, ["response_already_completed"]);
            assert_eq!(completed, 2);
            assert_late_continuation(&events(session));
            return;
        }
        let probe = &session.close_outcome["probe"];
        assert_eq!(probe["sibling_streaming_observed"], true);
        assert_eq!(probe["injected_during_sibling_execution"], true);
        assert_eq!(probe["active_validation_observed"], true);
        assert_eq!(completed, 1);
        assert_eq!(
            outcomes,
            if case == "duplicate-injection" {
                ["accepted", "invalid_input"]
            } else {
                ["invalid_input", "accepted"]
            }
        );
        let mut terminal = false;
        let mut failed = None;
        let mut injections = Vec::new();
        for event in events(session) {
            match event["type"].as_str().unwrap() {
                "response.completed" => terminal = true,
                "response.inject" => injections.push(event["input"].clone()),
                "response.inject.failed" => {
                    assert!(!terminal, "validation must precede completion");
                    let id = event["input"][0]["call_id"].as_str().unwrap();
                    let message = if case == "mixed-valid-invalid" {
                        format!(
                            "Tool call 'call_edge_unknown' is not pending on response '{}'.",
                            event["response_id"].as_str().unwrap()
                        )
                    } else {
                        format!("Tool call '{id}' already has an output.")
                    };
                    assert_eq!(event["error"]["message"], message);
                    failed = Some(event["input"].clone());
                }
                "response.inject.created" => assert!(!terminal),
                _ => {}
            }
        }
        assert!(failed.is_some());
        assert_eq!(injections.len(), 2);
        // The valid member of a rejected batch is accepted on retry.
        assert_eq!(injections[0][0], injections[1][0]);
        assert_eq!(injections[1].as_array().unwrap().len(), 1);
    }
}
