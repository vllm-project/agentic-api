#![allow(clippy::doc_markdown)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use agentic_core::executor::{ExecutorError, LoopDecision, ToolContext, dispatch_tools};
use agentic_core::tools::McpToolExecutor;
use agentic_core::types::io::{FunctionToolCall, InputItem, OutputItem, OutputMessage, OutputTextContent};

// --- Mock implementations ---

/// Mock MCP executor that returns pre-configured responses by tool name.
/// If a tool name is not in the map, returns an error (simulating unknown tool).
struct MockMcp {
    responses: std::collections::HashMap<String, String>,
}

impl MockMcp {
    fn new(pairs: &[(&str, &str)]) -> Self {
        Self {
            responses: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }
}

impl McpToolExecutor for MockMcp {
    fn execute(
        &self,
        tool_name: &str,
        _arguments: &str,
        _server_config: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ExecutorError>> + Send + '_>> {
        let name = tool_name.to_string();
        Box::pin(async move {
            self.responses
                .get(&name)
                .cloned()
                .ok_or_else(|| ExecutorError::InvalidRequest(format!("unknown tool: {name}")))
        })
    }
}

/// Mock MCP executor that always fails — simulates a crashed/unavailable tool provider.
struct FailingMcp;

impl McpToolExecutor for FailingMcp {
    fn execute(
        &self,
        tool_name: &str,
        _arguments: &str,
        _server_config: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ExecutorError>> + Send + '_>> {
        let name = tool_name.to_string();
        Box::pin(async move { Err(ExecutorError::StreamError(format!("tool '{name}' crashed"))) })
    }
}

// --- Helpers ---

fn make_function_call(name: &str, args: &str, call_id: &str) -> OutputItem {
    OutputItem::FunctionCall(FunctionToolCall {
        id: format!("fc_{call_id}"),
        call_id: call_id.to_string(),
        name: name.to_string(),
        arguments: args.to_string(),
        status: "completed".to_string(),
    })
}

fn make_message(text: &str) -> OutputItem {
    OutputItem::Message(OutputMessage {
        id: "msg_1".to_string(),
        role: "assistant".to_string(),
        status: "completed".to_string(),
        content: vec![OutputTextContent::new(text)],
    })
}

fn tool_ctx_with_mcp(mcp: impl McpToolExecutor + 'static) -> ToolContext {
    ToolContext {
        mcp: Some(Arc::new(mcp)),
        max_iterations: 10,
        ..ToolContext::default()
    }
}

// --- Tests ---

/// When output contains only text messages (no FunctionCall items),
/// dispatch should return Done — nothing to execute.
#[tokio::test]
async fn test_no_function_calls_returns_done() {
    let output = vec![make_message("Hello world")];
    let ctx = ToolContext::default();

    let decision = dispatch_tools(&output, &ctx, 0).await.unwrap();
    assert!(matches!(decision, LoopDecision::Done));
}

/// When output is completely empty, dispatch returns Done.
#[tokio::test]
async fn test_empty_output_returns_done() {
    let output: Vec<OutputItem> = vec![];
    let ctx = ToolContext::default();

    let decision = dispatch_tools(&output, &ctx, 0).await.unwrap();
    assert!(matches!(decision, LoopDecision::Done));
}

/// Single FunctionCall in output → execute via MCP → return Continue with
/// the tool result wrapped as InputItem::FunctionCallOutput.
#[tokio::test]
async fn test_single_function_call_returns_continue() {
    let output = vec![make_function_call("get_weather", r#"{"city":"SF"}"#, "call_1")];
    let mcp = MockMcp::new(&[("get_weather", r#"{"temp": 72}"#)]);
    let ctx = tool_ctx_with_mcp(mcp);

    let decision = dispatch_tools(&output, &ctx, 0).await.unwrap();

    if let LoopDecision::Continue(items) = decision {
        assert_eq!(items.len(), 1);
        if let InputItem::FunctionCallOutput(result) = &items[0] {
            assert_eq!(result.call_id, "call_1");
            assert_eq!(result.output, r#"{"temp": 72}"#);
        } else {
            panic!("expected FunctionCallOutput");
        }
    } else {
        panic!("expected Continue, got {decision:?}");
    }
}

/// Multiple FunctionCall items in output → all execute concurrently via join_all →
/// Continue with results for each call_id. Order may vary (parallel execution).
#[tokio::test]
async fn test_parallel_function_calls() {
    let output = vec![
        make_function_call("get_weather", r#"{"city":"SF"}"#, "call_1"),
        make_function_call("get_time", r#"{"tz":"PST"}"#, "call_2"),
    ];
    let mcp = MockMcp::new(&[("get_weather", "sunny"), ("get_time", "10:30 AM")]);
    let ctx = tool_ctx_with_mcp(mcp);

    let decision = dispatch_tools(&output, &ctx, 0).await.unwrap();

    if let LoopDecision::Continue(items) = decision {
        assert_eq!(items.len(), 2);
        let outputs: Vec<_> = items
            .iter()
            .filter_map(|item| match item {
                InputItem::FunctionCallOutput(r) => Some((r.call_id.as_str(), r.output.as_str())),
                _ => None,
            })
            .collect();
        assert!(outputs.contains(&("call_1", "sunny")));
        assert!(outputs.contains(&("call_2", "10:30 AM")));
    } else {
        panic!("expected Continue");
    }
}

/// When iteration count reaches max_iterations, dispatch returns Incomplete
/// WITHOUT executing any tools — prevents infinite tool loops.
#[tokio::test]
async fn test_max_iterations_returns_incomplete() {
    let output = vec![make_function_call("get_weather", "{}", "call_1")];
    let mcp = MockMcp::new(&[("get_weather", "sunny")]);
    let ctx = ToolContext {
        mcp: Some(Arc::new(mcp)),
        max_iterations: 3,
        ..ToolContext::default()
    };

    // iteration=3, max=3 → 3 >= 3 is true → Incomplete
    let decision = dispatch_tools(&output, &ctx, 3).await.unwrap();

    if let LoopDecision::Incomplete(reason) = decision {
        assert!(reason.contains("max tool iterations"));
    } else {
        panic!("expected Incomplete, got {decision:?}");
    }
}

/// When a tool provider returns an error, it becomes an error JSON string
/// in the output (not a total dispatch failure). The model sees the error
/// and can decide to retry on the next iteration.
#[tokio::test]
async fn test_failing_tool_produces_error_output_not_total_failure() {
    let output = vec![make_function_call("bad_tool", "{}", "call_1")];
    let ctx = tool_ctx_with_mcp(FailingMcp);

    let decision = dispatch_tools(&output, &ctx, 0).await.unwrap();

    if let LoopDecision::Continue(items) = decision {
        assert_eq!(items.len(), 1);
        if let InputItem::FunctionCallOutput(result) = &items[0] {
            assert_eq!(result.call_id, "call_1");
            assert!(result.output.contains("error"));
            assert!(result.output.contains("crashed"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    } else {
        panic!("expected Continue (with error output), got {decision:?}");
    }
}

/// When no providers are configured at all, each call gets an error output
/// saying "no tool provider configured". dispatch still returns Continue
/// (the model sees the errors and handles them).
#[tokio::test]
async fn test_no_provider_configured_produces_error_output() {
    let output = vec![make_function_call("get_weather", "{}", "call_1")];
    let ctx = ToolContext {
        max_iterations: 10,
        ..ToolContext::default()
    };

    let decision = dispatch_tools(&output, &ctx, 0).await.unwrap();

    if let LoopDecision::Continue(items) = decision {
        assert_eq!(items.len(), 1);
        if let InputItem::FunctionCallOutput(result) = &items[0] {
            assert!(result.output.contains("error"));
            assert!(result.output.contains("no tool provider configured"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    } else {
        panic!("expected Continue (with error output), got {decision:?}");
    }
}

/// When multiple tools are called and some succeed while others fail,
/// ALL results are returned — successes with their output, failures with
/// error JSON. The model gets partial results and decides what to do.
#[tokio::test]
async fn test_mixed_success_and_failure() {
    let output = vec![
        make_function_call("good_tool", "{}", "call_1"),
        make_function_call("bad_tool", "{}", "call_2"),
    ];
    let mcp = MockMcp::new(&[("good_tool", "success result")]);
    // bad_tool not in MockMcp map → returns InvalidRequest error
    let ctx = tool_ctx_with_mcp(mcp);

    let decision = dispatch_tools(&output, &ctx, 0).await.unwrap();

    if let LoopDecision::Continue(items) = decision {
        assert_eq!(items.len(), 2);
        let results: std::collections::HashMap<_, _> = items
            .iter()
            .filter_map(|item| match item {
                InputItem::FunctionCallOutput(r) => Some((r.call_id.as_str(), r.output.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(results["call_1"], "success result");
        assert!(results["call_2"].contains("error"));
    } else {
        panic!("expected Continue");
    }
}

/// When output contains both Message and FunctionCall items, only the
/// FunctionCall items are dispatched. Messages are ignored by dispatch
/// (they're part of the response, not actionable tool calls).
#[tokio::test]
async fn test_function_call_mixed_with_message_output() {
    let output = vec![
        make_message("Let me check the weather"),
        make_function_call("get_weather", r#"{"city":"NYC"}"#, "call_1"),
    ];
    let mcp = MockMcp::new(&[("get_weather", "rainy")]);
    let ctx = tool_ctx_with_mcp(mcp);

    let decision = dispatch_tools(&output, &ctx, 0).await.unwrap();

    if let LoopDecision::Continue(items) = decision {
        assert_eq!(items.len(), 1);
        if let InputItem::FunctionCallOutput(result) = &items[0] {
            assert_eq!(result.call_id, "call_1");
            assert_eq!(result.output, "rainy");
        } else {
            panic!("expected FunctionCallOutput");
        }
    } else {
        panic!("expected Continue");
    }
}

/// Boundary test: iteration=0 with max_iterations=1 should execute (0 < 1).
/// iteration=1 with max_iterations=1 should return Incomplete (1 >= 1).
/// Verifies the >= comparison is correct.
#[tokio::test]
async fn test_iteration_zero_under_max_executes() {
    let output = vec![make_function_call("tool", "{}", "call_1")];
    let mcp = MockMcp::new(&[("tool", "ok")]);
    let ctx = ToolContext {
        mcp: Some(Arc::new(mcp)),
        max_iterations: 1,
        ..ToolContext::default()
    };

    // iteration=0, max=1 → 0 < 1 → should execute
    let decision = dispatch_tools(&output, &ctx, 0).await.unwrap();
    assert!(matches!(decision, LoopDecision::Continue(_)));

    // iteration=1, max=1 → 1 >= 1 → should be Incomplete
    let mcp2 = MockMcp::new(&[("tool", "ok")]);
    let ctx2 = ToolContext {
        mcp: Some(Arc::new(mcp2)),
        max_iterations: 1,
        ..ToolContext::default()
    };
    let decision2 = dispatch_tools(&output, &ctx2, 1).await.unwrap();
    assert!(matches!(decision2, LoopDecision::Incomplete(_)));
}
