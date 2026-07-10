//! Cassette-driven integration tests for the reworked gateway tool loop
//! (`run_until_gateway_tools_complete` via `ExecuteRequest::run`).
//!
//! These drive real recorded `OpenAI` Responses wire bodies through the loop to
//! confirm the `LoopDecision` classification behaves end-to-end:
//!   * client-owned (function) tool calls terminate the loop in one model call
//!     and are handed back to the caller (`RequiresClientAction`);
//!   * parallel function calls in a single turn are preserved on output;
//!   * a gateway-owned call drives another round and comes back `completed`.
//!
//! The synthetic-payload edge cases (usage accumulation, fanout cap, error
//! feedback, the round-cap `incomplete` path) live in `web_search_tool_test.rs`;
//! this file focuses on coverage against recorded `OpenAI` wire bodies.

use std::sync::Arc;

use agentic_core::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::WebSearchHandler;
use agentic_core::types::io::{OutputItem, ResponsesInput};
use agentic_core::types::request_response::RequestPayload;
use agentic_core::types::tools::ResponsesTool;
use either::Either;

mod support;

const MULTI_TURN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/tool_calls/multi_turn");

/// Load turn N (0-based) response body from a multi-turn cassette as a mock
/// JSON response for the LLM `MockServer`.
///
/// Uses a permissive local parser (input typed as `Value`) rather than the
/// shared strict `Cassette` — these `OpenAI` cassettes carry array-valued `input`
/// on later turns, which the shared string-typed loader rejects. We only need
/// the response body, so the request shape is irrelevant here.
fn cassette_turn(filename: &str, turn: usize) -> support::MockResponse {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Doc {
        turns: Vec<DocTurn>,
    }
    #[derive(Deserialize)]
    struct DocTurn {
        response: DocResponse,
    }
    #[derive(Deserialize)]
    struct DocResponse {
        #[serde(default)]
        body: Option<serde_json::Value>,
        #[serde(default)]
        sse: Option<Vec<String>>,
    }

    let path = format!("{MULTI_TURN_DIR}/{filename}");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let doc: Doc = serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"));
    let resp = doc
        .turns
        .into_iter()
        .nth(turn)
        .unwrap_or_else(|| panic!("cassette {filename} missing turn {turn}"))
        .response;

    if let Some(body) = resp.body {
        support::MockResponse::Json(serde_json::to_string(&body).expect("cassette body serializes"))
    } else if let Some(sse) = resp.sse {
        support::MockResponse::Sse(sse.join(""))
    } else {
        panic!("cassette {filename} turn {turn} has neither body nor sse");
    }
}

async fn build_exec_ctx(llm_url: &str) -> Arc<ExecutionContext> {
    let pool = support::setup_pool().await;
    let conv_handler = ConversationHandler::new(ConversationStore::new(Arc::clone(&pool)));
    let resp_handler = ResponseHandler::new(ResponseStore::new(pool));
    let client = Arc::new(reqwest::Client::new());
    Arc::new(ExecutionContext::new(
        conv_handler,
        resp_handler,
        client,
        llm_url.to_owned(),
    ))
}

/// Same as `build_exec_ctx` but registers a `WebSearchHandler` gateway executor
/// backed by the given You.com mock URL.
async fn build_exec_ctx_with_web_search(llm_url: &str, you_url: &str) -> Arc<ExecutionContext> {
    let pool = support::setup_pool().await;
    let conv_handler = ConversationHandler::new(ConversationStore::new(Arc::clone(&pool)));
    let resp_handler = ResponseHandler::new(ResponseStore::new(pool));
    let client = Arc::new(reqwest::Client::new());
    Arc::new(
        ExecutionContext::new(conv_handler, resp_handler, Arc::clone(&client), llm_url.to_owned())
            .with_gateway_executor(Arc::new(WebSearchHandler::with_api_key(
                client,
                "secret-you-key".to_owned(),
                you_url,
            ))),
    )
}

fn request(text: &str, tools: Option<Vec<ResponsesTool>>) -> RequestPayload {
    RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text(text.to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools,
        tool_choice: None,
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
    }
}

