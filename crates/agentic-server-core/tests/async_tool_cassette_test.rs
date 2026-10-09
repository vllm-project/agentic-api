//! Characterization of async client tool calls (issue #332).
//!
//! Recorded by `tests/cassettes/record_async_tool_cassettes.py`; `tests/cassettes/async_tools/README.md` lists
//! what each recording shows. The `openai-reference` set (`gpt-6-astra`) pins the public contract gateway
//! support must match; the `gateway` set replays the same scenarios through the gateway and a vLLM-served
//! model and is compared with it step by step. Every request carries an `x-run-id: <scenario>/<step>` header,
//! so steps are found by label. These tests replay recordings; they do not run the gateway.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// A recorded provider and model, as they appear in cassette file names.
#[derive(Clone, Copy)]
struct Source {
    provider: &'static str,
    model_slug: &'static str,
}

const OPENAI: Source = Source {
    provider: "openai-reference",
    model_slug: "gpt-6-astra",
};
const NONSTREAMING: &[&str] = &["nonstreaming"];
const STREAMING: &[&str] = &["streaming"];
const BOTH: &[&str] = &["nonstreaming", "streaming"];

/// One scenario, the modes it records, and its request count. Mirrors the driver's scenario table.
#[derive(Clone, Copy)]
struct Recording {
    source: Source,
    scenario: &'static str,
    modes: &'static [&'static str],
    requests: usize,
}

const OPENAI_RECORDINGS: [Recording; 9] = [
    Recording {
        source: OPENAI,
        scenario: "function-delayed-result",
        modes: BOTH,
        requests: 4,
    },
    Recording {
        source: OPENAI,
        scenario: "custom-delayed-result",
        modes: NONSTREAMING,
        requests: 3,
    },
    Recording {
        source: OPENAI,
        scenario: "web-search-mixed",
        modes: STREAMING,
        requests: 2,
    },
    Recording {
        source: OPENAI,
        scenario: "wait-tool",
        modes: NONSTREAMING,
        requests: 2,
    },
    Recording {
        source: OPENAI,
        scenario: "edge-cases",
        modes: NONSTREAMING,
        requests: 11,
    },
    Recording {
        source: OPENAI,
        scenario: "client-tool-types",
        modes: NONSTREAMING,
        requests: 6,
    },
    Recording {
        source: OPENAI,
        scenario: "parallel-mixed",
        modes: NONSTREAMING,
        requests: 3,
    },
    Recording {
        source: OPENAI,
        scenario: "multi-agent-parallel",
        modes: NONSTREAMING,
        requests: 1,
    },
    Recording {
        source: OPENAI,
        scenario: "multi-agent-sequential",
        modes: NONSTREAMING,
        requests: 2,
    },
];

const GATEWAY: Source = Source {
    provider: "gateway",
    model_slug: "Qwen-Qwen3.6-35B-A3B",
};
/// Recorded with no web-search provider configured, so the gateway set has no web-search scenario.
const GATEWAY_SKIPPED: [&str; 1] = ["web-search-mixed"];

/// The gateway set replays the `openai-reference` scenarios through the gateway and a vLLM model.
fn gateway_recordings() -> impl Iterator<Item = Recording> {
    OPENAI_RECORDINGS
        .iter()
        .filter(|recording| !GATEWAY_SKIPPED.contains(&recording.scenario))
        .map(|recording| Recording {
            source: GATEWAY,
            ..*recording
        })
}

fn all_recordings() -> impl Iterator<Item = Recording> {
    OPENAI_RECORDINGS.into_iter().chain(gateway_recordings())
}

/// One recorded request with the outcome a client observed.
struct Step {
    label: String,
    status: u64,
    request: Value,
    /// Terminal response object for HTTP 200 (from `response.completed` when streaming).
    response: Option<Value>,
    /// Error body for non-200 responses.
    error: Option<Value>,
    /// Named SSE events, streaming only.
    events: Vec<Value>,
}

impl Step {
    fn output(&self) -> &[Value] {
        self.response
            .as_ref()
            .and_then(|response| response["output"].as_array())
            .map_or(&[], Vec::as_slice)
    }

    fn calls(&self, item_type: &str) -> Vec<&Value> {
        self.output().iter().filter(|item| item["type"] == item_type).collect()
    }

