//! Actual model, You search, and MCP exchanges recorded by the existing dependency proxy.
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs::read_to_string;
use std::sync::Arc;

use agentic_core::types::agent_commands::AgentCommand;
use agentic_core::types::io::MultiAgentAction;
use agentic_core::types::io::ResponsesInput;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

#[derive(Deserialize)]
struct Capture {
    turns: Vec<Exchange>,
}

#[derive(Deserialize)]
struct Exchange {
    request: Request,
    response: RecordedResponse,
}

#[derive(Debug, PartialEq, Deserialize)]
struct Request {
    method: String,
    path: String,
    body: Value,
    query_params: BTreeMap<String, String>,
}

#[derive(Debug, PartialEq, Deserialize)]
struct RecordedResponse {
    status_code: u16,
    headers: BTreeMap<String, String>,
    body: Option<Value>,
    sse: Option<Vec<String>>,
}

struct Replay {
    turns: Vec<Option<Exchange>>,
    errors: Vec<String>,
}

pub struct Dependencies {
    pub url: String,
    state: Arc<Mutex<Replay>>,
    task: JoinHandle<()>,
}

impl Drop for Dependencies {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Dependencies {
    pub async fn start(path: &str) -> Self {
        let capture: Capture = serde_yaml::from_str(&read_to_string(path).expect("dependency cassette")).unwrap();
        let mut turns: Vec<Option<Exchange>> = Vec::new();
        let mut setup: HashMap<String, usize> = HashMap::new();
        for turn in capture
            .turns
            .into_iter()
            .filter(|turn| turn.request.path != "/health")
            .filter(|turn| !(turn.request.method != "POST" && turn.response.status_code == 405))
        {
            let method = turn.request.body["method"].as_str().unwrap_or_default();
            if turn.request.path == "/openai/tiktoken"
                && matches!(method, "initialize" | "notifications/initialized" | "tools/list")
            {
                if let Some(index) = setup.get(method) {
                    let first: &Exchange = turns[*index].as_ref().unwrap();
                    // This fixture's MCP work finishes before the client shell
                    // continuation. Require one connection/discovery setup;
                    // another live setup request has no matching exchange.
                    assert_eq!(turn.request, first.request, "recorded MCP setup changed");
                    assert_eq!(turn.response, first.response, "recorded MCP setup changed");
                    continue;
                }
                setup.insert(method.to_owned(), turns.len());
            }
            turns.push(Some(turn));
        }
        let state = Arc::new(Mutex::new(Replay {
            turns,
            errors: Vec::new(),
        }));
        let router = Router::new().fallback(replay).with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, state, task }
    }

    pub async fn assert_consumed(&self) {
        let state = self.state.lock().await;
        assert!(state.errors.is_empty(), "dependency mismatches: {:?}", state.errors);
        let remaining: Vec<_> = state
            .turns
            .iter()
            .flatten()
            .map(|turn| (&turn.request.path, &turn.request.body))
            .collect();
        assert!(remaining.is_empty(), "unconsumed dependency requests: {remaining:?}");
    }

    pub async fn assert_no_errors(&self) {
        let state = self.state.lock().await;
        assert!(state.errors.is_empty(), "dependency mismatches: {:?}", state.errors);
    }

    pub async fn max_output_tokens(&self) -> Option<u64> {
        self.state
            .lock()
            .await
            .turns
            .iter()
            .flatten()
            .find(|turn| turn.request.path == "/v1/responses")
            .and_then(|turn| turn.request.body["max_output_tokens"].as_u64())
    }

    pub async fn shell_results(&self) -> HashMap<String, Value> {
        let state = self.state.lock().await;
        let mut results = HashMap::new();
        for turn in state.turns.iter().flatten() {
            let Some(input) = turn.request.body["input"].as_array() else {
                continue;
            };
            for call in input
                .iter()
                .filter(|item| item["type"] == "function_call" && item["name"] == "shell")
            {
                let Some(result) = input
                    .iter()
                    .find(|item| item["type"] == "function_call_output" && item["call_id"] == call["call_id"])
                else {
                    continue;
                };
                let action: Value = serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
                let outputs: Vec<Value> = serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
                let commands = action["commands"].as_array().unwrap();
                assert_eq!(commands.len(), outputs.len(), "one recorded result per command");
                for (index, command) in commands.iter().enumerate() {
                    if let Some(previous) = results.insert(command.as_str().unwrap().to_owned(), outputs[index].clone())
                    {
                        assert_eq!(previous, outputs[index], "shell fixture must be deterministic");
                    }
                }
            }
        }
        assert!(
            !results.is_empty(),
            "dependency trace must capture client shell results"
        );
        results
    }
}