fn function_call_names(output: &[OutputItem]) -> Vec<&str> {
    output
        .iter()
        .filter_map(|item| match item {
            OutputItem::FunctionCall(fc) => Some(fc.name.as_str()),
            _ => None,
        })
        .collect()
}

/// `OpenAI` cassette, turn 1 emits a single client-owned `get_job_status`
/// function call. With no gateway executor registered, the loop must classify
/// this as `RequiresClientAction`: exactly one model call, the call handed back
/// on `output`, `status: "completed"`.
#[tokio::test]
async fn openai_single_client_owned_call_terminates_in_one_round() {
    let llm = support::MockServer::start_deque(vec![cassette_turn("openai_responses_tool_calls_3turn.yaml", 0)]).await;
    let exec_ctx = build_exec_ctx(llm.url()).await;

    let result = ExecuteRequest::new(request("check job status", None), exec_ctx)
        .run()
        .await
        .expect("execute should succeed");
    let Either::Left(response) = result else {
        panic!("non-streaming request should return a payload");
    };

    // Client-owned function call: no gateway execution, exactly one LLM call.
    assert_eq!(
        llm.request_bodies().await.len(),
        1,
        "client-owned tools take one model call"
    );
    assert_eq!(response.status, "completed");
    assert_eq!(
        function_call_names(&response.output),
        vec!["get_job_status"],
        "the client-owned call must be handed back on output"
    );
}

/// `OpenAI` cassette, turn 1 emits two parallel client-owned function calls
/// (`get_job_status` + `web_search`). With no gateway executor, both are handed
/// back unexecuted in a single round.
#[tokio::test]
async fn openai_parallel_client_owned_calls_preserved_on_output() {
    let llm =
        support::MockServer::start_deque(vec![cassette_turn("openai_responses_tool_calls_parallel.yaml", 0)]).await;
    let exec_ctx = build_exec_ctx(llm.url()).await;

    let result = ExecuteRequest::new(request("check status and search", None), exec_ctx)
        .run()
        .await
        .expect("execute should succeed");
    let Either::Left(response) = result else {
        panic!("non-streaming request should return a payload");
    };

    assert_eq!(llm.request_bodies().await.len(), 1);
    let names = function_call_names(&response.output);
    assert_eq!(names.len(), 2, "both parallel calls preserved, got {names:?}");
    assert!(
        names.contains(&"get_job_status"),
        "expected get_job_status in {names:?}"
    );
    assert!(names.contains(&"web_search"), "expected web_search in {names:?}");
}