    fn input_items(&self) -> &[Value] {
        self.request["input"].as_array().map_or(&[], Vec::as_slice)
    }

    fn outputs_for(&self, call_id: &str) -> Vec<&Value> {
        self.input_items()
            .iter()
            .filter(|item| item["call_id"] == call_id && item["type"].as_str().is_some_and(|t| t.ends_with("_output")))
            .collect()
    }

    fn response_id(&self) -> &str {
        self.response
            .as_ref()
            .and_then(|response| response["id"].as_str())
            .expect("response id")
    }
}

fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes/async_tools")
}

fn file_name(source: Source, scenario: &str, mode: &str) -> String {
    format!(
        "async-tool-{}-{scenario}-{}-{mode}.yaml",
        source.provider, source.model_slug
    )
}

fn load(scenario: &str, mode: &str) -> Vec<Step> {
    load_from(OPENAI, scenario, mode)
}

fn load_from(source: Source, scenario: &str, mode: &str) -> Vec<Step> {
    let path = directory().join(file_name(source, scenario, mode));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let document: Value = serde_yaml::from_str(&text).expect("cassette YAML");
    document["turns"]
        .as_array()
        .expect("turns")
        .iter()
        .map(|turn| {
            let label = turn["request"]["headers"]["x-run-id"]
                .as_str()
                .expect("x-run-id label")
                .to_owned();
            let status = turn["response"]["status_code"].as_u64().expect("recorded HTTP status");
            let events = turn["response"]["sse"]
                .as_array()
                .map(|chunks| sse_events(chunks))
                .unwrap_or_default();
            let (response, error) = if status == 200 {
                let response = if events.is_empty() {
                    turn["response"]["body"].clone()
                } else {
                    events
                        .iter()
                        .find(|event| event["type"] == "response.completed")
                        .map(|event| event["response"].clone())
                        .expect("streamed response.completed")
                };
                (Some(response), None)
            } else {
                (None, Some(turn["response"]["body"].clone()))
            };
            Step {
                label,
                status,
                request: turn["request"]["body"].clone(),
                response,
                error,
                events,
            }
        })
        .collect()
}

fn sse_events(chunks: &[Value]) -> Vec<Value> {
    chunks
        .iter()
        .filter_map(Value::as_str)
        .flat_map(str::lines)
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("SSE data JSON"))
        .collect()
}

fn step<'a>(steps: &'a [Step], label: &str) -> &'a Step {
    steps
        .iter()
        .find(|step| step.label == label)
        .unwrap_or_else(|| panic!("no step labelled {label}"))
}

fn single<'a>(items: &[&'a Value], what: &str) -> &'a Value {
    assert_eq!(items.len(), 1, "expected exactly one {what}");
    items[0]
}

fn assert_ok(step: &Step) {
    assert_eq!(step.status, 200, "{} should succeed: {:?}", step.label, step.error);
    let response = step.response.as_ref().expect("response");
    assert_eq!(response["status"], "completed", "{} should complete", step.label);
}

fn assert_error(step: &Step, code: Option<&str>, param: &str, message_prefix: &str) {
    assert_eq!(step.status, 400, "{} should be rejected", step.label);
    let error = &step.error.as_ref().expect("error body")["error"];
    assert_eq!(error["type"], "invalid_request_error", "{}", step.label);
    assert_eq!(error["code"].as_str(), code, "{}", step.label);
    assert_eq!(error["param"], param, "{}", step.label);
    let message = error["message"].as_str().expect("error message");
    assert!(
        message.starts_with(message_prefix),
        "{}: unexpected message {message:?}",
        step.label
    );
}

/// The follow-up chains from `previous` and carries no output for the pending call.
fn assert_follow_up_without_output(step: &Step, previous: &Step, pending_call_id: &str) {
    assert_ok(step);
    assert_eq!(
        step.request["previous_response_id"],
        previous.response_id(),
        "{}",
        step.label
    );
    assert!(
        step.outputs_for(pending_call_id).is_empty(),
        "{} must not answer the pending call",
        step.label
    );
}

// ── the recording set ────────────────────────────────────────────────────────