fn agent(body: &Value) -> Option<&str> {
    body["input"].as_array()?.last()?["content"]
        .as_str()?
        .strip_prefix("You are `")?
        .split('`')
        .next()
}

fn normalize(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            // Output-item and JSON-RPC request IDs are allocated independently on replay.
            fields.remove("id");
            // Older captures omit the now-canonical empty logprob array.
            if fields.get("type").and_then(Value::as_str) == Some("output_text")
                && fields.get("logprobs").is_none_or(Value::is_null)
            {
                fields.insert("logprobs".into(), json!([]));
            }
            for field in fields.values_mut() {
                normalize(field);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize),
        Value::String(text) if text.starts_with("You are `") => {
            // Concurrent sibling progress changes capacity and live states between requests.
            *text = text
                .lines()
                .filter(|line| !line.starts_with("Shared subagent capacity:"))
                .map(|line| {
                    if line.starts_with("/root/") {
                        line.split_once(": ").map_or(line, |(name, _)| name)
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        _ => {}
    }
}

// Compare typed collaboration arguments independently of JSON formatting.
fn normalize_request(body: &mut Value) {
    normalize(body);
    let Some(input) = body["input"].as_array_mut() else {
        return;
    };
    // This fixture declares this MCP tool on the gateway. Its public record
    // retains the result but not the private inference call ID. Compare the
    // call/result relationship without changing client-owned call IDs.
    let mut mcp_calls = HashMap::new();
    for item in input.iter_mut() {
        if item["type"] == "function_call" && item["name"] == "mcp__gitmcp_tiktoken__search_tiktoken_documentation" {
            let id = item["call_id"].as_str().expect("MCP function call ID").to_owned();
            let canonical = format!("recorded_mcp_{}", mcp_calls.len());
            assert!(
                mcp_calls.insert(id, canonical.clone()).is_none(),
                "duplicate MCP call ID"
            );
            item["call_id"] = json!(canonical);
        } else if item["type"] == "function_call_output"
            && let Some(canonical) = item["call_id"].as_str().and_then(|id| mcp_calls.get(id))
        {
            item["call_id"] = json!(canonical);
        }
        if item["type"] != "function_call" {
            continue;
        }
        let name = item["name"].as_str().unwrap_or_default();
        if let Some(action) = MultiAgentAction::from_tool_name(name)
            && let Ok(command) = AgentCommand::parse(action, item["arguments"].as_str().unwrap())
        {
            item["arguments"] = json!(serde_json::to_string(&command).unwrap());
        }
    }
}

#[test]
fn gateway_mcp_comparison_preserves_pairing_and_payloads() {
    let fixture = json!({"input":[
        {"type":"function_call","name":"mcp__gitmcp_tiktoken__search_tiktoken_documentation",
            "call_id":"provider_call","arguments":"{\"query\":\"encoding\"}"},
        {"type":"function_call_output","call_id":"provider_call","output":"result","status":"completed"}
    ]});
    let mut expected = fixture.clone();
    normalize_request(&mut expected);
    let mut returned = fixture.clone();
    returned["input"][0]["call_id"] = json!("mcp_public");
    returned["input"][1]["call_id"] = json!("mcp_public");
    normalize_request(&mut returned);
    assert_eq!(expected, returned);
    for (index, field, value) in [
        (1, "call_id", "unmatched"),
        (1, "output", "different result"),
        (0, "arguments", "{}"),
        (1, "status", "incomplete"),
    ] {
        let mut changed = fixture.clone();
        changed["input"][index][field] = json!(value);
        normalize_request(&mut changed);
        assert_ne!(expected, changed, "{field} must remain observable");
    }
}

#[test]
fn full_history_normalization_preserves_client_call_identity() {
    let mut expected = json!({"input":[{"type":"function_call","name":"shell","call_id":"original",
        "arguments":"{\"commands\":[\"printf 55\"]}"},
        {"type":"function_call_output","call_id":"original","output":"55","status":"completed"}]});
    let mut actual = expected.clone();
    actual["input"][0]["call_id"] = json!("changed");
    actual["input"][1]["call_id"] = json!("changed");
    normalize_request(&mut expected);
    normalize_request(&mut actual);
    assert_ne!(expected, actual);
}

#[test]
fn canonical_expected_status_does_not_mask_missing_or_changed_actual_status() {
    let recorded = json!([{"type":"function_call_output", "call_id":"call_1", "output":"result"}]);
    let input: ResponsesInput = serde_json::from_value(recorded.clone()).unwrap();
    let mut expected = serde_json::to_value(input.into_canonical_items()).unwrap();
    normalize(&mut expected);
    for status in [
        None,
        Some(Value::Null),
        Some(json!("in_progress")),
        Some(json!("incomplete")),
    ] {
        let mut actual = recorded.clone();
        if let Some(status) = status {
            actual[0]["status"] = status;
        }
        normalize(&mut actual);
        assert_ne!(expected, actual);
    }
    let mut actual = recorded;
    actual[0]["status"] = json!("completed");
    normalize(&mut actual);
    assert_eq!(expected, actual);
}

async fn replay(State(state): State<Arc<Mutex<Replay>>>, method: Method, uri: Uri, bytes: Bytes) -> Response {
    if uri.path() == "/health" {
        return StatusCode::OK.into_response();
    }
    if matches!(method, Method::GET | Method::DELETE) && uri.path() == "/openai/tiktoken" {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let body = if bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice::<Value>(&bytes).unwrap()
    };
    let query: BTreeMap<String, String> = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .into_owned()
        .collect();
    let mut state = state.lock().await;
    let index = state.turns.iter().position(|turn| {
        turn.as_ref().is_some_and(|turn| {
            if turn.request.method != method.as_str()
                || turn.request.path != uri.path()
                || turn.request.query_params != query
            {
                return false;
            }
            if uri.path() == "/v1/responses" {
                return agent(&turn.request.body) == agent(&body);
            }
            let mut expected = turn.request.body.clone();
            let mut actual = body.clone();
            normalize(&mut expected);
            normalize(&mut actual);
            expected == actual
        })
    });
    let Some(index) = index else {
        state.errors.push(format!(
            "unexpected {method} {} for {:?}: {body}",
            uri.path(),
            agent(&body)
        ));
        return (StatusCode::BAD_GATEWAY, "unexpected dependency request").into_response();
    };
    let turn = state.turns[index].take().unwrap();
    let mut expected = turn.request.body;
    let mut actual = body.clone();
    if uri.path() == "/v1/responses" {
        // These captures predate explicit upstream storage forwarding. Require
        // store:false on every live model round rather than ignoring the field.
        expected.as_object_mut().unwrap().entry("store").or_insert(json!(false));
        // Upgrade legacy expected history through the existing canonical input
        // contract. Actual requests must supply the resulting statuses themselves.
        let input: ResponsesInput = serde_json::from_value(expected["input"].take()).unwrap();
        expected["input"] = serde_json::to_value(input.into_canonical_items()).unwrap();
    }
    normalize_request(&mut expected);
    normalize_request(&mut actual);
    if actual != expected {
        state.errors.push(format!(
            "request differs for {:?}\nexpected: {expected}\nactual: {actual}",
            agent(&body)
        ));
        return (StatusCode::BAD_GATEWAY, "dependency request differs from capture").into_response();
    }
    drop(state);
    let content_type = turn
        .response
        .headers
        .get("content-type")
        .cloned()
        .unwrap_or_else(|| "application/json".into());
    let text = match (turn.response.sse, turn.response.body) {
        (Some(lines), _) => lines.concat(),
        (_, Some(Value::String(text))) => text,
        (_, Some(mut value)) => {
            if value.get("jsonrpc").is_some() {
                value["id"] = body["id"].clone();
            }
            value.to_string()
        }
        _ => String::new(),
    };
    let text = if uri.path() == "/openai/tiktoken" && content_type.contains("text/event-stream") {
        text.split_inclusive('\n').fold(String::new(), |mut output, line| {
            if let Some(data) = line.strip_prefix("data: ")
                && let Ok(mut value) = serde_json::from_str::<Value>(data)
            {
                value["id"] = body["id"].clone();
                writeln!(output, "data: {value}").unwrap();
            } else {
                output.push_str(line);
            }
            output
        })
    } else {
        text
    };
    (
        StatusCode::from_u16(turn.response.status_code).unwrap(),
        [("content-type", content_type)],
        text,
    )
        .into_response()
}
