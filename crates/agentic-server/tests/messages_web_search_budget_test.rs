//! `max_uses` on a native Messages web search limits the searches the gateway
//! performs, over HTTP JSON and SSE alike.
#[allow(dead_code)]
mod common;

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentic_core::executor::ExecutionContext;
use axum::body::Body;
use axum::extract::State;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Inference and search backends behind one listener.
#[derive(Clone, Default)]
struct Backend {
    /// Inference request bodies, in arrival order.
    inferences: Arc<Mutex<Vec<Value>>>,
    /// Queries the search backend received.
    searches: Arc<Mutex<Vec<String>>>,
}

/// The scripted model: three tool rounds that ask for eight searches in total,
/// then the answer.
fn scripted_turn(round: usize) -> Vec<Value> {
    let call = |id: &str, input: Value| json!({"type":"tool_use", "id":id, "name":"web_search", "input":input});
    match round {
        0 => vec![
            call("t1", json!({"queries":["a one", "a two", "a three"]})),
            call("t2", json!({"query":"b one"})),
            call("t3", json!({"queries":["c one", "c two"]})),
        ],
        1 => vec![call("t4", json!({"query":"d one"}))],
        2 => vec![call("t5", json!({"query":"e one"}))],
        _ => vec![json!({"type":"text", "text":"Done."})],
    }
}

fn encode_sse(message: &Value) -> String {
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    let mut events = vec![json!({"type":"message_start", "message":start})];
    for (index, block) in message["content"].as_array().unwrap().iter().enumerate() {
        let mut initial = block.clone();
        let delta = if block["type"] == "tool_use" {
            initial["input"] = json!({});
            json!({"type":"input_json_delta", "partial_json":block["input"].to_string()})
        } else {
            initial["text"] = json!("");
            json!({"type":"text_delta", "text":block["text"]})
        };
        events.extend([
            json!({"type":"content_block_start", "index":index, "content_block":initial}),
            json!({"type":"content_block_delta", "index":index, "delta":delta}),
            json!({"type":"content_block_stop", "index":index}),
        ]);
    }
    events.extend([
        json!({"type":"message_delta", "delta":{"stop_reason":message["stop_reason"]}, "usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ]);
    events.iter().fold(String::new(), |mut body, event| {
        write!(body, "event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).unwrap();
        body
    })
}

async fn infer(State(backend): State<Backend>, Json(request): Json<Value>) -> Response {
    // Every completed tool round adds one assistant and one user message.
    let round = (request["messages"].as_array().unwrap().len() - 1) / 2;
    let content = scripted_turn(round);
    let stop_reason = if content[0]["type"] == "tool_use" {
        "tool_use"
    } else {
        "end_turn"
    };
    let message = json!({"id":"msg_budget", "type":"message", "role":"assistant", "model":"test-model",
        "content":content, "stop_reason":stop_reason, "usage":{"input_tokens":5, "output_tokens":3}});
    let stream = request["stream"] == true;
    backend.inferences.lock().unwrap().push(request);
    if stream {
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(encode_sse(&message)))
            .unwrap()
    } else {
        Json(message).into_response()
    }
}

async fn search(State(backend): State<Backend>, uri: Uri) -> Json<Value> {
    let query = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .find(|(key, _)| key == "query")
        .map(|(_, value)| value.into_owned())
        .expect("query");
    backend.searches.lock().unwrap().push(query);
    Json(json!({"results":{"web":[{"url":"https://example.com/proof", "title":"proof", "description":"found"}]}}))
}

/// The `is_error` flag of every tool result the gateway fed back after a round.
fn fed_back_errors(inference: &Value) -> Vec<bool> {
    inference["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_array())
        .expect("tool results fed back")
        .iter()
        .map(|result| {
            let is_error = result["is_error"].as_bool().expect("is_error");
            assert_eq!(
                is_error,
                result["content"] == "web_search max_uses exceeded; search was not run",
                "only a refused call is an error: {result}"
            );
            is_error
        })
        .collect()
}

async fn assert_max_uses_limits_searches(stream: bool) {
    let backend = Backend::default();
    let app = Router::new()
        .route("/v1/messages", post(infer))
        .route("/v1/search", get(search))
        .with_state(backend.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_url = format!("http://{}", listener.local_addr().unwrap());
    let provider = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let mut config = common::test_config(&backend_url);
    config.db_url = Some(format!("sqlite://{}", directory.path().join("state.db").display()));
    config.tools.web_search.api_key = Some("local-key".to_owned());
    config.tools.web_search.base_url = Some(backend_url);
    let context = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&context);
    let (url, gateway) = common::spawn_gateway(state).await;

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .post(format!("{url}/v1/messages"))
        .json(&json!({"model":"test-model", "max_tokens":64, "stream":stream,
            "messages":[{"role":"user", "content":"Search"}],
            "tools":[{"type":"web_search_20250305", "name":"web_search", "max_uses":2}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let body = response.text().await.unwrap();

    assert!(body.contains("Done."), "the answer reaches the client: {body}");
    assert!(!body.contains("web_search"), "gateway calls stay hidden: {body}");
    if stream {
        assert!(body.contains("event: message_stop"), "{body}");
        assert!(!body.contains("event: error"), "{body}");
    } else {
        let message: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(message["stop_reason"], "end_turn", "{body}");
        assert_eq!(message["content"], json!([{"type":"text", "text":"Done."}]));
    }

    let mut searches = backend.searches.lock().unwrap().clone();
    searches.sort();
    assert_eq!(searches, ["b one", "d one"], "max_uses=2 allows exactly two searches");
    let inferences = backend.inferences.lock().unwrap().clone();
    assert_eq!(inferences.len(), 4, "three tool rounds, then the answer");
    assert_eq!(fed_back_errors(&inferences[1]), [true, false, true]);
    assert_eq!(fed_back_errors(&inferences[2]), [false]);
    assert_eq!(fed_back_errors(&inferences[3]), [true]);

    gateway.abort();
    provider.abort();
    let _ = tokio::join!(gateway, provider);
    context.storage_pool().unwrap().close().await;
}

#[tokio::test]
async fn max_uses_limits_batched_searches_over_http_json() {
    assert_max_uses_limits_searches(false).await;
}

#[tokio::test]
async fn max_uses_limits_batched_searches_over_http_sse() {
    assert_max_uses_limits_searches(true).await;
}