#[test]
fn every_declared_recording_is_present_masked_and_labelled() {
    let mut expected = BTreeSet::new();
    for recording in all_recordings() {
        for mode in recording.modes {
            let name = file_name(recording.source, recording.scenario, mode);
            let path = directory().join(&name);
            expected.insert(name);
            let text =
                std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let document: Value = serde_yaml::from_str(&text).expect("cassette YAML");
            let turns = document["turns"].as_array().expect("turns");
            assert_eq!(turns.len(), recording.requests, "{}: request count", path.display());
            let mut labels = BTreeSet::new();
            for turn in turns {
                let headers = &turn["request"]["headers"];
                // The vLLM endpoint is called without a key; any recorded key must be masked.
                let authorization = &headers["authorization"];
                assert!(
                    authorization == "Bearer ***"
                        || (recording.source.provider != "openai-reference" && authorization.is_null()),
                    "{}: authorization must be masked, got {authorization}",
                    path.display()
                );
                let label = headers["x-run-id"].as_str().expect("x-run-id");
                assert!(
                    label.starts_with(&format!("{}/", recording.scenario)),
                    "{label} belongs to another scenario"
                );
                assert!(labels.insert(label.to_owned()), "{label} is recorded twice");
                assert_eq!(
                    turn["request"]["body"]["stream"],
                    *mode == "streaming",
                    "{label}: stream flag"
                );
                assert!(
                    turn["response"]["transport_error"].is_null(),
                    "{label}: proxy transport failure"
                );
                assert!(
                    turn["response"]["stream_error"].is_null(),
                    "{label}: stream was cut short"
                );
            }
        }
    }
    // Every recording in the directory is declared, so stale or failed recordings are noticed.
    let present: BTreeSet<String> = std::fs::read_dir(directory())
        .expect("cassette directory")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("yaml"))
        })
        .collect();
    assert_eq!(present, expected, "recordings on disk must match the declared set");
}

// ── OpenAI reference: the core contract ──────────────────────────────────────

/// `function-delayed-result`, both modes.
#[test]
fn async_function_call_stays_pending_across_follow_ups_and_accepts_a_late_output() {
    for mode in BOTH {
        let steps = load("function-delayed-result", mode);
        let start = step(&steps, "function-delayed-result/t1-start");
        assert_ok(start);
        assert_eq!(
            start.request["tools"][0]["async"], true,
            "request declares the tool async"
        );
        assert_eq!(
            start.response.as_ref().expect("response")["tools"][0]["async"],
            true,
            "response echoes async"
        );
        let call = single(&start.calls("function_call"), "function_call");
        assert_eq!(call["async"], true, "{mode}: the emitted call carries async");
        assert!(
            !start.calls("message").is_empty(),
            "{mode}: the model answers alongside the async call"
        );
        let call_id = call["call_id"].as_str().expect("call_id");

        let first = step(&steps, "function-delayed-result/t2-follow-up-without-output");
        assert_follow_up_without_output(first, start, call_id);
        let second = step(&steps, "function-delayed-result/t3-follow-up-without-output");
        assert_follow_up_without_output(second, first, call_id);

        let late = step(&steps, "function-delayed-result/t4-late-output");
        assert_ok(late);
        assert_eq!(
            late.request["previous_response_id"],
            second.response_id(),
            "late output chains from latest"
        );
        let output = single(&late.outputs_for(call_id), "late output");
        assert_eq!(output["type"], "function_call_output");
    }
}

/// `custom-delayed-result`.
#[test]
fn async_custom_tool_call_follows_the_same_contract() {
    let steps = load("custom-delayed-result", "nonstreaming");
    let start = step(&steps, "custom-delayed-result/t1-start");
    assert_ok(start);
    assert_eq!(start.request["tools"][0]["async"], true);
    let call = single(&start.calls("custom_tool_call"), "custom_tool_call");
    assert_eq!(call["async"], true, "custom call carries async");
    assert!(
        !start.calls("message").is_empty(),
        "answer alongside the async custom call"
    );
    let call_id = call["call_id"].as_str().expect("call_id");

    let follow_up = step(&steps, "custom-delayed-result/t2-follow-up-without-output");
    assert_follow_up_without_output(follow_up, start, call_id);
    let late = step(&steps, "custom-delayed-result/t3-late-output");
    assert_ok(late);
    assert_eq!(
        single(&late.outputs_for(call_id), "late output")["type"],
        "custom_tool_call_output"
    );
}

