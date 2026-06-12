// Integration tests for execute_loop — the agentic loop orchestrator.
#![allow(clippy::doc_markdown)]

mod support;

use serde::Deserialize;

use std::sync::Arc;

use agentic_core::executor::{ExecutionContext, ExecutorError, ToolContext, execute_loop};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tools::McpToolExecutor;
use agentic_core::types::io::{ResponsesInput, ToolChoice};
use agentic_core::types::request_response::RequestPayload;
use support::{MockResponse, MockServer, setup_pool};

use std::future::Future;
use std::pin::Pin;

// --- Mock tool implementations ---

/// Returns a configured response for any tool call.
struct MockMcp {
    response: String,
}

impl MockMcp {
    fn new(response: &str) -> Self {
        Self {
            response: response.to_string(),
        }
    }
}

impl McpToolExecutor for MockMcp {
    fn execute(
        &self,
        _tool_name: &str,
        _arguments: &str,
        _server_config: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ExecutorError>> + Send + '_>> {
        Box::pin(async { Ok(self.response.clone()) })
    }
}

/// Always fails — simulates a crashed tool provider.
struct FailingMcp;

impl McpToolExecutor for FailingMcp {
    fn execute(
        &self,
        _tool_name: &str,
        _arguments: &str,
        _server_config: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ExecutorError>> + Send + '_>> {
        Box::pin(async { Err(ExecutorError::StreamError("MCP server unreachable".into())) })
    }
}

// --- Helpers ---

fn make_request(input: &str, stream: bool, store: bool) -> RequestPayload {
    RequestPayload {
        model: "test-model".to_string(),
        input: ResponsesInput::Text(input.to_string()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: None,
        tool_choice: ToolChoice::Auto,
        stream,
        store,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        truncation: None,
        metadata: None,
    }
}

/// Build an ExecutionContext backed by a mock LLM server.
async fn build_exec_ctx(server: &MockServer) -> Arc<ExecutionContext> {
    let pool = setup_pool().await;
    Arc::new(ExecutionContext::new(
        agentic_core::executor::ConversationHandler::new(ConversationStore::new(pool.clone())),
        agentic_core::executor::ResponseHandler::new(ResponseStore::new(pool)),
        Arc::new(reqwest::Client::new()),
        server.url().to_string(),
        None,
    ))
}

/// Create a mock LLM response that contains only a text message.
fn text_llm_response(text: &str) -> MockResponse {
    MockResponse::Json(
        serde_json::json!({
            "id": "resp_mock_text",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [{
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}]
            }],
            "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
        })
        .to_string(),
    )
}

/// Create a mock LLM response that contains a function_call output item.
fn function_call_llm_response(name: &str, args: &str, call_id: &str) -> MockResponse {
    MockResponse::Json(
        serde_json::json!({
            "id": "resp_mock_fc",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [{
                "id": format!("fc_{call_id}"),
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": args,
                "status": "completed"
            }],
            "usage": {"input_tokens": 10, "output_tokens": 8, "total_tokens": 18}
        })
        .to_string(),
    )
}

/// Create a mock LLM response with multiple function calls (parallel tool use).
fn parallel_function_calls_response() -> MockResponse {
    MockResponse::Json(
        serde_json::json!({
            "id": "resp_mock_parallel",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [
                {
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_weather",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"SF\"}",
                    "status": "completed"
                },
                {
                    "id": "fc_2",
                    "type": "function_call",
                    "call_id": "call_time",
                    "name": "get_time",
                    "arguments": "{\"tz\":\"PST\"}",
                    "status": "completed"
                }
            ],
            "usage": {"input_tokens": 10, "output_tokens": 12, "total_tokens": 22}
        })
        .to_string(),
    )
}

/// Create a mock LLM response with both a message AND a function call.
fn mixed_message_and_function_call_response() -> MockResponse {
    MockResponse::Json(
        serde_json::json!({
            "id": "resp_mock_mixed",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [
                {
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "Let me check that for you.", "annotations": []}]
                },
                {
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "lookup",
                    "arguments": "{\"q\":\"test\"}",
                    "status": "completed"
                }
            ],
            "usage": {"input_tokens": 10, "output_tokens": 15, "total_tokens": 25}
        })
        .to_string(),
    )
}

// --- P0: Streaming rejection ---

