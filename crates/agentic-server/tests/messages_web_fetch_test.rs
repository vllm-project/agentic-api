//! The native `web_fetch` tool over HTTP: the gateway fetches the page the
//! user linked and hides the call on `/v1/messages` (JSON and SSE) for every
//! supported version (#408, #419), `/v1/messages/count_tokens` accepts the
//! declaration, and a disabled executor or an unsupported version is refused
//! with HTTP 400.
#[allow(dead_code)]
mod common;

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentic_core::config::{Config, WebFetchConfig};
use agentic_core::executor::ExecutionContext;
use agentic_core::types::messages::tool_seam::NATIVE_WEB_FETCH_VERSIONS;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Inference, token counting, and the page origin behind one listener.
#[derive(Clone, Default)]
struct Backend {
    /// Inference request bodies, in arrival order.
    inferences: Arc<Mutex<Vec<Value>>>,
    /// `count_tokens` request bodies, in arrival order.
    counts: Arc<Mutex<Vec<Value>>>,
    /// Page paths served, in arrival order.
    pages: Arc<Mutex<Vec<String>>>,
}

fn assistant(content: &[Value], stop_reason: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "test-model",
        "content": content, "stop_reason": stop_reason, "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 2}
    })
}

/// The scripted model: it fetches the page named in the user message, then answers.
fn scripted_turn(round: usize, request: &Value) -> Value {
    if round == 0 {
        let user = request["messages"][0]["content"].as_str().unwrap_or_default();
        let url = user
            .split_whitespace()
            .find(|word| word.starts_with("http"))
            .unwrap_or_default();
        return assistant(
            &[json!({"type": "tool_use", "id": "t1", "name": "web_fetch", "input": {"url": url}})],
            "tool_use",
        );
    }
    assistant(&[json!({"type": "text", "text": "Done."})], "end_turn")
}

fn encode_sse(message: &Value) -> String {
    let mut out = String::new();
    let mut push = |event: &str, data: &Value| {
        write!(out, "event: {event}\ndata: {data}\n\n").unwrap();
    };
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    push("message_start", &json!({"type": "message_start", "message": start}));
    for (index, block) in message["content"].as_array().unwrap().iter().enumerate() {
        let mut initial = block.clone();
        let delta = if block["type"] == "tool_use" {
            initial["input"] = json!({});
            json!({"type": "input_json_delta", "partial_json": block["input"].to_string()})
        } else {
            initial["text"] = json!("");
            json!({"type": "text_delta", "text": block["text"]})
        };
        push(
            "content_block_start",
            &json!({"type": "content_block_start", "index": index, "content_block": initial}),
        );
        push(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": index, "delta": delta}),
        );
        push(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        );
    }
    push(
        "message_delta",
        &json!({"type": "message_delta", "delta": {"stop_reason": message["stop_reason"], "stop_sequence": null},
            "usage": {"output_tokens": 2}}),
    );
    push("message_stop", &json!({"type": "message_stop"}));
    out
}

async fn infer(State(backend): State<Backend>, Json(request): Json<Value>) -> Response {
    let round = backend.inferences.lock().unwrap().len();
    backend.inferences.lock().unwrap().push(request.clone());
    let turn = scripted_turn(round, &request);
    if request["stream"] == true {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(encode_sse(&turn)))
            .unwrap()
    } else {
        Json(turn).into_response()
    }
}

async fn count_tokens(State(backend): State<Backend>, Json(request): Json<Value>) -> Response {
    backend.counts.lock().unwrap().push(request);
    Json(json!({"input_tokens": 42})).into_response()
}

