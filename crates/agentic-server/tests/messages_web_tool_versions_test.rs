//! The later versions of Claude's native web search and web fetch tools over
//! HTTP (#419). With `allowed_callers` permitting direct calls they are counted
//! on `/v1/messages/count_tokens` as the same function tool as the basic
//! versions; without it `/v1/messages` (JSON and SSE) and `count_tokens` refuse
//! them with HTTP 400, naming `allowed_callers` and the reason, before anything
//! reaches the upstream, as they refuse a setting the declared version does not
//! define. Execution of every version over HTTP is covered with the basic
//! versions' tests in `messages_web_search_budget_test.rs` and
//! `messages_web_fetch_test.rs`.
#[allow(dead_code)]
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentic_core::config::Config;
use agentic_core::executor::ExecutionContext;
use agentic_core::types::messages::tool_seam::{
    NATIVE_WEB_FETCH_VERSIONS, NATIVE_WEB_SEARCH_VERSIONS, NativeToolVersion,
};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Inference and token counting behind one listener.
#[derive(Clone, Default)]
struct Backend {
    /// Inference request bodies, in arrival order.
    inferences: Arc<Mutex<Vec<Value>>>,
    /// `count_tokens` request bodies, in arrival order.
    counts: Arc<Mutex<Vec<Value>>>,
}

async fn infer(State(backend): State<Backend>, Json(request): Json<Value>) -> Response {
    backend.inferences.lock().unwrap().push(request);
    Json(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "test-model",
        "content": [{"type": "text", "text": "Done."}], "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 2}
    }))
    .into_response()
}

async fn count_tokens(State(backend): State<Backend>, Json(request): Json<Value>) -> Response {
    backend.counts.lock().unwrap().push(request);
    Json(json!({"input_tokens": 42})).into_response()
}

struct Gateway {
    backend: Backend,
    url: String,
    _directory: tempfile::TempDir,
}

async fn spawn() -> Gateway {
    let backend = Backend::default();
    let app = Router::new()
        .route("/v1/messages", post(infer))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .with_state(backend.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let mut config: Config = common::test_config(&backend_url);
    config.db_url = Some(format!("sqlite://{}", directory.path().join("state.db").display()));
    let context = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&context);
    let (url, _gateway) = common::spawn_gateway(state).await;
    Gateway {
        backend,
        url,
        _directory: directory,
    }
}

async fn send(gateway: &Gateway, path: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
        .post(format!("{}{path}", gateway.url))
        .json(body)
        .send()
        .await
        .unwrap()
}

fn messages_request(tool: &Value, stream: bool) -> Value {
    json!({"model": "test-model", "max_tokens": 64, "stream": stream,
        "messages": [{"role": "user", "content": "hi"}], "tools": [tool]})
}

/// One native declaration of `version`, named after its tool, with `max_uses`,
/// the given `allowed_callers`, and `use_cache` and `response_inclusion` where
/// the version defines them.
fn native(version: NativeToolVersion, allowed_callers: Option<Value>) -> Value {
    let name = if version.type_name.starts_with("web_search_") {
        "web_search"
    } else {
        "web_fetch"
    };
    let mut tool = json!({"type": version.type_name, "name": name, "max_uses": 3});
    if let Some(allowed_callers) = allowed_callers {
        tool["allowed_callers"] = allowed_callers;
    }
    if version.use_cache {
        tool["use_cache"] = false.into();
    }
    if version.response_inclusion {
        tool["response_inclusion"] = "excluded".into();
    }
    tool
}

fn every_version() -> impl Iterator<Item = NativeToolVersion> {
    NATIVE_WEB_SEARCH_VERSIONS
        .iter()
        .chain(NATIVE_WEB_FETCH_VERSIONS)
        .copied()
}

fn later_versions() -> impl Iterator<Item = NativeToolVersion> {
    NATIVE_WEB_SEARCH_VERSIONS[1..]
        .iter()
        .chain(&NATIVE_WEB_FETCH_VERSIONS[1..])
        .copied()
}