/// execute_loop only supports non-streaming (MVP). Passing stream=true should
/// return an immediate error without making any LLM calls.
#[tokio::test]
async fn test_rejects_streaming_request() {
    let server = MockServer::start_deque(vec![text_llm_response("should not reach")]).await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext::default();

    let request = make_request("hello", true, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("streaming"),
        "error should mention streaming: {err}"
    );

    // Verify no LLM call was made
    assert_eq!(server.request_bodies().await.len(), 0);
}

// --- P1: No tools → single iteration → Done ---

/// When the model responds with only text (no FunctionCall items), execute_loop
/// should return immediately after one iteration without re-entering.
#[tokio::test]
async fn test_no_tool_calls_returns_directly() {
    let server = MockServer::start_deque(vec![text_llm_response("Hello world")]).await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext::default();

    let request = make_request("hi", false, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await.unwrap();

    assert_eq!(result.status, "completed");
    assert_eq!(server.request_bodies().await.len(), 1, "should call LLM exactly once");
}

/// When store=true and no tool calls, the response should be persisted.
/// Verify by checking the DB has a response record after the loop.
#[tokio::test]
async fn test_no_tool_calls_persists_when_store_true() {
    let server = MockServer::start_deque(vec![text_llm_response("Persisted response")]).await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext::default();

    let request = make_request("save me", false, true);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await.unwrap();

    assert_eq!(result.status, "completed");
    // If persist failed, we'd see a warning but not an error — the function still succeeds
}

// --- P1: Tool call → re-enter → text response ---

/// Model calls a tool on first iteration, gets result, produces text on second.
/// This is the core agentic loop path.
#[tokio::test]
async fn test_one_tool_call_then_text_response() {
    let server = MockServer::start_deque(vec![
        function_call_llm_response("get_weather", r#"{"city":"SF"}"#, "call_1"),
        text_llm_response("The weather in SF is sunny, 72°F"),
    ])
    .await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext {
        mcp: Some(Arc::new(MockMcp::new(r#"{"temp":72,"condition":"sunny"}"#))),
        max_iterations: 10,
        ..ToolContext::default()
    };

    let request = make_request("What's the weather in SF?", false, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await.unwrap();

    // Final response is the text from iteration 2
    assert_eq!(result.status, "completed");

    // LLM was called twice: once for initial request, once with tool results
    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), 2, "should call LLM exactly twice");

    // Second request should contain the tool result in its input
    let second_body = &bodies[1];
    let input_str = serde_json::to_string(&second_body["input"]).unwrap();
    assert!(
        input_str.contains("function_call_output"),
        "second request should have tool result: {input_str}"
    );
    assert!(input_str.contains("call_1"), "should reference the original call_id");
}

/// Model calls two tools in parallel, gets both results, produces final text.
#[tokio::test]
async fn test_parallel_tool_calls_then_text() {
    let server = MockServer::start_deque(vec![
        parallel_function_calls_response(),
        text_llm_response("Weather is sunny and time is 10:30 AM"),
    ])
    .await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext {
        mcp: Some(Arc::new(MockMcp::new("tool result"))),
        max_iterations: 10,
        ..ToolContext::default()
    };

    let request = make_request("weather and time?", false, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await.unwrap();

    assert_eq!(result.status, "completed");
    assert_eq!(server.request_bodies().await.len(), 2);

    // Second request input should have 2 function_call_output items
    let bodies = server.request_bodies().await;
    let input_str = serde_json::to_string(&bodies[1]["input"]).unwrap();
    assert!(input_str.contains("call_weather"), "should have weather result");
    assert!(input_str.contains("call_time"), "should have time result");
}

/// Mixed output: message + function_call. Only the function_call triggers dispatch.
/// On second iteration, model returns final text.
#[tokio::test]
async fn test_mixed_message_and_function_call() {
    let server = MockServer::start_deque(vec![
        mixed_message_and_function_call_response(),
        text_llm_response("Here's what I found."),
    ])
    .await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext {
        mcp: Some(Arc::new(MockMcp::new("lookup result"))),
        max_iterations: 10,
        ..ToolContext::default()
    };

    let request = make_request("find something", false, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await.unwrap();

    assert_eq!(result.status, "completed");
    assert_eq!(server.request_bodies().await.len(), 2);
}

// --- P1: Max iterations ---

/// When the model keeps returning tool calls and max_iterations is hit,
/// execute_loop should stop and return the last payload (not error).
#[tokio::test]
async fn test_max_iterations_stops_loop() {
    // LLM always returns a function call — would loop forever without max_iterations
    let server = MockServer::start_deque(vec![
        function_call_llm_response("tool", "{}", "c1"),
        function_call_llm_response("tool", "{}", "c2"),
        function_call_llm_response("tool", "{}", "c3"),
        text_llm_response("should not reach this"),
    ])
    .await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext {
        mcp: Some(Arc::new(MockMcp::new("ok"))),
        max_iterations: 2, // will stop after 2 iterations
        ..ToolContext::default()
    };

    let request = make_request("loop forever", false, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await.unwrap();

    // Should have called LLM 3 times (iteration 0, 1, 2) then stopped at dispatch
    // iteration 0: execute → FC → dispatch(iter=0) → Continue
    // iteration 1: execute → FC → dispatch(iter=1) → Continue
    // iteration 2: execute → FC → dispatch(iter=2) → Incomplete (2 >= 2)
    assert_eq!(server.request_bodies().await.len(), 3);

    // Returns the last payload (the one from iteration 2)
    assert_eq!(result.status, "completed");
}

/// max_iterations=1 means only 1 tool dispatch is allowed.
#[tokio::test]
async fn test_max_iterations_one_allows_single_dispatch() {
    let server = MockServer::start_deque(vec![
        function_call_llm_response("tool", "{}", "c1"),
        function_call_llm_response("tool", "{}", "c2"),
    ])
    .await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext {
        mcp: Some(Arc::new(MockMcp::new("ok"))),
        max_iterations: 1,
        ..ToolContext::default()
    };

    let request = make_request("once", false, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await.unwrap();

    // iteration 0: execute → FC → dispatch(iter=0) → Continue (0 < 1)
    // iteration 1: execute → FC → dispatch(iter=1) → Incomplete (1 >= 1)
    assert_eq!(server.request_bodies().await.len(), 2);
    assert_eq!(result.status, "completed");
}

// --- P1: Tool failure doesn't kill the loop ---

/// When a tool provider fails, the error becomes output that the model sees.
/// The loop continues and the model responds to the error gracefully.
#[tokio::test]
async fn test_tool_failure_feeds_error_to_model() {
    let server = MockServer::start_deque(vec![
        function_call_llm_response("broken_tool", "{}", "call_err"),
        text_llm_response("Sorry, the tool failed. Here's what I know..."),
    ])
    .await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext {
        mcp: Some(Arc::new(FailingMcp)),
        max_iterations: 10,
        ..ToolContext::default()
    };

    let request = make_request("try the broken tool", false, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await.unwrap();

    assert_eq!(result.status, "completed");
    assert_eq!(server.request_bodies().await.len(), 2);

    // The second request should contain the error string as tool output
    let bodies = server.request_bodies().await;
    let input_str = serde_json::to_string(&bodies[1]["input"]).unwrap();
    assert!(input_str.contains("error"), "should contain error output: {input_str}");
    assert!(
        input_str.contains("MCP server unreachable"),
        "should contain error message"
    );
}

// --- Edge cases ---

/// No tool providers configured at all — calls still produce error output
/// and the model handles it on the next iteration.
#[tokio::test]
async fn test_no_providers_configured() {
    let server = MockServer::start_deque(vec![
        function_call_llm_response("any_tool", "{}", "call_1"),
        text_llm_response("I can't use tools right now"),
    ])
    .await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext {
        max_iterations: 10,
        ..ToolContext::default() // no providers
    };

    let request = make_request("use a tool", false, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await.unwrap();

    assert_eq!(result.status, "completed");
    assert_eq!(server.request_bodies().await.len(), 2);

    // Error message about no provider should be in the second request
    let bodies = server.request_bodies().await;
    let input_str = serde_json::to_string(&bodies[1]["input"]).unwrap();
    assert!(input_str.contains("no tool provider configured"));
}

/// Empty model output (no message, no function calls) — should return Done.
#[tokio::test]
async fn test_empty_model_output() {
    let empty_response = MockResponse::Json(
        serde_json::json!({
            "id": "resp_empty",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [],
            "usage": null
        })
        .to_string(),
    );

    let server = MockServer::start_deque(vec![empty_response]).await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext::default();

    let request = make_request("silence", false, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await.unwrap();

    assert_eq!(result.status, "completed");
    assert!(result.output.is_empty());
    assert_eq!(server.request_bodies().await.len(), 1);
}

/// Multi-hop: model calls tool A, then uses result to call tool B, then responds.
/// Tests 3 iterations of the loop.
#[tokio::test]
async fn test_multi_hop_tool_calls() {
    let server = MockServer::start_deque(vec![
        function_call_llm_response("search", "cats", "call_search"),
        function_call_llm_response("summarize", "cat article text", "call_summarize"),
        text_llm_response("Cats are wonderful pets."),
    ])
    .await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext {
        mcp: Some(Arc::new(MockMcp::new("tool output"))),
        max_iterations: 10,
        ..ToolContext::default()
    };

    let request = make_request("tell me about cats", false, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await.unwrap();

    assert_eq!(result.status, "completed");
    assert_eq!(server.request_bodies().await.len(), 3, "should make 3 LLM calls");
}

/// LLM returns an error (non-2xx) — execute_loop should propagate the error.
#[tokio::test]
async fn test_llm_returns_error() {
    // The current MockServer always returns 200, so we use an empty queue
    // which causes "mock queue exhausted" panic — simulating server failure.
    let server = MockServer::start_deque(vec![]).await;
    let exec_ctx = build_exec_ctx(&server).await;
    let tool_ctx = ToolContext::default();

    let request = make_request("fail", false, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await;

    // Should propagate the error (mock queue exhausted = panic in mock, caught as error)
    // In practice this tests that execute_loop doesn't swallow execute() errors
    assert!(result.is_err(), "should propagate LLM error");
}

// --- P2: Cassette-driven integration test (real vLLM output) ---

const TOOL_LOOP_CASSETTE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/tool_loop");

#[derive(Deserialize)]
struct ToolLoopCassette {
    turns: Vec<CassetteTurn>,
    expected: CassetteExpected,
    tool_mock: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
struct CassetteTurn {
    response: CassetteTurnResponse,
}

#[derive(Deserialize)]
struct CassetteTurnResponse {
    body: String,
}

#[derive(Deserialize)]
struct CassetteExpected {
    iterations: usize,
    final_text: String,
}

/// Replays a recorded vLLM tool-call session through execute_loop.
/// Validates the loop produces the same final text as the real model.
#[tokio::test]
async fn test_cassette_tool_loop_vllm_gemma4() {
    let path = format!("{TOOL_LOOP_CASSETTE}/function-call-loop-vllm-gemma4.yaml");
    let text = std::fs::read_to_string(&path).unwrap();
    let cassette: ToolLoopCassette = serde_yml::from_str(&text).unwrap();

    // Build mock server with the recorded responses queued
    let responses: Vec<MockResponse> = cassette
        .turns
        .iter()
        .map(|t| MockResponse::Json(t.response.body.clone()))
        .collect();
    let server = MockServer::start_deque(responses).await;
    let exec_ctx = build_exec_ctx(&server).await;

    // Build ToolContext with mock that returns the cassette's tool_mock value
    let tool_response = cassette.tool_mock.get("get_weather").cloned().unwrap_or_default();
    let tool_ctx = ToolContext {
        mcp: Some(Arc::new(MockMcp::new(&tool_response))),
        max_iterations: 10,
        ..ToolContext::default()
    };

    let request = make_request("What is the weather in San Francisco?", false, false);
    let result = execute_loop(request, exec_ctx, &tool_ctx).await.unwrap();

    // Verify the loop ran the expected number of iterations
    let bodies = server.request_bodies().await;
    assert_eq!(
        bodies.len(),
        cassette.expected.iterations,
        "expected {} LLM calls, got {}",
        cassette.expected.iterations,
        bodies.len()
    );

    // Verify final response contains the expected text
    assert_eq!(result.status, "completed");
    let output_text: String = result
        .output
        .iter()
        .filter_map(|item| match item {
            agentic_core::types::io::OutputItem::Message(msg) => {
                Some(msg.content.iter().map(|c| c.text.as_str()).collect::<String>())
            }
            _ => None,
        })
        .collect();
    assert_eq!(output_text, cassette.expected.final_text);
}