async fn page(State(backend): State<Backend>, Path(name): Path<String>) -> Response {
    backend.pages.lock().unwrap().push(name.clone());
    match name.as_str() {
        "doc.html" => (
            [("content-type", "text/html")],
            "<html><head><title>Doc</title></head><body><p>The page body.</p></body></html>",
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

struct Gateway {
    backend: Backend,
    backend_url: String,
    url: String,
    _directory: tempfile::TempDir,
}

async fn spawn(web_fetch: WebFetchConfig) -> Gateway {
    let backend = Backend::default();
    let app = Router::new()
        .route("/v1/messages", post(infer))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/page/{name}", get(page))
        .with_state(backend.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let mut config: Config = common::test_config(&backend_url);
    config.db_url = Some(format!("sqlite://{}", directory.path().join("state.db").display()));
    config.tools.web_fetch = web_fetch;
    let context = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&context);
    let (url, _gateway) = common::spawn_gateway(state).await;
    Gateway {
        backend,
        backend_url,
        url,
        _directory: directory,
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

fn messages_request(user: &str, tool: &Value, stream: bool) -> Value {
    json!({"model": "test-model", "max_tokens": 64, "stream": stream,
        "messages": [{"role": "user", "content": user}], "tools": [tool]})
}

fn native_fetch() -> Value {
    json!({"type": "web_fetch_20250910", "name": "web_fetch", "max_uses": 3})
}

async fn assert_fetch_runs_over_http(tool: &Value, stream: bool) {
    let gateway = spawn(WebFetchConfig::default().with_allow_private_networks(true)).await;
    let page_url = format!("{}/page/doc.html", gateway.backend_url);

    let response = client()
        .post(format!("{}/v1/messages", gateway.url))
        .json(&messages_request(&format!("Summarize {page_url}"), tool, stream))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();

    assert!(body.contains("Done."), "the answer reaches the client: {body}");
    if stream {
        assert_eq!(body.matches("event: message_start").count(), 1, "{body}");
        assert!(body.contains("event: message_stop"), "{body}");
        assert!(!body.contains("event: error"), "{body}");
    } else {
        let message: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(message["stop_reason"], "end_turn", "{body}");
        assert_eq!(message["usage"]["server_tool_use"], json!({"web_fetch_requests": 1}));
    }
    let content = common::assembled_messages_content(&body, stream);
    assert_eq!(
        common::shown_blocks(&content),
        [
            "server_tool_use srvtoolu_gw_t1",
            "web_fetch_tool_result srvtoolu_gw_t1 page",
            "text Done."
        ],
        "{body}"
    );
    assert_eq!(content[0]["input"], json!({"url": page_url}));
    assert_eq!(content[1]["content"]["content"]["source"]["data"], "The page body.");
    assert_eq!(content[1]["content"]["content"]["title"], "Doc");
    assert_eq!(*gateway.backend.pages.lock().unwrap(), vec!["doc.html"]);
    let inferences = gateway.backend.inferences.lock().unwrap().clone();
    assert_eq!(inferences.len(), 2, "one fetch round, then the answer");
    let upstream_tool = &inferences[0]["tools"][0];
    assert!(
        upstream_tool.get("type").is_none(),
        "native type rewritten for vLLM: {upstream_tool}"
    );
    assert_eq!(upstream_tool["input_schema"]["required"], json!(["url"]));
    let fed_back = inferences[1]["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(fed_back["role"], "user");
    assert_eq!(fed_back["content"][0]["tool_use_id"], "t1");
    assert_eq!(fed_back["content"][0]["is_error"], false);
    let content: Value = serde_json::from_str(fed_back["content"][0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["type"], "web_fetch_result");
    assert_eq!(content["title"], "Doc");
    assert_eq!(content["content"], "The page body.");
}

#[tokio::test]
async fn web_fetch_runs_over_http_json() {
    assert_fetch_runs_over_http(&native_fetch(), false).await;
}

#[tokio::test]
async fn web_fetch_runs_over_http_sse() {
    assert_fetch_runs_over_http(&native_fetch(), true).await;
}

/// The later versions run exactly like the basic one when the declaration
/// permits direct calls, with the settings only they define (#419).
#[tokio::test]
async fn later_web_fetch_versions_run_over_http_json_and_sse() {
    for version in &NATIVE_WEB_FETCH_VERSIONS[1..] {
        let mut tool = native_fetch();
        tool["type"] = json!(version.type_name);
        tool["allowed_callers"] = json!(["direct"]);
        if version.use_cache {
            tool["use_cache"] = json!(false);
        }
        if version.response_inclusion {
            tool["response_inclusion"] = json!("excluded");
        }
        for stream in [false, true] {
            assert_fetch_runs_over_http(&tool, stream).await;
        }
    }
}

#[tokio::test]
async fn count_tokens_accepts_the_native_declaration() {
    let gateway = spawn(WebFetchConfig::default()).await;

    let response = client()
        .post(format!("{}/v1/messages/count_tokens", gateway.url))
        .json(
            &json!({"model": "test-model", "messages": [{"role": "user", "content": "hi"}],
            "tools": [native_fetch(), {"name": "echo", "input_schema": {"type": "object"}}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap()["input_tokens"], 42);

    let counts = gateway.backend.counts.lock().unwrap().clone();
    assert_eq!(counts.len(), 1);
    let tools = counts[0]["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "web_fetch");
    assert!(tools[0].get("type").is_none(), "{}", tools[0]);
    assert!(tools[0].get("input_schema").is_some());
    assert_eq!(tools[1]["name"], "echo", "other tools are forwarded unchanged");
}

/// Every version is refused on both endpoints when the operator disables the
/// fetcher.
#[tokio::test]
async fn a_disabled_executor_rejects_the_declaration_with_400() {
    let gateway = spawn(WebFetchConfig::default().with_enabled(false)).await;
    let page_url = format!("{}/page/doc.html", gateway.backend_url);

    for version in NATIVE_WEB_FETCH_VERSIONS {
        let mut tool = native_fetch();
        tool["type"] = json!(version.type_name);
        tool["allowed_callers"] = json!(["direct"]);
        for path in ["/v1/messages", "/v1/messages/count_tokens"] {
            let response = client()
                .post(format!("{}{path}", gateway.url))
                .json(&messages_request(&page_url, &tool, false))
                .send()
                .await
                .unwrap();
            let case = format!("{} {path}", version.type_name);
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{case}");
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["type"], "error", "{case}: {body}");
            assert!(
                body["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("web_fetch is disabled")),
                "{case}: {body}"
            );
        }
    }
    assert!(gateway.backend.inferences.lock().unwrap().is_empty());
    assert!(gateway.backend.counts.lock().unwrap().is_empty());
    assert!(gateway.backend.pages.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unsupported_versions_and_parameters_are_rejected_with_400() {
    let gateway = spawn(WebFetchConfig::default()).await;
    for (tool, expected) in [
        (
            json!({"type": "web_fetch_20991231", "name": "web_fetch"}),
            "unsupported web_fetch tool type 'web_fetch_20991231'",
        ),
        (
            json!({"type": "web_fetch_20260209", "name": "web_fetch", "allowed_callers": ["direct"], "use_cache": false}),
            "web_fetch_20260209 does not define use_cache",
        ),
        (
            json!({"type": "web_fetch_20250910", "name": "web_fetch", "citations": {"enabled": true}}),
            "web_fetch citations are not supported",
        ),
        (
            json!({"type": "web_fetch_20250910", "name": "web_fetch", "max_uses": 0}),
            "web_fetch max_uses must be a positive integer",
        ),
    ] {
        let response = client()
            .post(format!("{}/v1/messages", gateway.url))
            .json(&messages_request("hi", &tool, false))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{tool}");
        let body: Value = response.json().await.unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(expected)),
            "{tool}: {body}"
        );
    }
    assert!(
        gateway.backend.inferences.lock().unwrap().is_empty(),
        "nothing reached the upstream"
    );
}

#[tokio::test]
async fn an_allowlist_that_names_no_host_is_rejected_with_400_on_both_endpoints() {
    let gateway = spawn(WebFetchConfig::default()).await;
    let tool = json!({"type": "web_fetch_20250910", "name": "web_fetch", "allowed_domains": ["."]});
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        let response = client()
            .post(format!("{}{path}", gateway.url))
            .json(&messages_request("hi", &tool, false))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        let body: Value = response.json().await.unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("allowed_domains entry \".\" is not a host name")),
            "{path}: {body}"
        );
    }
    assert!(gateway.backend.inferences.lock().unwrap().is_empty());
    assert!(gateway.backend.counts.lock().unwrap().is_empty());
}
