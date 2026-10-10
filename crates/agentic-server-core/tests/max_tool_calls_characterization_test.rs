//! Recorded Responses API `max_tool_calls` behavior that the gateway implementation mirrors.
//!
//! Legs are documented in `record_max_tool_calls_cassettes.sh`. These tests pin
//! the reference contract and check the gateway's request validation against it.

use agentic_core::executor::ExecutorError;
use agentic_core::types::request_response::RequestPayload;
use serde_json::Value;

mod support;

const CASSETTE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/max_tool_calls");
const OPENAI: &str = "openai-reference-gpt-5.6";

fn cassette(group: &str, mode: &str) -> support::Cassette {
    support::load_cassette(&format!("{CASSETTE_DIR}/max-tool-calls-{group}-{OPENAI}{mode}.yaml"))
}

fn terminal(turn: &support::Turn) -> Value {
    if let Some(body) = &turn.response.body {
        return body.clone();
    }
    support::recorded_named_sse_events(turn)
        .into_iter()
        .find(|event| event["type"] == "response.completed")
        .and_then(|event| event.get("response").cloned())
        .expect("streaming turn records response.completed")
}

fn items<'a>(response: &'a Value, kind: &str) -> Vec<&'a Value> {
    response["output"]
        .as_array()
        .expect("response output")
        .iter()
        .filter(|item| item["type"] == kind)
        .collect()
}

fn statuses(response: &Value, kind: &str) -> Vec<String> {
    items(response, kind)
        .iter()
        .map(|item| item["status"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn gateway_validation_matches_recorded_openai_errors() {
    let recorded = cassette("validation", "-nonstreaming");
    assert_eq!(recorded.turns.len(), 12);
    for turn in &recorded.turns {
        let sent = turn.request.body.extra["max_tool_calls"].clone();
        let request: RequestPayload =
            serde_json::from_value(serde_json::json!({"model": "m", "input": "x", "max_tool_calls": sent}))
                .expect("any max_tool_calls value parses");
        let response = turn.response.body.as_ref().expect("validation turns are JSON");
        let rejected = response["error"].is_object();
        match request.max_tool_calls_limit() {
            Ok(limit) => {
                assert!(!rejected, "OpenAI rejected {sent}");
                assert_eq!(response["max_tool_calls"], sent);
                assert_eq!(limit.map(std::num::NonZeroU64::get), sent.as_u64());
            }
            Err(error) => {
                assert!(rejected, "OpenAI accepted {sent}");
                let gateway = ExecutorError::from(error);
                assert_eq!(response["error"]["code"], gateway.error_code());
                assert_eq!(response["error"]["param"], gateway.error_param().unwrap_or_default());
                assert_eq!(response["error"]["message"], gateway.error_message());
                assert_eq!(response["error"]["type"], gateway.error_type());
            }
        }
    }
}

#[test]
fn refused_built_in_calls_stay_unfinished_and_responses_complete() {
    for mode in ["-nonstreaming", "-streaming"] {
        let recorded = cassette("builtin", mode);
        let responses: Vec<_> = recorded.turns.iter().map(terminal).collect();
        assert_eq!(
            responses.len(),
            8,
            "builtin legs: sequential, parallel, required, mixed, code, failures, mcp-then-search"
        );
        for (turn, response) in recorded.turns.iter().zip(&responses) {
            assert_eq!(response["status"], "completed");
            assert_eq!(response["incomplete_details"], Value::Null);
            assert_eq!(response["max_tool_calls"], turn.request.body.extra["max_tool_calls"]);
        }
        // sequential, parallel (limit 2) and required: one search refused at `searching`.
        assert_eq!(statuses(&responses[0], "web_search_call"), ["completed", "searching"]);
        assert_eq!(
            statuses(&responses[1], "web_search_call"),
            ["completed", "completed", "searching"]
        );
        assert_eq!(statuses(&responses[2], "web_search_call"), ["completed", "searching"]);
        // Web search and MCP share one budget; listing MCP tools is not counted, and
        // the refused MCP call has no output item.
        assert_eq!(statuses(&responses[3], "web_search_call"), ["completed", "completed"]);
        assert_eq!(items(&responses[3], "mcp_list_tools").len(), 1);
        assert!(items(&responses[3], "mcp_call").is_empty());
        // Code interpreter: the refused execution stops at `interpreting`, including
        // a retry after an execution that raised.
        assert_eq!(
            statuses(&responses[4], "code_interpreter_call"),
            ["completed", "interpreting"]
        );
        assert_eq!(
            statuses(&responses[5], "code_interpreter_call"),
            ["completed", "interpreting"]
        );
        // A failed MCP call still consumed the budget; its retry was omitted.
        assert_eq!(statuses(&responses[6], "mcp_call"), ["failed"]);
        // mcp-then-search: the refused MCP call has no item, yet the later refused
        // search is still shown, so an omitted refusal does not use the public slot.
        assert!(items(&responses[7], "mcp_call").is_empty());
        assert_eq!(statuses(&responses[7], "web_search_call"), ["completed", "searching"]);
    }
}

#[test]
fn client_calls_are_uncounted_and_each_response_has_its_own_budget() {
    for mode in ["-nonstreaming", "-streaming"] {
        let recorded = cassette("counting", mode);
        let responses: Vec<_> = recorded.turns.iter().map(terminal).collect();
        assert_eq!(responses.len(), 8);
        assert_eq!(
            items(&responses[0], "function_call").len(),
            3,
            "functions are not limited"
        );
        assert_eq!(statuses(&responses[1], "web_search_call"), ["completed"]);
        assert_eq!(
            items(&responses[1], "function_call").len(),
            1,
            "a function call follows an exhausted budget"
        );
        // continuation: the second turn omits the limit, so it is neither inherited nor echoed.
        assert_eq!(responses[5]["max_tool_calls"], Value::Null);
        assert_eq!(statuses(&responses[5], "web_search_call"), ["completed"; 3]);
        // exhausted-continuation: a re-sent limit starts a fresh budget.
        assert_eq!(statuses(&responses[6], "web_search_call"), ["completed", "searching"]);
        assert_eq!(statuses(&responses[7], "web_search_call"), ["completed"]);
    }
}

#[test]
fn websocket_refusal_matches_http() {
    let recorded = support::load_cassette(&format!("{CASSETTE_DIR}/max-tool-calls-websocket-{OPENAI}.yaml"));
    let response = terminal(&recorded.turns[0]);
    assert_eq!(response["max_tool_calls"], 1);
    assert_eq!(statuses(&response, "web_search_call"), ["completed", "searching"]);
}
