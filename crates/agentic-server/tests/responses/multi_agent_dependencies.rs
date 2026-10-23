//! Actual model, You search, and MCP exchanges recorded by the existing dependency proxy.
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::read_to_string;
use std::sync::Arc;

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

#[derive(Deserialize)]
struct Request {
    method: String,
    path: String,
    body: Value,
    query_params: BTreeMap<String, String>,
}

#[derive(Deserialize)]
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
        let state = Arc::new(Mutex::new(Replay {
            turns: capture
                .turns
                .into_iter()
                .filter(|turn| turn.request.path != "/health")
                // Unsupported MCP polling/teardown is timing-dependent, unlike tool RPCs.
                .filter(|turn| !(turn.request.method != "POST" && turn.response.status_code == 405))
                .map(Some)
                .collect(),
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
    }
    normalize(&mut expected);
    normalize(&mut actual);
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
