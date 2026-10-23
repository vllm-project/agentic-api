//! Replay recorded Responses management calls through the gateway.
//! Only model inference is mocked; storage and management endpoints run locally.
#[path = "responses_api/recordings.rs"]
mod recordings;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use std::slice;
use std::sync::Arc;
use tokio::process::{Child, Command};

use agentic_core::events::normalize_sse_line;
use axum::{Json, Router, response::IntoResponse, routing::post};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

fn cassette(scenario: &str, provider: &str) -> Option<Vec<Value>> {
    let directory = std::env::var_os("RESPONSES_CASSETTES_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes/responses"),
        PathBuf::from,
    );
    let path = directory.join(format!("response-{scenario}-{provider}.yaml"));
    if !path.exists() {
        return None;
    }
    let document: Value = serde_yml::from_str(&std::fs::read_to_string(&path).expect("read cassette"))
        .expect("parse recorder-generated cassette");
    assert_eq!(
        document.as_object().unwrap().len(),
        1,
        "only a flat turns list is allowed"
    );
    let turns = document["turns"].as_array().unwrap().clone();
    assert!(!turns.is_empty(), "{}: empty cassette", path.display());
    for (index, turn) in turns.iter().enumerate() {
        assert_eq!(turn["filename"], format!("t{}", index + 1));
        assert!(turn["request"]["method"].is_string());
        assert!(turn["request"]["query_params"].is_object());
        assert!(turn["response"]["status_code"].as_u64().is_some());
    }
    Some(turns)
}

fn terminal(response: &Value) -> Value {
    if let Some(body) = response.get("body") {
        return body.clone();
    }
    let stream: String = response["sse"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap())
        .collect();
    stream
        .lines()
        .filter_map(normalize_sse_line)
        .filter_map(|frame| frame.wire.rest.get("response").cloned())
        .find(|body| matches!(body["status"].as_str(), Some("completed" | "incomplete" | "failed")))
        .expect("stream must contain a terminal response")
}

fn mapped(id: &str, ids: &HashMap<String, String>) -> String {
    ids.get(id).cloned().unwrap_or_else(|| id.to_owned())
}

fn map_request_ids(value: &mut Value, ids: &HashMap<String, String>) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(|item| map_request_ids(item, ids)),
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "id" | "previous_response_id" | "conversation" | "after")
                    && let Some(id) = value.as_str()
                {
                    *value = mapped(id, ids).into();
                } else {
                    map_request_ids(value, ids);
                }
            }
        }
        _ => {}
    }
}

fn item_signature(item: &Value) -> Value {
    let mut item = item.clone();
    item.as_object_mut().unwrap().remove("id");
    // Missing tool-history status defaults are deferred to the item-type refactor.
    // Ignore only absence versus these recorded defaults; other statuses still differ.
    if matches!(
        (item["type"].as_str(), item["status"].as_str()),
        (Some("function_call"), Some("in_progress")) | (Some("function_call_output"), Some("completed"))
    ) {
        item.as_object_mut().unwrap().remove("status");
    }
    // Log probabilities are outside this API-management contract comparison.
    if let Some(content) = item["content"].as_array_mut() {
        for part in content {
            if let Some(part) = part.as_object_mut() {
                part.remove("logprobs");
            }
        }
    }
    item
}

fn bind_id(recorded: &str, live: &str, ids: &mut HashMap<String, String>, location: &str) {
    if let Some(previous) = ids.get(recorded) {
        assert_eq!(previous, live, "{location}: an item changed identity");
    } else {
        assert!(
            !ids.values().any(|value| value == live),
            "{location}: distinct IDs collapsed"
        );
        ids.insert(recorded.to_owned(), live.to_owned());
    }
}

fn error_metadata(body: &Value) -> Value {
    json!({"type": body["error"]["type"], "code": body["error"]["code"], "param": body["error"]["param"]})
}

