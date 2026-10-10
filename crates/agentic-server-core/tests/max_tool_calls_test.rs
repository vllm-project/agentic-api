//! `max_tool_calls` enforcement through the executor with a scripted upstream.

use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agentic_core::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::WebSearchHandler;
use agentic_core::types::io::ResponsesInput;
use agentic_core::types::request_response::{MaxToolCalls, RequestPayload, ResponsePayload};
use agentic_core::types::tools::ResponsesTool;
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use either::Either;
use futures::StreamExt;
use serde_json::{Value, json};

mod support;

async fn spawn_counting_search() -> (String, Arc<AtomicUsize>) {
    let searches = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&searches);
    let app = Router::new().route(
        "/v1/search",
        get(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async {
                axum::Json(json!({
                    "results": {"web": [{"url": "https://example.com/r", "title": "r", "description": "r"}], "news": []}
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), searches)
}

async fn execution_context(llm_url: &str, search_url: &str) -> Arc<ExecutionContext> {
    let pool = support::setup_pool().await;
    let client = Arc::new(reqwest::Client::new());
    Arc::new(
        ExecutionContext::new(
            ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
            ResponseHandler::new(ResponseStore::new(pool)),
            Arc::clone(&client),
            llm_url.to_owned(),
        )
        .with_gateway_executor(Arc::new(WebSearchHandler::with_api_key(
            client,
            "test-key".to_owned(),
            search_url,
        ))),
    )
}

fn request(limit: Option<u64>, stream: bool, previous_response_id: Option<String>) -> RequestPayload {
    let tools: Vec<ResponsesTool> = serde_json::from_value(json!([
        {"type": "web_search"},
        {"type": "function", "name": "get_weather", "parameters": {"type": "object"}}
    ]))
    .unwrap();
    RequestPayload {
        model: "test-model".into(),
        input: ResponsesInput::Text("search".to_owned()),
        store: true,
        stream,
        tools: Some(tools),
        previous_response_id,
        max_tool_calls: limit.and_then(NonZeroU64::new).map(MaxToolCalls::new),
        ..Default::default()
    }
}

fn function_call(name: &str, call_id: &str) -> Value {
    json!({
        "id": format!("fc_{call_id}"), "type": "function_call", "call_id": call_id, "name": name,
        "arguments": format!(r#"{{"query":"{call_id}"}}"#), "status": "completed"
    })
}

fn calls_response(calls: &[(&str, &str)]) -> support::MockResponse {
    support::MockResponse::Json(
        json!({
            "id": "resp_calls", "object": "response", "created_at": 0, "model": "test-model",
            "status": "completed", "usage": null, "incomplete_details": null, "error": null,
            "output": calls.iter().map(|(name, id)| function_call(name, id)).collect::<Vec<_>>()
        })
        .to_string(),
    )
}

fn sse_response(events: impl IntoIterator<Item = Value>) -> support::MockResponse {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    support::MockResponse::Sse(body)
}

fn calls_sse_response(call_ids: &[&str]) -> support::MockResponse {
    let calls: Vec<_> = call_ids.iter().map(|id| ("web_search", *id)).collect();
    round_sse_response(&calls, None)
}

/// One upstream round streaming function calls, optionally followed by a message.
fn round_sse_response(calls: &[(&str, &str)], trailing_text: Option<&str>) -> support::MockResponse {
    let mut events =
        vec![json!({"type": "response.created", "response": {"id": "resp_calls", "status": "in_progress"}})];
    for (index, (name, call_id)) in calls.iter().enumerate() {
        let mut started = function_call(name, call_id);
        started["arguments"] = json!("");
        started["status"] = json!("in_progress");
        events.push(json!({"type": "response.output_item.added", "output_index": index, "item": started}));
        events.push(json!({
            "type": "response.function_call_arguments.done", "item_id": format!("fc_{call_id}"),
            "output_index": index, "call_id": call_id, "name": name,
            "arguments": format!(r#"{{"query":"{call_id}"}}"#)
        }));
    }
    if let Some(text) = trailing_text {
        let index = calls.len();
        events.push(
            json!({"type": "response.output_item.added", "output_index": index, "item": {
                "id": "msg_partial", "type": "message", "role": "assistant", "status": "in_progress", "content": []
            }}),
        );
        events.push(json!({"type": "response.output_text.delta", "item_id": "msg_partial",
            "output_index": index, "content_index": 0, "delta": text}));
    }
    events.push(
        json!({"type": "response.completed", "response": {"id": "resp_calls", "status": "completed", "usage": null}}),
    );
    sse_response(events)
}

fn text_sse_response(text: &str) -> support::MockResponse {
    let events = [
        json!({"type": "response.created", "response": {"id": "resp_text", "status": "in_progress"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {
            "id": "msg_text", "type": "message", "role": "assistant", "status": "in_progress", "content": []
        }}),
        json!({"type": "response.output_text.delta", "item_id": "msg_text", "output_index": 0,
            "content_index": 0, "delta": text}),
        json!({"type": "response.completed", "response": {"id": "resp_text", "status": "completed", "usage": null}}),
    ];
    sse_response(events)
}

async fn run_blocking(exec_ctx: &Arc<ExecutionContext>, payload: RequestPayload) -> ResponsePayload {
    match ExecuteRequest::new(payload, Arc::clone(exec_ctx)).run().await.unwrap() {
        Either::Left(response) => response,
        Either::Right(_) => panic!("expected a blocking response"),
    }
}

fn web_search_statuses(output: &Value) -> Vec<String> {
    output
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "web_search_call")
        .map(|item| item["status"].as_str().unwrap().to_owned())
        .collect()
}

fn tool_outputs(request_body: &Value) -> Vec<String> {
    request_body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn blocking_response_refuses_calls_over_the_limit_and_stops_offering_built_in_tools() {
    let (search_url, searches) = spawn_counting_search().await;
    let llm = support::MockServer::start_deque(vec![
        calls_response(&[
            ("web_search", "a"),
            ("web_search", "b"),
            ("web_search", "c"),
            ("web_search", "d"),
        ]),
        support::text_response("done"),
    ])
    .await;
    let exec_ctx = execution_context(llm.url(), &search_url).await;

    let response = run_blocking(&exec_ctx, request(Some(2), false, None)).await;

    assert_eq!(
        searches.load(Ordering::SeqCst),
        2,
        "refused calls must not reach the provider"
    );
    assert_eq!(response.status, "completed");
    assert_eq!(response.max_tool_calls, Some(2));
    let output = serde_json::to_value(&response.output).unwrap();
    assert_eq!(web_search_statuses(&output), ["completed", "completed", "searching"]);
    let bodies = llm.request_bodies().await;
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies[0].get("max_tool_calls").is_none(),
        "upstream never enforces built-in limits"
    );
    let refusals = tool_outputs(&bodies[1])
        .into_iter()
        .filter(|output| output.contains("UserError: Reached tool call limit of 2"))
        .count();
    assert_eq!(refusals, 2, "every refused call is answered for the model");
    let tool_names: Vec<_> = bodies[1]["tools"]
        .as_array()
        .map(|tools| tools.iter().map(|tool| tool["name"].clone()).collect())
        .unwrap_or_default();
    assert_eq!(
        tool_names,
        [json!("get_weather")],
        "built-in tools are withheld after a refusal"
    );
    assert!(
        bodies[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "web_search"),
        "the persisted request keeps the declared built-in tools"
    );
}

#[tokio::test]
async fn streaming_response_completes_a_refused_call_at_searching() {
    let (search_url, searches) = spawn_counting_search().await;
    let llm = support::MockServer::start_deque(vec![calls_sse_response(&["a", "b"]), text_sse_response("done")]).await;
    let exec_ctx = execution_context(llm.url(), &search_url).await;

    let result = ExecuteRequest::new(request(Some(1), true, None), Arc::clone(&exec_ctx))
        .run()
        .await
        .unwrap();
    let Either::Right(stream) = result else {
        panic!("expected a streaming response");
    };
    let chunks: Vec<String> = stream.collect().await;
    let events = support::streamed_sse_events(&chunks);

    assert_eq!(searches.load(Ordering::SeqCst), 1);
    let created = events.iter().find(|event| event["type"] == "response.created").unwrap();
    assert_eq!(created["response"]["max_tool_calls"], 1);
    let refused: Vec<_> = events
        .iter()
        .filter(|event| event["output_index"] == 1)
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        refused,
        [
            "response.output_item.added",
            "response.web_search_call.in_progress",
            "response.web_search_call.searching",
            "response.output_item.done",
        ]
    );
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    assert_eq!(completed["response"]["max_tool_calls"], 1);
    assert_eq!(
        web_search_statuses(&completed["response"]["output"]),
        ["completed", "searching"]
    );
    let message_index = events
        .iter()
        .find(|event| event["type"] == "response.output_item.added" && event["item"]["type"] == "message")
        .map(|event| event["output_index"].clone());
    assert_eq!(message_index, Some(json!(2)));
}

#[tokio::test]
async fn each_response_gets_its_own_budget_and_the_limit_is_not_inherited() {
    let (search_url, searches) = spawn_counting_search().await;
    let llm = support::MockServer::start_deque(vec![
        calls_response(&[("web_search", "a")]),
        calls_response(&[("web_search", "b")]),
        support::text_response("first"),
        calls_response(&[("web_search", "c")]),
        support::text_response("second"),
        calls_response(&[("web_search", "d"), ("web_search", "e")]),
        support::text_response("third"),
    ])
    .await;
    let exec_ctx = execution_context(llm.url(), &search_url).await;

    let first = run_blocking(&exec_ctx, request(Some(1), false, None)).await;
    assert_eq!(searches.load(Ordering::SeqCst), 1, "the budget spans inference rounds");
    let output = serde_json::to_value(&first.output).unwrap();
    assert_eq!(web_search_statuses(&output), ["completed", "searching"]);

    let second = run_blocking(&exec_ctx, request(Some(1), false, Some(first.id.clone()))).await;
    assert_eq!(
        searches.load(Ordering::SeqCst),
        2,
        "a follow-up response starts a fresh budget"
    );
    assert_eq!(second.max_tool_calls, Some(1));

    let third = run_blocking(&exec_ctx, request(None, false, Some(second.id.clone()))).await;
    assert_eq!(searches.load(Ordering::SeqCst), 4, "an omitted limit is not inherited");
    assert_eq!(third.max_tool_calls, None);
    assert_eq!(serde_json::to_value(&third).unwrap()["max_tool_calls"], Value::Null);
}

#[tokio::test]
async fn client_function_calls_are_not_counted() {
    let (search_url, searches) = spawn_counting_search().await;
    let llm = support::MockServer::start_deque(vec![calls_response(&[
        ("get_weather", "w1"),
        ("get_weather", "w2"),
        ("web_search", "s"),
    ])])
    .await;
    let exec_ctx = execution_context(llm.url(), &search_url).await;

    let response = run_blocking(&exec_ctx, request(Some(1), false, None)).await;

    assert_eq!(searches.load(Ordering::SeqCst), 1);
    let output = serde_json::to_value(&response.output).unwrap();
    assert_eq!(web_search_statuses(&output), ["completed"]);
    let client_calls = output
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call" && item["name"] == "get_weather")
        .count();
    assert_eq!(client_calls, 2);
}

#[tokio::test]
async fn streaming_omitted_refusal_keeps_later_public_indexes_contiguous() {
    let (search_url, searches) = spawn_counting_search().await;
    let llm = support::MockServer::start_deque(vec![
        round_sse_response(
            &[("web_search", "a"), ("web_search", "b"), ("file_search", "c")],
            Some("partial"),
        ),
        text_sse_response("done"),
    ])
    .await;
    let exec_ctx = execution_context(llm.url(), &search_url).await;
    let mut payload = request(Some(1), true, None);
    payload.tools = Some(
        serde_json::from_value(json!([{"type": "web_search"}, {"type": "file_search", "vector_store_ids": ["vs"]}]))
            .unwrap(),
    );

    let result = ExecuteRequest::new(payload, Arc::clone(&exec_ctx)).run().await.unwrap();
    let Either::Right(stream) = result else {
        panic!("expected a streaming response");
    };
    let chunks: Vec<String> = stream.collect().await;
    let events = support::streamed_sse_events(&chunks);

    assert_eq!(searches.load(Ordering::SeqCst), 1);
    // The refused web search is shown; the refused file search has no public item,
    // so the message that followed it in the same round moves up to index 2.
    let added: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "response.output_item.added")
        .map(|event| (event["output_index"].as_u64().unwrap(), event["item"]["type"].clone()))
        .collect();
    assert_eq!(
        added,
        [
            (0, json!("web_search_call")),
            (1, json!("web_search_call")),
            (2, json!("message")),
            (3, json!("message")),
        ]
    );
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    let output = &completed["response"]["output"];
    assert_eq!(output.as_array().unwrap().len(), 4);
    assert_eq!(web_search_statuses(output), ["completed", "searching"]);
}

#[tokio::test]
async fn a_round_with_client_calls_and_a_refusal_continues_with_a_fresh_budget() {
    let (search_url, searches) = spawn_counting_search().await;
    let llm = support::MockServer::start_deque(vec![
        calls_response(&[("web_search", "a"), ("web_search", "b"), ("get_weather", "w")]),
        support::text_response("done"),
    ])
    .await;
    let exec_ctx = execution_context(llm.url(), &search_url).await;

    let first = run_blocking(&exec_ctx, request(Some(1), false, None)).await;
    assert_eq!(searches.load(Ordering::SeqCst), 1);
    let output = serde_json::to_value(&first.output).unwrap();
    assert_eq!(web_search_statuses(&output), ["completed", "searching"]);
    assert!(
        output
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call" && item["call_id"] == "w"),
        "the client call is handed back"
    );

    let mut follow_up = request(Some(1), false, Some(first.id.clone()));
    follow_up.input =
        serde_json::from_value(json!([{"type": "function_call_output", "call_id": "w", "output": "sunny"}])).unwrap();
    let second = run_blocking(&exec_ctx, follow_up).await;

    assert_eq!(second.status, "completed");
    let bodies = llm.request_bodies().await;
    assert_eq!(bodies.len(), 2);
    let outputs = tool_outputs(&bodies[1]);
    assert!(
        outputs
            .iter()
            .any(|output| output.contains("UserError: Reached tool call limit of 1"))
    );
    assert!(outputs.iter().any(|output| output == "sunny"));
    assert!(
        bodies[1]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "web_search"),
        "a new response offers built-in tools again"
    );
}

/// Search provider that is always down, counting how often the gateway reached it.
async fn spawn_failing_search() -> (String, Arc<AtomicUsize>) {
    let searches = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&searches);
    let app = Router::new().route(
        "/v1/search",
        get(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { (StatusCode::INTERNAL_SERVER_ERROR, "provider down") }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), searches)
}

/// Minimal stateless MCP server over streamable HTTP with one `echo` tool, counting calls.
async fn spawn_counting_mcp() -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let app = Router::new().route(
        "/mcp",
        post(move |axum::Json(request): axum::Json<Value>| {
            let counter = Arc::clone(&counter);
            async move {
                let id = request["id"].clone();
                if id.is_null() {
                    // A notification such as `notifications/initialized`.
                    return StatusCode::ACCEPTED.into_response();
                }
                let reply = match request["method"].as_str() {
                    Some("initialize") => json!({"result": {
                        "protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                        "serverInfo": {"name": "stub", "version": "0"}
                    }}),
                    Some("tools/list") => json!({"result": {"tools": [{
                        "name": "echo", "description": "Echo the input.",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]}}),
                    Some("tools/call") => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        json!({"result": {"content": [{"type": "text", "text": "echoed"}], "isError": false}})
                    }
                    _ => json!({"error": {"code": -32601, "message": "method not found"}}),
                };
                let mut body = json!({"jsonrpc": "2.0", "id": id});
                body.as_object_mut().unwrap().extend(reply.as_object().unwrap().clone());
                axum::Json(body).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}/mcp"), calls)
}

/// A dispatched call that fails still spends the budget, and the budget is one
/// pool across tool kinds: the next call, to a different built-in tool, is refused.
#[tokio::test]
async fn a_failed_call_spends_the_budget_shared_across_tool_kinds() {
    let (search_url, searches) = spawn_failing_search().await;
    let (mcp_url, mcp_calls) = spawn_counting_mcp().await;
    let llm = support::MockServer::start_deque(vec![
        calls_response(&[("web_search", "a"), ("mcp__stub__echo", "b")]),
        support::text_response("done"),
    ])
    .await;
    let exec_ctx = execution_context(llm.url(), &search_url).await;
    let mut payload = request(Some(1), false, None);
    payload.tools = Some(
        serde_json::from_value(json!([
            {"type": "web_search"},
            {"type": "mcp", "server_label": "stub", "server_url": mcp_url, "require_approval": "never"}
        ]))
        .unwrap(),
    );

    let response = run_blocking(&exec_ctx, payload).await;

    assert_eq!(
        searches.load(Ordering::SeqCst),
        1,
        "the failing call executes exactly once"
    );
    assert_eq!(
        mcp_calls.load(Ordering::SeqCst),
        0,
        "the refused MCP call never reaches the server"
    );
    assert_eq!(response.status, "completed");
    assert_eq!(response.max_tool_calls, Some(1));
    let output = serde_json::to_value(&response.output).unwrap();
    assert_eq!(web_search_statuses(&output), ["failed"]);
    let item_types: Vec<_> = output
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["type"].clone())
        .collect();
    assert!(item_types.contains(&json!("mcp_list_tools")), "{item_types:?}");
    assert!(
        !item_types.contains(&json!("mcp_call")),
        "a refused MCP call is omitted: {item_types:?}"
    );
    let bodies = llm.request_bodies().await;
    assert_eq!(bodies.len(), 2, "the model answers after the refusal round");
    let outputs = tool_outputs(&bodies[1]);
    assert_eq!(outputs.len(), 2);
    assert!(
        outputs[0].contains("\"error\""),
        "the failed search reports its error: {}",
        outputs[0]
    );
    assert_eq!(outputs[1], r#"{"error":"UserError: Reached tool call limit of 1"}"#);
    let tool_names: Vec<_> = bodies[1]["tools"]
        .as_array()
        .map(|tools| tools.iter().map(|tool| tool["name"].clone()).collect())
        .unwrap_or_default();
    assert!(
        !tool_names
            .iter()
            .any(|name| name == "web_search" || name == "mcp__stub__echo"),
        "built-in tools are withheld after the refusal: {tool_names:?}"
    );
}
