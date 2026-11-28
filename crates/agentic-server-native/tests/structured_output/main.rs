//! Replay the native JSON/SSE captures through the shared gateway, including stored continuations.
mod gateway;
mod reference;

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use agentic_core::events::{ClassifiedSseLine, SseLine};
use reqwest::Client;
use serde_json::{Value, json};

const JSON: &str =
    include_str!("../../../agentic-server-core/tests/cassettes/responses/response-structured-output-native-json.yaml");
const SSE: &str =
    include_str!("../../../agentic-server-core/tests/cassettes/responses/response-structured-output-native-sse.yaml");

fn array(value: &Value) -> &[Value] {
    value.as_array().unwrap()
}
fn value_str(value: &Value) -> &str {
    value.as_str().unwrap()
}
fn text(item: &Value) -> &str {
    assert_eq!(item["type"], "message");
    assert_eq!(item["role"], "assistant");
    let [part] = array(&item["content"]) else {
        panic!("expected one content part: {item}")
    };
    assert_eq!(part["type"], "output_text");
    value_str(&part["text"])
}

fn output(response: &Value) -> &Value {
    let [item] = array(&response["output"]) else {
        panic!("expected one output item: {response}")
    };
    item
}

fn streamed<'a>(lines: impl Iterator<Item = &'a str>) -> Value {
    let events: Vec<Value> = lines
        .filter_map(|line| match SseLine::parse(line) {
            ClassifiedSseLine::Data(data) => Some(serde_json::from_str(data.as_str()).unwrap()),
            ClassifiedSseLine::Done | ClassifiedSseLine::Ignore => None,
        })
        .collect();
    let first = events.first().unwrap();
    let last = events.last().unwrap();
    assert_eq!(first["type"], "response.created");
    assert_eq!(last["type"], "response.completed");
    let response = &last["response"];
    assert_eq!(first["response"]["id"], response["id"]);
    assert_eq!(first["response"]["text"]["format"], response["text"]["format"]);
    let item = output(response);
    let (event_type, expected) = if item["type"] == "function_call" {
        ("response.function_call_arguments.delta", value_str(&item["arguments"]))
    } else {
        ("response.output_text.delta", text(item))
    };
    let mut deltas = String::new();
    let mut done = Vec::new();
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], index);
        if event["type"] == event_type {
            assert_eq!(event["item_id"], item["id"]);
            deltas.push_str(value_str(&event["delta"]));
        } else if event["type"] == "response.output_item.done" {
            done.push(&event["item"]);
        } else if event["type"] == "response.completed" {
            assert_eq!(index + 1, events.len());
        }
    }
    assert_eq!(deltas, expected);
    assert_eq!(done, [item]);
    response.clone()
}

fn recorded(turn: &Value) -> Value {
    let response = &turn["response"];
    assert_eq!(response["status_code"], 200);
    if turn["request"]["body"]["stream"] == true {
        streamed(array(&response["sse"]).iter().map(value_str))
    } else {
        response["body"].clone()
    }
}

fn generations(turns: &[Value]) -> VecDeque<gateway::Generation> {
    let mut previous = HashMap::new();
    let mut steps = VecDeque::new();
    for turn in turns.iter().filter(|t| t["request"]["method"] == "POST") {
        let body = &turn["request"]["body"];
        let response = recorded(turn);
        let item = output(&response);
        let previous_message = body["previous_response_id"]
            .as_str()
            .and_then(|id| previous.get(id))
            .cloned();
        steps.push_back(gateway::Generation {
            format: body["text"]["format"].clone(),
            previous_message,
            tool: array(&response["tools"]).first().cloned(),
            tool_output: body["input"].as_array().map(|items| items[0].clone()),
            item: item.clone(),
        });
        if item["type"] == "message" {
            previous.insert(value_str(&response["id"]).to_owned(), text(item).to_owned());
        }
    }
    steps
}

async fn replay(url: &str, turns: &[Value]) {
    let client = Client::builder().timeout(Duration::from_secs(45)).build().unwrap();
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut responses = Vec::new();
    for turn in turns {
        let req = &turn["request"];
        let expected = recorded(turn);
        let path = value_str(&req["path"])
            .split('/')
            .map(|s| ids.get(s).map_or(s, String::as_str))
            .collect::<Vec<_>>()
            .join("/");
        let mut body = req["body"].clone();
        let is_create = req["method"] == "POST";
        let request = if is_create {
            if let Some(parent) = body["previous_response_id"].as_str() {
                body["previous_response_id"] = json!(ids[parent]);
            }
            if let Some(input) = body["input"].as_array_mut() {
                for item in input {
                    item["call_id"] = json!(ids[value_str(&item["call_id"])]);
                }
            }
            client.post(format!("{url}{path}")).json(&body)
        } else {
            client.get(format!("{url}{path}")).query(&req["query_params"])
        };
        let reply = request.send().await.unwrap();
        let status = reply.status();
        let content_type = reply.headers()["content-type"].to_str().unwrap().to_owned();
        let raw = reply.text().await.unwrap();
        assert_eq!(status, 200, "{}: {raw}", turn["filename"]);
        let actual = if body["stream"] == true {
            assert!(content_type.starts_with("text/event-stream"));
            streamed(raw.lines())
        } else {
            assert!(content_type.starts_with("application/json"));
            serde_json::from_str(&raw).unwrap()
        };
        if is_create {
            assert_eq!(actual["status"], "completed");
            assert_eq!(actual["previous_response_id"], body["previous_response_id"]);
            assert_eq!(actual["text"]["format"], expected["text"]["format"]);
            let (item, captured) = (output(&actual), output(&expected));
            assert_eq!(item["type"], captured["type"]);
            assert_eq!(item["status"], "completed");
            if item["type"] == "function_call" {
                for key in ["name", "arguments"] {
                    assert_eq!(item[key], captured[key]);
                }
                ids.insert(
                    value_str(&captured["call_id"]).into(),
                    value_str(&item["call_id"]).into(),
                );
            } else {
                assert_eq!(text(item), text(captured));
            }
            let id = value_str(&actual["id"]);
            assert!(!ids.values().any(|used| used == id), "response ID reused: {id}");
            ids.insert(value_str(&expected["id"]).into(), id.into());
        }
        responses.push(actual);
    }
    reference::assert_contract(turns, &responses);
}

async fn offline(capture: &str) {
    let cassette: Value = serde_yaml::from_str(capture).unwrap();
    let turns = array(&cassette["turns"]);
    let model = value_str(&turns[0]["request"]["body"]["model"]);
    let (url, queue, server) = gateway::start(generations(turns), model).await;
    replay(&url, turns).await;
    assert!(queue.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn native_structured_output_json() {
    offline(JSON).await;
}

#[tokio::test]
async fn native_structured_output_sse() {
    offline(SSE).await;
}

#[tokio::test]
#[ignore = "requires a running native gateway with the recorded model and SQLite"]
async fn live_native_structured_output() {
    let url = std::env::var("NATIVE_STRUCTURED_OUTPUT_URL").unwrap();
    for capture in [JSON, SSE] {
        let cassette: Value = serde_yaml::from_str(capture).unwrap();
        replay(&url, array(&cassette["turns"])).await;
    }
}