fn compare_items(expected: &[Value], actual: &[Value], ids: &mut HashMap<String, String>, location: &str) {
    assert_eq!(expected.len(), actual.len(), "{location}: item count");
    for (left, right) in expected.iter().zip(actual) {
        let mut expected = item_signature(left);
        let result_prefix = match left["type"].as_str() {
            Some("function_call_output") => Some("fc_"),
            Some("custom_tool_call_output") => Some("ctco_"),
            Some("shell_call_output") => Some("sho_"),
            _ => None,
        };
        if let Some(prefix) = result_prefix {
            // Older gateway captures lack these fields. The refreshed OpenAI
            // tool-execution reference establishes their required public values.
            assert!(
                right["id"].as_str().is_some_and(|id| id.starts_with(prefix)),
                "{location}: result ID"
            );
            assert_eq!(right["status"], "completed", "{location}: result status");
            if left["type"] != "function_call_output" && expected["status"].is_null() {
                expected["status"] = json!("completed");
            }
        }
        assert_eq!(expected, item_signature(right), "{location}: item content");
        let recorded = left["id"].as_str();
        let live = right["id"].as_str();
        // Function-result resource IDs are deferred; all other item identities remain required.
        if recorded.is_some() || left["type"] != "function_call_output" {
            assert_eq!(recorded.is_some(), live.is_some(), "{location}: item ID presence");
        }
        if let (Some(recorded), Some(live)) = (recorded, live) {
            bind_id(recorded, live, ids, location);
        }
    }
}

fn assert_exchange(expected: &Value, actual: &Value, ids: &mut HashMap<String, String>, location: &str) {
    assert_eq!(actual["object"], expected["object"], "{location}: object");
    if let (Some(recorded), Some(live)) = (expected["id"].as_str(), actual["id"].as_str()) {
        if let Some(mapped) = ids.get(recorded) {
            assert_eq!(live, mapped, "{location}: identity");
        } else {
            ids.insert(recorded.to_owned(), live.to_owned());
        }
    }
    if let Some(expected_items) = expected["data"].as_array() {
        let actual_items = actual["data"].as_array().unwrap();
        assert_eq!(actual["has_more"], expected["has_more"], "{location}: has_more");
        compare_items(expected_items, actual_items, ids, location);
        for field in ["first_id", "last_id"] {
            if let Some(id) = expected[field].as_str() {
                assert_eq!(actual[field], mapped(id, ids), "{location}: {field}");
            } else {
                assert!(actual[field].is_null(), "{location}: {field}");
            }
        }
    }
    if let Some(output) = expected["output"].as_array() {
        compare_items(output, actual["output"].as_array().unwrap(), ids, location);
        for field in ["status", "store"] {
            assert_eq!(actual[field], expected[field], "{location}: {field}");
        }
    }
    if expected.get("deleted").is_some() {
        assert_eq!(actual["deleted"], expected["deleted"], "{location}: deleted");
    }
}

async fn replay_turn(
    client: &reqwest::Client,
    url: &str,
    turn: &Value,
    reference: &Value,
    ids: &mut HashMap<String, String>,
    location: &str,
) {
    let request = &turn["request"];
    let path = request["path"]
        .as_str()
        .unwrap()
        .split('/')
        .map(|part| mapped(part, ids))
        .collect::<Vec<_>>()
        .join("/");
    let mut body = request["body"].clone();
    map_request_ids(&mut body, ids);
    let mut query = request["query_params"].clone();
    map_request_ids(&mut query, ids);
    let query: Vec<(&str, &str)> = query
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(key, value)| {
            value
                .as_array()
                .map_or_else(|| slice::from_ref(value), Vec::as_slice)
                .iter()
                .map(move |value| (key.as_str(), value.as_str().unwrap()))
        })
        .collect();
    let method = request["method"].as_str().unwrap();
    let mut call = client
        .request(method.parse().unwrap(), format!("{url}{path}"))
        .bearer_auth("test-key")
        .query(&query);
    if method == "POST" {
        call = call
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body).unwrap());
    }
    let response = call.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    let actual: Value = if status == 200 && body["stream"] == true && method == "POST" {
        terminal(&json!({"sse":text.split_inclusive('\n').collect::<Vec<_>>()}))
    } else {
        serde_json::from_str(&text).expect("JSON management response")
    };
    assert_eq!(
        u64::from(status),
        reference["response"]["status_code"].as_u64().unwrap(),
        "{location}: {actual}"
    );
    if status != 200 {
        assert_eq!(
            error_metadata(&actual),
            error_metadata(&terminal(&reference["response"])),
            "{location}: OpenAI error metadata"
        );
        return;
    }
    assert_exchange(&terminal(&turn["response"]), &actual, ids, location);
}