#[tokio::test]
async fn count_tokens_counts_every_version_as_its_basic_function_tool() {
    let gateway = spawn().await;
    for version in every_version() {
        let tool_type = version.type_name;
        let body = json!({"model": "test-model", "messages": [{"role": "user", "content": "hi"}],
            "tools": [native(version, Some(json!(["direct"]))), {"name": "echo", "input_schema": {"type": "object"}}]});
        let response = send(&gateway, "/v1/messages/count_tokens", &body).await;
        assert_eq!(response.status(), StatusCode::OK, "{tool_type}");
        assert_eq!(
            response.json::<Value>().await.unwrap()["input_tokens"],
            42,
            "{tool_type}"
        );
    }

    let counts = gateway.backend.counts.lock().unwrap().clone();
    let counted: Vec<Value> = counts.iter().map(|count| count["tools"][0].clone()).collect();
    assert_eq!(
        counted.len(),
        NATIVE_WEB_SEARCH_VERSIONS.len() + NATIVE_WEB_FETCH_VERSIONS.len()
    );
    for (version, (count, tool)) in every_version().zip(counts.iter().zip(&counted)) {
        let tool_type = version.type_name;
        assert!(
            tool.get("input_schema").is_some()
                && ["type", "allowed_callers", "max_uses", "use_cache", "response_inclusion"]
                    .iter()
                    .all(|field| tool.get(field).is_none()),
            "{tool_type} is counted as an ordinary function tool: {tool}"
        );
        assert_eq!(count["tools"][1]["name"], "echo", "{tool_type}: other tools unchanged");
    }
    let (search, fetch) = counted.split_at(NATIVE_WEB_SEARCH_VERSIONS.len());
    for (tool, tools) in [("web_search", search), ("web_fetch", fetch)] {
        assert!(
            tools.windows(2).all(|pair| pair[0] == pair[1]),
            "every {tool} version is counted as the same function tool: {tools:?}"
        );
    }
    assert!(gateway.backend.inferences.lock().unwrap().is_empty());
}

#[tokio::test]
async fn later_versions_without_direct_calls_are_refused_with_400_on_both_endpoints() {
    let gateway = spawn().await;
    for version in later_versions() {
        let tool_type = version.type_name;
        let expected = format!(
            "invalid request: {tool_type} allowed_callers must include \"direct\": dynamic filtering calls the tool \
             from code execution, which this gateway does not run"
        );
        for allowed_callers in [None, Some(json!(["code_execution_20260120"]))] {
            let tool = native(version, allowed_callers.clone());
            for (path, stream) in [
                ("/v1/messages", false),
                ("/v1/messages", true),
                ("/v1/messages/count_tokens", false),
            ] {
                let response = send(&gateway, path, &messages_request(&tool, stream)).await;
                let case = format!("{tool_type} {path} stream={stream} allowed_callers={allowed_callers:?}");
                assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{case}");
                let body: Value = response.json().await.unwrap();
                assert_eq!(body["type"], "error", "{case}: {body}");
                assert_eq!(body["error"]["type"], "invalid_request_error", "{case}: {body}");
                assert_eq!(body["error"]["message"], expected, "{case}: {body}");
            }
        }
    }
    assert!(
        gateway.backend.inferences.lock().unwrap().is_empty(),
        "nothing reached the upstream"
    );
    assert!(gateway.backend.counts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unsupported_web_search_version_is_refused_with_the_supported_ones() {
    let gateway = spawn().await;
    let tool = json!({"type": "web_search_20991231", "name": "web_search", "allowed_callers": ["direct"]});
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        let response = send(&gateway, path, &messages_request(&tool, false)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["error"]["message"],
            "invalid request: unsupported web_search tool type 'web_search_20991231'; supported versions are \
             web_search_20250305, web_search_20260209, web_search_20260318",
            "{path}: {body}"
        );
    }
    assert!(gateway.backend.inferences.lock().unwrap().is_empty());
    assert!(gateway.backend.counts.lock().unwrap().is_empty());
}

/// `use_cache` and `response_inclusion` are refused on a version that does not
/// define them, and a basic version whose `allowed_callers` omits `"direct"` is
/// refused in the message shape every version uses, on both endpoints.
#[tokio::test]
async fn settings_a_version_does_not_define_are_refused_with_400() {
    let gateway = spawn().await;
    for (tool, expected) in [
        (
            json!({"type": "web_search_20260318", "name": "web_search", "allowed_callers": ["direct"], "use_cache": false}),
            "invalid request: web_search_20260318 does not define use_cache",
        ),
        (
            json!({"type": "web_search_20260209", "name": "web_search", "allowed_callers": ["direct"],
                "response_inclusion": "full"}),
            "invalid request: web_search_20260209 does not define response_inclusion",
        ),
        (
            json!({"type": "web_fetch_20260309", "name": "web_fetch", "allowed_callers": ["direct"],
                "response_inclusion": "full"}),
            "invalid request: web_fetch_20260309 does not define response_inclusion",
        ),
        (
            json!({"type": "web_search_20250305", "name": "web_search", "allowed_callers": ["code_execution_20260120"]}),
            "invalid request: web_search_20250305 allowed_callers must include \"direct\": this gateway calls the tool \
             only directly, never from code execution",
        ),
    ] {
        for path in ["/v1/messages", "/v1/messages/count_tokens"] {
            let response = send(&gateway, path, &messages_request(&tool, false)).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path} {tool}");
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["error"]["message"], expected, "{path} {tool}: {body}");
        }
    }
    assert!(gateway.backend.inferences.lock().unwrap().is_empty());
    assert!(gateway.backend.counts.lock().unwrap().is_empty());
}