/// `function-delayed-result` and `web-search-mixed`, streaming. The complete async call arrives before any
/// later output item starts, so a client can dispatch it while still consuming the response.
#[test]
fn streaming_completes_the_async_call_before_later_items() {
    for scenario in ["function-delayed-result", "web-search-mixed"] {
        let steps = load(scenario, "streaming");
        let start = step(&steps, &format!("{scenario}/t1-start"));
        assert_ok(start);
        let done_position = start
            .events
            .iter()
            .position(|event| {
                event["type"] == "response.output_item.done"
                    && event["item"]["type"] == "function_call"
                    && event["item"]["async"] == true
            })
            .unwrap_or_else(|| panic!("{scenario}: no completed async function_call"));
        let call_index = start.events[done_position]["output_index"]
            .as_u64()
            .expect("output_index");
        let first_later = start
            .events
            .iter()
            .position(|event| event["output_index"].as_u64().is_some_and(|index| index > call_index))
            .unwrap_or_else(|| panic!("{scenario}: no later output item"));
        assert!(
            done_position < first_later,
            "{scenario}: async call must complete before later items begin"
        );
    }
}

/// `web-search-mixed`, streaming.
#[test]
fn async_call_and_hosted_web_search_share_one_response() {
    let steps = load("web-search-mixed", "streaming");
    let start = step(&steps, "web-search-mixed/t1-start");
    assert_ok(start);
    let call = single(&start.calls("function_call"), "function_call");
    assert_eq!(call["async"], true);
    assert!(
        !start.calls("web_search_call").is_empty(),
        "hosted search runs in the same response"
    );
    assert!(!start.calls("message").is_empty(), "the response still answers");
    let late = step(&steps, "web-search-mixed/t2-late-output");
    assert_ok(late);
    single(
        &late.outputs_for(call["call_id"].as_str().expect("call_id")),
        "late output",
    );
}

// ── OpenAI reference: async calls next to synchronous calls ──────────────────

/// `wait-tool`. Async calls next to a synchronous call end the response; results go before the wait status.
#[test]
fn wait_tool_results_are_delivered_before_wait_status() {
    let steps = load("wait-tool", "nonstreaming");
    let start = step(&steps, "wait-tool/t1-start");
    assert_ok(start);
    let calls = start.calls("function_call");
    let lookups: Vec<_> = calls.iter().filter(|call| call["name"] == "lookup_price").collect();
    let wait = calls
        .iter()
        .find(|call| call["name"] == "wait_for_tasks")
        .expect("wait_for_tasks call");
    assert_eq!(lookups.len(), 2, "two background lookups");
    assert!(lookups.iter().all(|call| call["async"] == true), "lookups are async");
    assert!(
        wait.get("async").is_none_or(|value| value == false),
        "the wait tool stays synchronous"
    );
    assert!(
        start.calls("message").is_empty(),
        "a synchronous call ends the response after the calls"
    );

    let delivery = step(&steps, "wait-tool/t2-deliver-results");
    assert_ok(delivery);
    let delivered: Vec<&str> = delivery
        .input_items()
        .iter()
        .map(|item| item["call_id"].as_str().expect("call_id"))
        .collect();
    let wait_id = wait["call_id"].as_str().expect("call_id");
    assert_eq!(delivered.last(), Some(&wait_id), "wait status comes after the results");
    for lookup in lookups {
        assert!(
            delivered.contains(&lookup["call_id"].as_str().expect("call_id")),
            "result delivered"
        );
    }
}

/// `parallel-mixed`. Async and synchronous calls together end the response; answering only the
/// synchronous call is accepted, and the async call is answered later.
#[test]
fn parallel_async_call_may_stay_pending_after_its_sync_sibling_is_answered() {
    let steps = load("parallel-mixed", "nonstreaming");
    let start = step(&steps, "parallel-mixed/t1-start");
    assert_ok(start);
    assert_eq!(start.request["parallel_tool_calls"], true);
    let calls = start.calls("function_call");
    let weather = calls
        .iter()
        .find(|call| call["name"] == "get_weather")
        .expect("async call");
    let clock = calls
        .iter()
        .find(|call| call["name"] == "get_local_time")
        .expect("sync call");
    assert_eq!(weather["async"], true);
    assert!(clock.get("async").is_none(), "sync sibling has no async marker");
    assert!(
        start.calls("message").is_empty(),
        "a synchronous call ends the response after the calls"
    );

    let sync_only = step(&steps, "parallel-mixed/t2-sync-output-only");
    assert_follow_up_without_output(sync_only, start, weather["call_id"].as_str().expect("call_id"));
    single(
        &sync_only.outputs_for(clock["call_id"].as_str().expect("call_id")),
        "sync output",
    );

    let late = step(&steps, "parallel-mixed/t3-late-async-output");
    assert_ok(late);
    single(
        &late.outputs_for(weather["call_id"].as_str().expect("call_id")),
        "late async output",
    );
}