/// When `web_search` is declared as a gateway tool AND a `WebSearchHandler` is
/// registered, the same first-turn call is gateway-owned: the loop executes it,
/// feeds the output back, and the recorded second turn returns text. The
/// `get_job_status` call in the parallel cassette stays client-owned, so the
/// turn is `RequiresClientAction` and terminates after one model call — the
/// gateway `web_search` output is still recorded alongside it.
#[tokio::test]
async fn openai_mixed_gateway_and_client_owned_hands_back_after_gateway_exec() {
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you().await;
    let llm =
        support::MockServer::start_deque(vec![cassette_turn("openai_responses_tool_calls_parallel.yaml", 0)]).await;
    let exec_ctx = build_exec_ctx_with_web_search(llm.url(), &you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();

    let result = ExecuteRequest::new(request("status and search", Some(vec![web_search])), exec_ctx)
        .run()
        .await
        .expect("execute should succeed");
    let Either::Left(response) = result else {
        panic!("non-streaming request should return a payload");
    };

    // web_search is gateway-owned → executed against You.com mock.
    let search = captured_you.recv().await.expect("web_search should hit You.com");
    assert!(search.body.get("query").is_some(), "web_search executed with a query");

    // get_job_status is still client-owned → RequiresClientAction, one model call.
    assert_eq!(llm.request_bodies().await.len(), 1);
    assert!(
        function_call_names(&response.output).contains(&"get_job_status"),
        "client-owned call handed back alongside the executed gateway call"
    );
}

/// A gateway `web_search` turn followed by a Codex `namespace` turn, driven
/// through the loop as one conversation (per @maralbahari's review ask on #83).
///
/// Round 0: the model emits a gateway-owned `web_search` call → the loop
/// executes it against the You.com mock and continues. Round 1: the model emits
/// the flat, model-visible namespace call
/// `agentic_ns__mcp__agentic_fixture__add_numbers` → the loop restores it to
/// `{namespace: "mcp__agentic_fixture", name: "add_numbers"}`, classifies it as
/// client-owned, and hands the turn back (`RequiresClientAction`). Two model
/// calls total; the namespace call is returned restored, never flattened.
#[tokio::test]
async fn openai_gateway_web_search_then_codex_namespace_across_turns() {
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you().await;
    let llm = support::MockServer::start_deque(vec![
        cassette_turn("gateway_web_search_then_codex_namespace.yaml", 0),
        cassette_turn("gateway_web_search_then_codex_namespace.yaml", 1),
    ])
    .await;
    let exec_ctx = build_exec_ctx_with_web_search(llm.url(), &you_url).await;

    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let codex_namespace: ResponsesTool = serde_json::from_value(serde_json::json!({
        "type": "namespace",
        "name": "mcp__agentic_fixture",
        "tools": [{"type": "function", "name": "add_numbers", "parameters": {"type": "object"}}]
    }))
    .unwrap();

    let result = ExecuteRequest::new(
        request("search then add", Some(vec![web_search, codex_namespace])),
        exec_ctx,
    )
    .run()
    .await
    .expect("execute should succeed");
    let Either::Left(response) = result else {
        panic!("non-streaming request should return a payload");
    };

    // Round 0 executed the gateway web_search against the You.com mock, then
    // round 1 emitted the namespace call — two model calls in one conversation.
    let search = captured_you.recv().await.expect("web_search should hit You.com");
    assert!(search.body.get("query").is_some(), "web_search executed with a query");
    assert_eq!(
        llm.request_bodies().await.len(),
        2,
        "gateway round + client-action round"
    );

    // The namespace call is handed back restored to {namespace, name}, never
    // the flat model-visible name.
    let output = serde_json::to_value(&response.output).unwrap();
    let items = output.as_array().unwrap();
    let ns_call = items
        .iter()
        .find(|it| it["type"] == "function_call" && it["call_id"] == "call_ns")
        .expect("namespace call handed back to the client");
    assert_eq!(ns_call["name"], "add_numbers", "flat name restored to member name");
    assert_eq!(ns_call["namespace"], "mcp__agentic_fixture", "namespace restored");
    assert!(
        !items
            .iter()
            .any(|it| it["name"] == "agentic_ns__mcp__agentic_fixture__add_numbers"),
        "flat namespaced name must not leak to the client"
    );
    // The gateway web_search surfaced as a public web_search_call, not a raw fn.
    assert!(
        items.iter().any(|it| it["type"] == "web_search_call"),
        "web_search resolved into a public web_search_call: {output:#}"
    );
}

// --- You.com mock (mirrors web_search_tool_test.rs) -------------------------

struct CapturedSearch {
    body: serde_json::Value,
}

async fn spawn_mock_you() -> (
    String,
    tokio::sync::mpsc::Receiver<CapturedSearch>,
    tokio::task::JoinHandle<()>,
) {
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    let (tx, rx) = mpsc::channel(16);
    let app = Router::new()
        .route(
            "/v1/search",
            post(
                move |State(tx): State<mpsc::Sender<CapturedSearch>>, Json(body): Json<serde_json::Value>| async move {
                    tx.send(CapturedSearch { body }).await.unwrap();
                    (
                        axum::http::StatusCode::OK,
                        Json(serde_json::json!({
                            "results": {"web": [{"url": "https://example.com", "title": "R"}], "news": []},
                            "metadata": {"query": "q"}
                        })),
                    )
                },
            ),
        )
        .with_state(tx);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), rx, handle)
}
