pub mod otlp_stub;

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;
use http::StatusCode;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use agentic_core::config::Config;
use agentic_core::executor::{ConversationHandler, ExecutionContext, ResponseHandler};
use agentic_core::proxy::ProxyState;
use agentic_core::readiness::llm_readiness_client;
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_server::app::{
    AppState, DEFAULT_MAX_REQUEST_BODY_SIZE, ReadinessTracker, ServerConfig, WebSocketTracker, build_router,
};

pub fn test_config(llm_url: &str) -> Config {
    Config {
        llm_api_base: llm_url.to_owned(),
        openai_api_key: Some("test-key".to_owned()),
        llm_ready_timeout_s: 5.0,
        llm_ready_interval_s: 0.1,
        skip_llm_ready_check: false,
        db_url: None,
        postgres: agentic_core::config::PostgresConfig::default(),
        sqlite: agentic_core::config::SqliteConfig::default(),
        tools: agentic_core::config::ToolRuntimeConfig::default(),
        responses: agentic_core::config::ResponsesConfig::default(),
    }
}

// Used by other test modules, not directly by conversations_test
#[allow(dead_code)]
pub fn test_state(config: &Config) -> AppState {
    test_state_with_max_request_body_size(config, DEFAULT_MAX_REQUEST_BODY_SIZE)
}

// Used by other test modules, not directly by conversations_test
#[allow(dead_code)]
pub fn test_state_with_max_request_body_size(config: &Config, max_request_body_size: NonZeroUsize) -> AppState {
    let exec_ctx = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        config.llm_api_base.clone(),
    )
    .with_responses_config(config.responses);
    let exec_ctx = Arc::new(exec_ctx);
    let proxy_state = ProxyState::new(config.clone()).expect("proxy state");
    AppState {
        proxy_state,
        exec_ctx,
        llm_readiness_client: llm_readiness_client().expect("readiness client"),
        readiness_tracker: ReadinessTracker::default(),
        shutdown_token: CancellationToken::new(),
        websocket_tracker: WebSocketTracker::default(),
        llm_api_base: config.llm_api_base.clone(),
        skip_llm_ready_check: config.skip_llm_ready_check,
        openai_api_key: config.openai_api_key.clone(),
        model_capabilities: std::sync::Arc::default(),
        max_request_body_size,
    }
}

/// Spawn a minimal mock LLM that responds to `GET /health` with 200.
pub async fn spawn_mock_llm() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route("/health", get(|| async { StatusCode::OK.into_response() }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), handle)
}

/// Spawn the gateway router bound to a random port.
#[allow(dead_code)]
pub async fn spawn_gateway(state: AppState) -> (String, tokio::task::JoinHandle<()>) {
    let router = build_router(state, &ServerConfig::from_env());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), handle)
}

/// The recorder capture for a client request that declares a built-in, a function, and a
/// custom tool. The capture records the gateway's response before declaration restoration,
/// which is the behavior the replay tests assert against.
#[allow(dead_code)]
pub fn load_mixed_gateway_capture(streaming: bool) -> Value {
    let mode = if streaming { "streaming" } else { "nonstreaming" };
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../agentic-server-core/tests/cassettes/parallel_tool_calls/\
         parallel-mixed-gateway-Qwen-Qwen3.5-35B-A3B-FP8-{mode}.yaml"
    ));
    serde_yml::from_str(&std::fs::read_to_string(&path).expect("mixed capture should be readable"))
        .expect("mixed capture should be YAML")
}

/// The first client turn the capture recorded, replayed unchanged.
#[allow(dead_code)]
pub fn mixed_capture_client_request(capture: &Value) -> Value {
    capture["turns"][0]["request"]["body"].clone()
}

/// The declarations the client submitted, as the capture records them in that turn.
#[allow(dead_code)]
pub fn mixed_capture_public_tools(capture: &Value) -> Value {
    capture["turns"][0]["request"]["body"]["tools"].clone()
}

/// The gateway response the capture recorded, served as synthetic upstream inference: a JSON
/// body for a blocking request, an SSE stream for a streaming one.
#[allow(dead_code)]
pub fn mixed_capture_upstream_fixture(capture: &Value) -> String {
    let response = &capture["turns"][0]["response"];
    if let Some(body) = response["body"].as_object() {
        return serde_json::to_string(body).expect("recorded body serializes");
    }
    let frames = response["sse"].as_array().expect("recorded SSE frames");
    let mut body = frames
        .iter()
        .map(|frame| frame.as_str().expect("SSE frame is text").to_owned())
        .collect::<String>();
    if !body.contains("data: [DONE]") {
        body.push_str("data: [DONE]\n\n");
    }
    body
}

/// Asserts one client-facing `response` object reports the submitted declarations, not the
/// lowered `web_search` function schema the model server echoed. Explicit nulls a
/// declaration carries on the wire are not part of the comparison.
#[allow(dead_code)]
pub fn assert_public_mixed_tools(response: &Value, public_tools: &Value, context: &str) {
    assert_eq!(
        strip_nulls(&response["tools"]),
        strip_nulls(public_tools),
        "{context} tools"
    );
    assert!(
        !response["tools"].to_string().contains("\"name\":\"web_search\""),
        "the internal web_search function schema must not reach the client: {context}"
    );
}

#[allow(dead_code)]
fn strip_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), strip_nulls(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip_nulls).collect()),
        other => other.clone(),
    }
}

/// Asserts the upstream still receives one lowered function schema per declaration.
#[allow(dead_code)]
pub fn assert_lowered_mixed_upstream(body: &Value) {
    let tools = body["tools"].as_array().expect("upstream tools");
    let names: Vec<&str> = tools.iter().filter_map(|tool| tool["name"].as_str()).collect();
    assert_eq!(names, ["web_search", "get_weather", "set_temperature_unit"]);
    assert!(
        tools.iter().all(|tool| tool["type"] == "function"),
        "every declaration reaches inference as a function schema"
    );
}

/// Asserts the replayed response carries the same public output items the capture recorded,
/// so restoring the declarations leaves tool execution and output calls unchanged.
#[allow(dead_code)]
pub fn assert_recorded_mixed_output_kinds(response: &Value, capture: &Value, context: &str) {
    assert_eq!(
        output_kinds(&response["output"]),
        output_kinds(&recorded_terminal_output(capture)),
        "{context} output item kinds"
    );
}

#[allow(dead_code)]
fn output_kinds(output: &Value) -> Vec<String> {
    output
        .as_array()
        .expect("response output is an array")
        .iter()
        .filter_map(|item| item["type"].as_str())
        .filter(|kind| *kind != "reasoning")
        .map(str::to_owned)
        .collect()
}

/// The terminal gateway response the capture recorded, from its body or its last SSE event.
#[allow(dead_code)]
fn recorded_terminal_output(capture: &Value) -> Value {
    let response = &capture["turns"][0]["response"];
    if let Some(output) = response["body"].get("output") {
        return output.clone();
    }
    let frames = response["sse"].as_array().expect("recorded SSE frames");
    frames
        .iter()
        .filter_map(|frame| frame.as_str())
        .flat_map(|frame| frame.lines())
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| event["type"] == "response.completed")
        .map(|event| event["response"]["output"].clone())
        .expect("recorded capture completes with output")
}