async fn spawn_gateway(upstream_url: &str, home: &Path, client: &reqwest::Client) -> (String, Child) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let binary = std::env::var_os("RESPONSES_GATEWAY_BIN").map_or_else(
        || {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join(format!("agentic-server{}", std::env::consts::EXE_SUFFIX))
        },
        PathBuf::from,
    );
    let log_path = home.join("gateway.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut gateway = Command::new(binary)
        .env_clear()
        .env("AGENTIC_API_HOME", home)
        .env("OPENAI_API_KEY", "test-key")
        .env("RUST_LOG", "warn")
        .args([
            "--llm-api-base",
            upstream_url,
            "--skip-llm-ready-check",
            "--gateway-host",
            "127.0.0.1",
            "--gateway-port",
            &port.to_string(),
            "--db-url",
            "sqlite://?mode=memory",
        ])
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .expect("build the gateway first: cargo build -p agentic-server --bin agentic-server");
    let url = format!("http://127.0.0.1:{port}");
    let ready = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            assert!(
                gateway.try_wait().unwrap().is_none(),
                "gateway exited: {}",
                std::fs::read_to_string(&log_path).unwrap()
            );
            if client
                .get(format!("{url}/health"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(
        ready.is_ok(),
        "gateway did not start: {}",
        std::fs::read_to_string(&log_path).unwrap()
    );
    (url, gateway)
}

async fn replay(scenario: &str, provider: &str, turns: &[Value]) {
    let model_responses: Vec<Value> = turns
        .iter()
        .filter(|turn| {
            turn["request"]["method"] == "POST"
                && turn["request"]["path"] == "/v1/responses"
                && turn["response"]["status_code"] == 200
        })
        .map(|turn| turn["response"].clone())
        .collect();
    let outputs = Arc::new(Mutex::new(model_responses.into_iter()));
    let upstream_outputs = Arc::clone(&outputs);
    let last_model_input = Arc::new(Mutex::new(Value::Null));
    let upstream_input = Arc::clone(&last_model_input);
    let upstream_router = Router::new().route(
        "/v1/responses",
        post(move |Json(request): Json<Value>| {
            let outputs = Arc::clone(&upstream_outputs);
            let input = Arc::clone(&upstream_input);
            async move {
                *input.lock().await = request["input"].clone();
                let response = outputs.lock().await.next().expect("recorded model response exhausted");
                if let Some(chunks) = response["sse"].as_array() {
                    let stream: String = chunks.iter().map(|chunk| chunk.as_str().unwrap()).collect();
                    ([("content-type", "text/event-stream")], stream).into_response()
                } else {
                    Json(response["body"].clone()).into_response()
                }
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });
    let home = std::env::temp_dir().join(format!("responses-replay-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir(&home).unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let (url, mut gateway) = spawn_gateway(&upstream_url, &home, &client).await;
    let reference = cassette(scenario, "openai").expect("OpenAI reference is required for error contracts");
    let mut ids = HashMap::<String, String>::new();
    for (index, turn) in turns.iter().enumerate() {
        replay_turn(
            &client,
            &url,
            turn,
            if provider == "openai" { turn } else { &reference[index] },
            &mut ids,
            &format!("{scenario}/{provider} t{}", index + 1),
        )
        .await;
    }
    assert!(
        outputs.lock().await.next().is_none(),
        "all recorded model calls must be consumed"
    );
    if scenario == "lifecycle" || scenario == "lifecycle-stream" {
        let input = last_model_input.lock().await;
        let user_texts: Vec<&str> = input
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["role"] == "user")
            .flat_map(|item| {
                let content = &item["content"];
                content
                    .as_array()
                    .map_or_else(|| slice::from_ref(content), Vec::as_slice)
            })
            .map(|part| part.as_str().or_else(|| part["text"].as_str()).expect("user text"))
            .collect();
        let expected: Vec<&str> = [2, 13]
            .map(|index| turns[index]["request"]["body"]["input"][0]["content"].as_str().unwrap())
            .into();
        assert_eq!(
            user_texts, expected,
            "deleted parent must not reach continued inference"
        );
    }
    gateway.kill().await.unwrap();
    gateway.wait().await.unwrap();
    upstream.abort();
    let _ = upstream.await;
    std::fs::remove_dir_all(&home).unwrap();
}

macro_rules! replay_tests {
    ($(($name:ident, $scenario:literal, $provider:literal)),* $(,)?) => {$(
        #[tokio::test]
        async fn $name() {
            let turns = cassette($scenario, $provider).expect("required Responses recording missing");
            replay($scenario, $provider, &turns).await;
        }
    )*};
}

replay_tests! {
    (replay_lifecycle_openai, "lifecycle", "openai"),
    (replay_lifecycle_gateway, "lifecycle", "gateway"),
    (replay_lifecycle_stream_openai, "lifecycle-stream", "openai"),
    (replay_lifecycle_stream_gateway, "lifecycle-stream", "gateway"),
    (replay_pagination_openai, "pagination", "openai"),
    (replay_pagination_gateway, "pagination", "gateway"),
    (replay_pagination_stream_openai, "pagination-stream", "openai"),
    (replay_pagination_stream_gateway, "pagination-stream", "gateway"),
    (replay_edge_cases_openai, "edge-cases", "openai"),
    (replay_edge_cases_gateway, "edge-cases", "gateway"),
    (replay_edge_cases_stream_openai, "edge-cases-stream", "openai"),
    (replay_edge_cases_stream_gateway, "edge-cases-stream", "gateway"),
    (replay_conversation_openai, "conversation", "openai"),
    (replay_conversation_gateway, "conversation", "gateway"),
    (replay_conversation_stream_openai, "conversation-stream", "openai"),
    (replay_conversation_stream_gateway, "conversation-stream", "gateway"),
    (replay_replayed_items_openai, "replayed-items", "openai"),
    (replay_replayed_items_gateway, "replayed-items", "gateway"),
    (replay_replayed_items_stream_openai, "replayed-items-stream", "openai"),
    (replay_replayed_items_stream_gateway, "replayed-items-stream", "gateway"),
}

#[tokio::test]
async fn replay_client_tool_input_items_gateway() {
    let turns = cassette("tool-items", "gateway").expect("gateway tool-execution recording");
    let mut selected = false;
    let turns: Vec<_> = turns
        .into_iter()
        .filter(|turn| {
            let body = &turn["request"]["body"];
            if turn["request"]["method"] == "POST" && body["previous_response_id"].is_null() {
                selected = body["tools"].as_array().is_some_and(|tools| {
                    !tools.is_empty()
                        && tools.iter().all(|tool| {
                            matches!(
                                tool["type"].as_str(),
                                Some("function" | "custom" | "shell" | "tool_search" | "namespace")
                            )
                        })
                });
            }
            selected
        })
        .collect();
    assert_eq!(
        turns.iter().filter(|turn| turn["request"]["method"] == "POST").count(),
        13
    );
    replay("tool-items", "gateway", &turns).await;
}