// ── OpenAI reference: validation and edge cases ──────────────────────────────

/// `edge-cases`.
#[test]
fn edge_case_probes_match_the_recorded_openai_contract() {
    let steps = load("edge-cases", "nonstreaming");
    let by_label: HashMap<&str, &Step> = steps.iter().map(|step| (step.label.as_str(), step)).collect();
    let probe = |label: &str| *by_label.get(format!("edge-cases/{label}").as_str()).expect(label);

    // A synchronous call still requires its output on the next request.
    let sync_start = probe("baseline-sync-unanswered/t1-start");
    assert!(
        single(&sync_start.calls("function_call"), "sync call")
            .get("async")
            .is_none()
    );
    assert_error(
        probe("baseline-sync-unanswered/t2-follow-up-without-output"),
        None,
        "input",
        "No tool output found for function call",
    );

    assert_error(
        probe("unknown-call-id/t2-output"),
        None,
        "input",
        "No tool call found for function call output with call_id call_async_unknown_probe",
    );

    // Repeated and conflicting outputs for one async call are accepted.
    for label in [
        "duplicate-same-request/t2-output-twice",
        "duplicate-across-requests/t2-output",
        "duplicate-across-requests/t3-output-again",
        "conflicting-same-request/t2-two-different-outputs",
    ] {
        assert_ok(probe(label));
    }
    let twice = probe("duplicate-same-request/t2-output-twice");
    assert_eq!(twice.input_items().len(), 2, "both duplicate outputs were sent");

    // Every async probe is a separate branch of one response with the async call.
    let shared = probe("async-call/t1-start");
    assert_eq!(single(&shared.calls("function_call"), "async call")["async"], true);
    for label in [
        "unknown-call-id/t2-output",
        "duplicate-same-request/t2-output-twice",
        "duplicate-across-requests/t2-output",
        "conflicting-same-request/t2-two-different-outputs",
        "redeclared-without-async/t2-follow-up-without-output",
    ] {
        assert_eq!(
            probe(label).request["previous_response_id"],
            shared.response_id(),
            "{label} continues the shared async call"
        );
    }

    // Re-declaring the tool without `async` does not turn the pending call into an unanswered
    // synchronous call: the follow-up without its output is still accepted.
    let redeclared = probe("redeclared-without-async/t2-follow-up-without-output");
    assert_ok(redeclared);
    assert!(
        redeclared.request["tools"][0].get("async").is_none(),
        "re-declared without async"
    );

    assert_error(
        probe("unsupported-model/t1-start"),
        Some("unsupported_value"),
        "tools",
        "Async tools are not supported with gpt-5.5.",
    );
    assert_error(
        probe("async-hosted-web-search/t1-start"),
        Some("unknown_parameter"),
        "tools[0].async",
        "Unknown parameter: 'tools[0].async'.",
    );
}

/// `client-tool-types`. `async` is accepted on a namespace member function and rejected on the namespace
/// itself, the client-executed shell tool, and client-executed tool search.
#[test]
fn async_applies_to_namespace_members_but_not_namespaces_shell_or_tool_search() {
    let steps = load("client-tool-types", "nonstreaming");

    let start = step(&steps, "client-tool-types/namespace-member/t1-start");
    assert_ok(start);
    assert_eq!(start.request["tools"][0]["type"], "namespace");
    assert_eq!(start.request["tools"][0]["tools"][0]["async"], true);
    let call = single(&start.calls("function_call"), "namespace member call");
    assert_eq!(call["async"], true, "the member call carries async");
    assert_eq!(
        call["namespace"], "weather_tools",
        "the member call names its namespace"
    );
    let call_id = call["call_id"].as_str().expect("call_id");
    let follow_up = step(&steps, "client-tool-types/namespace-member/t2-follow-up-without-output");
    assert_follow_up_without_output(follow_up, start, call_id);
    let late = step(&steps, "client-tool-types/namespace-member/t3-late-output");
    assert_ok(late);
    single(&late.outputs_for(call_id), "late output");

    for (probe, tool_type) in [
        ("namespace-itself", "namespace"),
        ("shell", "shell"),
        ("tool-search-client", "tool_search"),
    ] {
        let rejected = step(&steps, &format!("client-tool-types/{probe}/t1-start"));
        assert_eq!(rejected.request["tools"][0]["type"], tool_type);
        assert_eq!(rejected.request["tools"][0]["async"], true);
        assert_error(
            rejected,
            Some("unknown_parameter"),
            "tools[0].async",
            "Unknown parameter: 'tools[0].async'.",
        );
    }
}

/// `multi-agent-parallel` and `multi-agent-sequential`.
#[test]
fn multi_agent_rejects_async_parallel_calls_but_accepts_sequential_async_calls() {
    let parallel = load("multi-agent-parallel", "nonstreaming");
    assert_eq!(parallel.len(), 1, "nothing to continue after the rejection");
    let rejected = &parallel[0];
    assert_eq!(rejected.request["parallel_tool_calls"], true);
    assert_eq!(rejected.request["multi_agent"]["enabled"], true);
    assert_error(
        rejected,
        Some("unsupported_value"),
        "multi_agent",
        "Async parallel tool calls are not yet supported in multi-agent mode.",
    );

    let sequential = load("multi-agent-sequential", "nonstreaming");
    let start = step(&sequential, "multi-agent-sequential/t1-start");
    assert_ok(start);
    assert_eq!(start.request["parallel_tool_calls"], false);
    // With the async output pending, the root agent called the hosted `wait_agent` on itself
    // within the same response; each wait timed out. No other multi-agent action occurred.
    let actions = start.calls("multi_agent_call");
    assert!(!actions.is_empty(), "root agent waited inside the response");
    for action in actions {
        assert_eq!(action["action"], "wait_agent", "only wait_agent actions");
        assert_eq!(action["agent"]["agent_name"], "/root", "the root waited on itself");
    }
    for output in start.calls("multi_agent_call_output") {
        let text = output["output"][0]["text"].as_str().expect("wait output text");
        let result: Value = serde_json::from_str(text).expect("wait output JSON");
        assert_eq!(result["timed_out"], true, "each wait timed out");
    }
    let call = start
        .calls("function_call")
        .into_iter()
        .find(|call| call["name"] == "get_weather")
        .expect("async weather call");
    assert_eq!(call["async"], true);
    let late = step(&sequential, "multi-agent-sequential/t2-late-output");
    assert_ok(late);
    single(
        &late.outputs_for(call["call_id"].as_str().expect("call_id")),
        "late output",
    );
}

// ── gateway: the same scenarios through the gateway and a vLLM model ─────────

/// The hint texts the gateway adds upstream (`prompts.json`), which must never reach the client.
fn upstream_hints() -> Value {
    let path = directory().join("prompts.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str::<Value>(&text).expect("prompts JSON")["upstream_hints"].clone()
}

/// The error envelope with call IDs replaced, so different models' IDs compare equal.
fn normalized_error(step: &Step, call_ids: &[&str]) -> Value {
    let mut error = step.error.as_ref().expect("error body")["error"].clone();
    if let Some(message) = error["message"].as_str() {
        let mut message = message.to_owned();
        for call_id in call_ids {
            message = message.replace(call_id, "<call_id>");
        }
        error["message"] = Value::String(message);
    }
    error
}

fn call_ids(steps: &[Step]) -> Vec<&str> {
    steps
        .iter()
        .flat_map(|step| step.output().iter())
        .filter_map(|item| item["call_id"].as_str())
        .collect()
}

/// Every step the gateway recorded has the status, and for errors the type, code, param, and
/// message, that `OpenAI` returned for the same step. The model probe is the one deliberate
/// difference: the gateway accepts `async` for every model it serves.
#[test]
fn gateway_steps_match_the_openai_statuses_and_errors() {
    for recording in gateway_recordings() {
        for mode in recording.modes {
            let openai = load(recording.scenario, mode);
            let gateway = load_from(GATEWAY, recording.scenario, mode);
            let (openai_ids, gateway_ids) = (call_ids(&openai), call_ids(&gateway));
            assert_eq!(
                gateway.iter().map(|step| step.label.as_str()).collect::<Vec<_>>(),
                openai.iter().map(|step| step.label.as_str()).collect::<Vec<_>>(),
                "{} {mode}: same steps",
                recording.scenario
            );
            for (reference, observed) in openai.iter().zip(&gateway) {
                if observed.label == "edge-cases/unsupported-model/t1-start" {
                    assert_error(
                        reference,
                        Some("unsupported_value"),
                        "tools",
                        "Async tools are not supported",
                    );
                    assert_ok(observed);
                    continue;
                }
                assert_eq!(observed.status, reference.status, "{} {mode}", observed.label);
                if observed.status == 200 {
                    assert_ok(observed);
                } else {
                    assert_eq!(
                        normalized_error(observed, &gateway_ids),
                        normalized_error(reference, &openai_ids),
                        "{} {mode}: error envelope",
                        observed.label
                    );
                }
            }
        }
    }
}

/// The response shapes that carry the contract: async calls are marked, an async-only round
/// still answers in the same response, and a synchronous call ends the response.
#[test]
fn gateway_responses_follow_the_openai_shape() {
    for mode in BOTH {
        let steps = load_from(GATEWAY, "function-delayed-result", mode);
        let start = step(&steps, "function-delayed-result/t1-start");
        let call = single(&start.calls("function_call"), "function_call");
        assert_eq!(call["async"], true, "{mode}: the call is marked async");
        assert!(
            !start.calls("message").is_empty(),
            "{mode}: the gateway continues and answers in the same response"
        );
    }
    let custom = load_from(GATEWAY, "custom-delayed-result", "nonstreaming");
    let start = step(&custom, "custom-delayed-result/t1-start");
    assert_eq!(single(&start.calls("custom_tool_call"), "custom call")["async"], true);
    assert!(!start.calls("message").is_empty());

    for (scenario, label) in [
        ("parallel-mixed", "parallel-mixed/t1-start"),
        ("wait-tool", "wait-tool/t1-start"),
    ] {
        let steps = load_from(GATEWAY, scenario, "nonstreaming");
        let start = step(&steps, label);
        let calls = start.calls("function_call");
        assert!(calls.iter().any(|call| call["async"] == true), "{scenario}: async call");
        assert!(
            calls.iter().any(|call| call.get("async").is_none()),
            "{scenario}: synchronous call"
        );
        assert!(
            start.calls("message").is_empty(),
            "{scenario}: a synchronous call ends the response"
        );
    }

    let streaming = load_from(GATEWAY, "function-delayed-result", "streaming");
    let start = step(&streaming, "function-delayed-result/t1-start");
    let done = start
        .events
        .iter()
        .position(|event| event["type"] == "response.output_item.done" && event["item"]["type"] == "function_call")
        .expect("streamed call");
    assert_eq!(start.events[done]["item"]["async"], true);
    let added = start
        .events
        .iter()
        .position(|event| event["type"] == "response.output_item.added" && event["item"]["type"] == "function_call")
        .expect("streamed call start");
    assert_eq!(start.events[added]["item"]["async"], true);
    let answer = start
        .events
        .iter()
        .position(|event| event["type"] == "response.output_item.added" && event["item"]["type"] == "message")
        .expect("streamed answer");
    assert!(done < answer, "the async call completes before the answer starts");

    // The response echoes the client's declaration: `async: true`, and none of the upstream hint
    // the gateway added to the model-visible description.
    let hint = upstream_hints()["tool_description_suffix"]
        .as_str()
        .expect("suffix")
        .trim()
        .to_owned();
    let blocking = load_from(GATEWAY, "function-delayed-result", "nonstreaming");
    let echoed: Vec<&Value> = start
        .events
        .iter()
        .filter_map(|event| event["response"]["tools"].as_array())
        .flatten()
        .chain(
            blocking[0]
                .response
                .as_ref()
                .and_then(|response| response["tools"].as_array())
                .into_iter()
                .flatten(),
        )
        .collect();
    assert!(!echoed.is_empty(), "lifecycle events and the response echo tools");
    for tool in echoed {
        assert_eq!(tool["async"], true, "{tool}");
        assert!(
            !tool["description"].as_str().unwrap_or_default().contains(&hint),
            "the upstream hint is not returned: {tool}"
        );
    }
}
