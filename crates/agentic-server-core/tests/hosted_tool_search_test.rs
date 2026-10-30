//! Gateway-executed (hosted) tool search, issue #448.
//!
//! The upstream model sees a lowered `tool_search` function; the gateway runs
//! each search itself and returns the public items recorded from the
//! Responses API (`hosted_tool_search_characterization_test.rs`).

mod support;

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use agentic_core::config::ToolRuntimeConfig;
use agentic_core::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler, ResponseSession};
use agentic_core::storage::{ConversationStore, ResponseStore, create_pool_with_schema};
use agentic_core::tool::mcp::McpServerEntry;
use agentic_core::tool::{GatewayExecutors, model_visible_namespace_member_name};
use agentic_core::types::request_response::{RequestPayload, ResponsePayload};
use axum::Router;
use axum::body::Bytes;
use axum::response::IntoResponse;
use axum::routing::post;
use either::Either;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

type Requests = Arc<Mutex<Vec<Value>>>;

/// A mock upstream that emits each prepared response as JSON or SSE according to the request.
async fn spawn_llm(responses: Vec<Value>) -> (String, Requests) {
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (route_responses, route_requests) = (Arc::clone(&responses), Arc::clone(&requests));
    let app = Router::new().route(
        "/v1/responses",
        post(move |body: Bytes| {
            let (responses, requests) = (Arc::clone(&route_responses), Arc::clone(&route_requests));
            async move {
                let request: Value = serde_json::from_slice(&body).expect("request JSON");
                let stream = request["stream"] == true;
                requests.lock().await.push(request);
                let response = responses.lock().await.pop_front().expect("one response per request");
                let (content_type, body) = if stream {
                    (
                        "text/event-stream",
                        sse(response["id"].as_str().unwrap(), response["output"].as_array().unwrap()),
                    )
                } else {
                    ("application/json", response.to_string())
                };
                axum::response::Response::builder()
                    .status(200)
                    .header("Content-Type", content_type)
                    .body(axum::body::Body::from(body))
                    .expect("mock response")
                    .into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock LLM");
    let address = listener.local_addr().expect("mock address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("mock server") });
    (format!("http://{address}"), requests)
}

fn context(llm_url: String) -> Arc<ExecutionContext> {
    Arc::new(ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        llm_url,
    ))
}

fn upstream(id: &str, output: Value) -> Value {
    let mut response = json!({"id": id, "object": "response", "status": "completed", "model": "test", "created_at": 0});
    response["output"] = output;
    response
}

fn function_call(suffix: &str, name: &str, arguments: &Value) -> Value {
    json!({
        "type": "function_call", "id": format!("fc_{suffix}"), "call_id": format!("call_{suffix}"),
        "name": name, "arguments": arguments.to_string(), "status": "completed"
    })
}

/// An assistant message; its id derives from the text, so each scripted answer is distinct.
fn message(text: &str) -> Value {
    let id: String = text.chars().filter(char::is_ascii_alphanumeric).collect();
    json!({
        "type": "message", "id": format!("msg_{id}"), "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": []}]
    })
}

fn deferred_weather() -> Value {
    json!({
        "type": "function", "name": "get_weather", "description": "Get the current weather for a city",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"],
            "additionalProperties": false},
        "strict": true, "defer_loading": true
    })
}

fn request(input: &Value, tools: &Value) -> RequestPayload {
    serde_json::from_value(json!({"model": "test", "input": input, "tools": tools, "store": false}))
        .expect("public request")
}

async fn run(request: RequestPayload, context: Arc<ExecutionContext>) -> ResponsePayload {
    if request.stream {
        let events = run_streaming(request, context).await;
        let terminal = events
            .iter()
            .find(|event| event["type"] == "response.completed")
            .unwrap_or_else(|| panic!("completed response: {events:?}"));
        return serde_json::from_value(terminal["response"].clone()).unwrap();
    }
    match Box::pin(ExecuteRequest::new(request, context).run())
        .await
        .expect("blocking execution")
    {
        Either::Left(response) => response,
        Either::Right(_) => panic!("blocking request returned a stream"),
    }
}

fn output(response: &ResponsePayload) -> Vec<Value> {
    response
        .output
        .iter()
        .map(|item| serde_json::to_value(item).expect("output item serializes"))
        .collect()
}

fn tool_names(upstream_request: &Value) -> Vec<String> {
    upstream_request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn blocking_search_loads_a_deferred_function_for_the_next_round() {
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call("s1", "tool_search", &json!({"paths": ["get_weather"]}))]),
        ),
        upstream(
            "up_2",
            json!([function_call("w1", "get_weather", &json!({"city": "Paris"}))]),
        ),
    ])
    .await;
    let response = run(
        request(
            &json!("weather in Paris?"),
            &json!([{"type": "tool_search"}, deferred_weather()]),
        ),
        context(llm_url),
    )
    .await;

    let items = output(&response);
    assert_eq!(
        items[0],
        json!({"type": "tool_search_call", "id": "tsc_s1", "call_id": null, "execution": "server",
            "arguments": {"paths": ["get_weather"]}, "status": "completed"})
    );
    assert_eq!(
        items[1],
        json!({"type": "tool_search_output", "id": "tso_s1", "call_id": null, "execution": "server",
            "status": "completed", "tools": [deferred_weather()]})
    );
    assert_eq!(items[2]["type"], "function_call");
    assert_eq!(items[2]["name"], "get_weather");
    // Recorded quirk: a searched top-level function reports its own name as namespace.
    assert_eq!(items[2]["namespace"], "get_weather");
    assert_eq!(items.len(), 3);

    // The tools echo keeps the declaration, as the Responses API does.
    let echoed = serde_json::to_value(&response.tools).expect("tools echo");
    assert_eq!(
        echoed,
        json!([{"type": "tool_search", "description": null, "parameters": null}, deferred_weather()])
    );

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(tool_names(&requests[0]), ["tool_search"], "get_weather is withheld");
    assert_eq!(requests[0]["tools"][0]["parameters"]["required"], json!(["paths"]));
    assert_eq!(tool_names(&requests[1]), ["tool_search", "get_weather"]);
    // The second round sees the call and output under the call's public id.
    let input = requests[1]["input"].as_array().expect("item input");
    let (call, result) = (&input[input.len() - 2], &input[input.len() - 1]);
    assert_eq!(call["type"], "function_call");
    assert_eq!(call["call_id"], "tsc_s1");
    assert_eq!(result["type"], "function_call_output");
    assert_eq!(result["call_id"], "tsc_s1");
    let loaded: Value = serde_json::from_str(result["output"].as_str().expect("text output")).expect("JSON output");
    assert_eq!(loaded["tools"][0]["name"], "get_weather");
}

#[tokio::test]
async fn blocking_search_with_a_query_and_no_match_returns_an_empty_output() {
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call(
                "s1",
                "tool_search",
                &json!({"paths": ["get_weather", "no_such_tool"], "query": "timezone"})
            )]),
        ),
        upstream("up_2", json!([message("none")])),
    ])
    .await;
    let response = run(
        request(
            &json!("find a tool"),
            &json!([{"type": "tool_search"}, deferred_weather()]),
        ),
        context(llm_url),
    )
    .await;
    let items = output(&response);
    assert_eq!(items[1]["type"], "tool_search_output");
    assert_eq!(items[1]["tools"], json!([]));
    assert_eq!(items[2]["type"], "message");
    assert_eq!(tool_names(&requests.lock().await[1]), ["tool_search"]);
}

fn travel() -> Value {
    let member = |name: &str, description: &str| {
        json!({
            "type": "function", "name": name, "description": description,
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"],
                "additionalProperties": false},
            "strict": true, "defer_loading": true
        })
    };
    json!({
        "type": "namespace", "name": "travel", "description": "Travel location tools",
        "tools": [
            member("get_timezone", "Get the IANA time zone for a city"),
            member("get_coordinates", "Get latitude and longitude for a city")
        ]
    })
}

#[tokio::test]
async fn query_loads_one_namespace_member_and_its_call_keeps_the_namespace() {
    let member = model_visible_namespace_member_name("travel", "get_timezone");
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call(
                "s1",
                "tool_search",
                &json!({"paths": ["travel"], "query": "timezone"})
            )]),
        ),
        upstream("up_2", json!([function_call("t1", &member, &json!({"city": "Paris"}))])),
    ])
    .await;
    let response = run(
        request(
            &json!("time zone of Paris?"),
            &json!([{"type": "tool_search"}, travel()]),
        ),
        context(llm_url),
    )
    .await;
    let items = output(&response);
    assert_eq!(items[1]["tools"][0]["name"], "travel");
    assert_eq!(items[1]["tools"][0]["tools"].as_array().map(Vec::len), Some(1));
    assert_eq!(items[1]["tools"][0]["tools"][0]["name"], "get_timezone");
    assert_eq!(items[2]["type"], "function_call");
    assert_eq!(items[2]["namespace"], "travel");
    assert_eq!(items[2]["name"], "get_timezone");

    let requests = requests.lock().await;
    assert_eq!(tool_names(&requests[0]), ["tool_search"]);
    assert_eq!(tool_names(&requests[1]), ["tool_search", member.as_str()]);
}

#[tokio::test]
async fn validation_matches_the_reference_where_the_gateway_can() {
    let context = context("http://127.0.0.1:9".to_owned());
    let error = |input: Value| {
        let context = Arc::clone(&context);
        async move {
            let request: RequestPayload = serde_json::from_value(input).expect("request parses");
            match Box::pin(ExecuteRequest::new(request, context).run()).await {
                Ok(_) => panic!("request must be rejected"),
                Err(error) => (error.http_status().as_u16(), error.to_string()),
            }
        }
    };
    let (status, message) = error(json!({"model": "test", "input": "x", "tools": [{"type": "tool_search"}]})).await;
    assert_eq!(status, 400);
    assert!(
        message.contains("tools.tool_search requires at least one deferred tool"),
        "{message}"
    );

    let (status, message) = error(json!({"model": "test", "input": "x", "tools": [deferred_weather()]})).await;
    assert_eq!(status, 400);
    assert!(
        message.contains("defer_loading requires a tool_search declaration"),
        "{message}"
    );

    let (status, message) = error(json!({"model": "test", "input": "x",
        "tools": [{"type": "tool_search"}, deferred_weather()],
        "tool_choice": {"type": "function", "name": "get_weather"}}))
    .await;
    assert_eq!(status, 400);
    assert!(message.contains("before its definition is loaded"), "{message}");

    let (status, message) = error(json!({"model": "test", "input": "x", "tools": [
        {"type": "tool_search", "execution": "client"},
        {"type": "mcp", "server_label": "docs", "server_url": "http://127.0.0.1:9/mcp", "defer_loading": true}
    ]}))
    .await;
    assert_eq!(status, 400);
    assert!(
        message.contains("deferred MCP servers require hosted tool_search"),
        "{message}"
    );

    // The Responses API fails a deferred MCP tool_choice with a 500. The gateway does not support an MCP
    // tool_choice at all, deferred or not, and says so with a typed 400 naming the parameter.
    for deferred in [true, false] {
        let mut server = json!({"type": "mcp", "server_label": "docs", "server_url": "http://127.0.0.1:9/mcp",
            "require_approval": "never"});
        if deferred {
            server["defer_loading"] = json!(true);
        }
        let payload: RequestPayload = serde_json::from_value(json!({"model": "test", "input": "x",
            "tools": [{"type": "tool_search"}, server, deferred_weather()],
            "tool_choice": {"type": "mcp", "server_label": "docs", "name": "search"}}))
        .expect("an MCP tool_choice parses");
        let Err(error) = Box::pin(ExecuteRequest::new(payload, Arc::clone(&context)).run()).await else {
            panic!("an MCP tool_choice is rejected")
        };
        assert_eq!(error.http_status().as_u16(), 400);
        let body: Value = serde_json::from_slice(&error.into_response_body()).expect("error body");
        assert_eq!(
            body,
            json!({"error": {"message": "MCP tool_choice is not supported; use `auto`, `required`, or a function `tool_choice` instead",
                "type": "invalid_request_error", "code": "unsupported_value", "param": "tool_choice"}}),
            "deferred={deferred}"
        );
    }
}

#[tokio::test]
async fn parallel_tool_calls_is_allowed_for_hosted_search() {
    let (llm_url, _) = spawn_llm(vec![upstream("up_1", json!([message("ok")]))]).await;
    let mut payload = request(&json!("hi"), &json!([{"type": "tool_search"}, deferred_weather()]));
    payload.parallel_tool_calls = Some(true);
    let response = run(payload, context(llm_url)).await;
    assert_eq!(output(&response)[0]["type"], "message");
}

/// A minimal streamable-HTTP MCP server with two tools; counts `tools/call`.
async fn spawn_mcp() -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let listings = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let listing_counter = Arc::clone(&listings);
    let app = Router::new().route(
        "/mcp",
        post(move |axum::Json(request): axum::Json<Value>| {
            let counter = Arc::clone(&counter);
            let listing_counter = Arc::clone(&listing_counter);
            async move {
                let id = request["id"].clone();
                if id.is_null() {
                    return axum::http::StatusCode::ACCEPTED.into_response();
                }
                let reply = match request["method"].as_str() {
                    Some("initialize") => json!({"result": {
                        "protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                        "serverInfo": {"name": "stub", "version": "0"}
                    }}),
                    Some("tools/list") => {
                        listing_counter.fetch_add(1, Ordering::SeqCst);
                        json!({"result": {"tools": [
                            {"name": "echo", "description": "Echo the input.",
                                "inputSchema": {"type": "object", "properties": {}}},
                            {"name": "reverse", "description": "Reverse a string.",
                                "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}
                        ]}})
                    }
                    Some("tools/call") => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        json!({"result": {"content": [{"type": "text", "text": "echoed"}], "isError": false}})
                    }
                    _ => json!({"error": {"code": -32601, "message": "method not found"}}),
                };
                let mut body = json!({"jsonrpc": "2.0", "id": id});
                body.as_object_mut()
                    .expect("object")
                    .extend(reply.as_object().expect("object").clone());
                axum::Json(body).into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind MCP");
    let address = listener.local_addr().expect("MCP address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("MCP server") });
    (format!("http://{address}/mcp"), calls, listings)
}

fn deferred_mcp(url: &str, description: Option<&str>) -> Value {
    let mut server = json!({"type": "mcp", "server_label": "stub", "server_url": url,
        "require_approval": "never", "defer_loading": true});
    if let Some(description) = description {
        server["server_description"] = json!(description);
    }
    server
}

#[tokio::test]
async fn deferred_mcp_server_is_listed_searched_as_a_namespace_and_called() {
    let (mcp_url, mcp_calls, _) = spawn_mcp().await;
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call(
                "s1",
                "tool_search",
                &json!({"paths": ["mcp_stub"], "query": "echo"})
            )]),
        ),
        upstream("up_2", json!([function_call("m1", "mcp__stub__echo", &json!({}))])),
        upstream("up_3", json!([message("done")])),
    ])
    .await;
    let response = run(
        request(
            &json!("echo something"),
            &json!([{"type": "tool_search"}, deferred_mcp(&mcp_url, Some("Echo tools."))]),
        ),
        context(llm_url),
    )
    .await;
    let items = output(&response);
    let kinds: Vec<_> = items
        .iter()
        .map(|item| item["type"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        kinds,
        [
            "mcp_list_tools",
            "tool_search_call",
            "tool_search_output",
            "mcp_call",
            "message"
        ]
    );
    assert_eq!(
        items[2]["tools"],
        json!([{"type": "namespace", "name": "mcp_stub", "description": "Echo tools.", "tools": [{
            "type": "function", "name": "echo", "description": "Echo the input.",
            "parameters": {"type": "object", "properties": {}},
            "defer_loading": true, "output_schema": null
        }]}])
    );
    assert_eq!(items[3]["name"], "echo");
    assert_eq!(items[3]["server_label"], "stub");
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 1);

    let requests = requests.lock().await;
    assert_eq!(
        tool_names(&requests[0]),
        ["tool_search"],
        "deferred MCP tools are withheld"
    );
    assert_eq!(tool_names(&requests[1]), ["tool_search", "mcp__stub__echo"]);
    assert_eq!(tool_names(&requests[2]), ["tool_search", "mcp__stub__echo"]);
}

/// Return the gateway's actual public output as full history, including the hosted search pairs and MCP calls.
/// Catalog loads and retained discovery must survive both stateless execution paths without a new tools/list.
#[tokio::test]
async fn stateless_hosted_mcp_search_preserves_loads_and_discovery_across_turns() {
    for multi_agent in [false, true] {
        for stream in [false, true] {
            let case = format!("multi_agent={multi_agent}, stream={stream}");
            let (mcp_url, calls, listings) = spawn_mcp().await;
            let rounds = vec![
                vec![function_call(
                    "s1",
                    "tool_search",
                    &json!({"paths":["mcp_stub"], "query":"echo"}),
                )],
                vec![function_call("m1", "mcp__stub__echo", &json!({}))],
                vec![message("first")],
                vec![function_call("s2", "tool_search", &json!({"paths":["mcp_stub"]}))],
                vec![function_call("m2", "mcp__stub__reverse", &json!({"text":"hello"}))],
                vec![message("second")],
            ];
            let (llm_url, requests) = spawn_llm(
                rounds
                    .into_iter()
                    .enumerate()
                    .map(|(i, items)| upstream(&format!("up_{i}"), json!(items)))
                    .collect(),
            )
            .await;
            // Disabled stores also ensure neither request can depend on durable history or agent-tree state.
            let context = context(llm_url);
            let mut server = deferred_mcp(&mcp_url, Some("Echo and reverse tools."));
            server["allowed_tools"] = json!(["echo", "reverse"]);
            let tools = json!([{"type":"tool_search"}, server]);
            let mut history = vec![json!({"role":"user", "content":"Echo something."})];
            for turn in 0..2 {
                let mut payload = request(&json!(history), &tools);
                payload.stream = stream;
                if multi_agent {
                    payload.multi_agent = Some(serde_json::from_value(json!({"enabled":true})).unwrap());
                }
                let response = run(payload, Arc::clone(&context)).await;
                assert!(!response.standard_fields.store, "{case}, turn={turn}");
                assert_eq!(response.status, "completed", "{case}, turn={turn}");
                let items = output(&response);
                let count = |kind: &str| items.iter().filter(|item| item["type"] == kind).count();
                assert_eq!(
                    count("mcp_list_tools"),
                    usize::from(turn == 0),
                    "{case}, turn={turn}: {items:?}"
                );
                assert_eq!(count("tool_search_call"), 1, "{case}, turn={turn}: {items:?}");
                assert_eq!(count("tool_search_output"), 1, "{case}, turn={turn}: {items:?}");
                assert_eq!(count("mcp_call"), 1, "{case}, turn={turn}: {items:?}");
                let loaded = items.iter().find(|item| item["type"] == "tool_search_output").unwrap();
                let members = loaded["tools"][0]["tools"].as_array().unwrap();
                assert_eq!(members.len(), 1, "{case}, turn={turn}");
                assert_eq!(members[0]["name"], if turn == 0 { "echo" } else { "reverse" }, "{case}");
                assert!(
                    items
                        .iter()
                        .filter(|item| item["type"] == "mcp_call")
                        .all(|item| item["status"] == "completed"),
                    "{case}: {items:?}"
                );
                history.extend(items);
                history.push(json!({"role":"user", "content":"Now reverse hello."}));
            }
            assert_eq!(listings.load(Ordering::SeqCst), 1, "{case}: discovery must be reused");
            assert_eq!(calls.load(Ordering::SeqCst), 2, "{case}: both loaded tools execute");
            let requests = requests.lock().await;
            assert_eq!(requests.len(), 6, "{case}");
            for (round, request) in requests.iter().enumerate() {
                let names = tool_names(request);
                assert_eq!(
                    names.contains(&"mcp__stub__echo".to_owned()),
                    round > 0,
                    "{case}, round={round}: {names:?}"
                );
                assert_eq!(
                    names.contains(&"mcp__stub__reverse".to_owned()),
                    round > 3,
                    "{case}, round={round}: {names:?}"
                );
                let input = request["input"].as_array().unwrap();
                for call in input.iter().filter(|item| item["type"] == "function_call") {
                    let results = input
                        .iter()
                        .filter(|item| item["type"] == "function_call_output" && item["call_id"] == call["call_id"])
                        .count();
                    assert_eq!(results, 1, "{case}, round={round}: {call:?}");
                }
                assert!(
                    input.iter().all(|item| !matches!(
                        item["type"].as_str(),
                        Some("mcp_call" | "mcp_list_tools" | "tool_search_call" | "tool_search_output")
                    )),
                    "{case}: public orchestration items must be handled before inference"
                );
            }
        }
    }
}

#[tokio::test]
async fn deferred_mcp_without_description_gets_an_empty_namespace_description() {
    let (mcp_url, _, _) = spawn_mcp().await;
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call("s1", "tool_search", &json!({"paths": ["mcp_stub"]}))]),
        ),
        upstream("up_2", json!([message("done")])),
    ])
    .await;
    let response = run(
        request(
            &json!("x"),
            &json!([{"type": "tool_search"}, deferred_mcp(&mcp_url, None)]),
        ),
        context(llm_url),
    )
    .await;
    let items = output(&response);
    assert_eq!(items[2]["tools"][0]["description"], "");
    assert_eq!(items[2]["tools"][0]["tools"].as_array().map(Vec::len), Some(2));
    // The loaded members are callable in the next round, description or not.
    assert_eq!(
        tool_names(&requests.lock().await[1]),
        ["tool_search", "mcp__stub__echo", "mcp__stub__reverse"]
    );
}

/// Scripted root/child inference decisions; gateway orchestration and tool execution run normally.
fn child_hosted_search_output(body: &Value) -> Vec<Value> {
    let input = body["input"].as_array().unwrap();
    let result = |id: &str| {
        input
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == id)
    };
    let guidance = input.last().unwrap()["content"].as_str().unwrap();
    if guidance.contains("You are `/root/seed_") {
        return vec![message(&format!(
            "Seed task complete: {}",
            guidance.split('`').nth(1).unwrap()
        ))];
    }
    if input.iter().any(|item| item["content"] == "Complete three seed tasks.")
        && !input.iter().any(|item| item["content"] == "Delegate the MCP job.")
    {
        if !tool_names(body).contains(&"approve".to_owned()) {
            return vec![function_call("seed_search", "tool_search", &json!({"query":"approve"}))];
        }
        if input
            .iter()
            .filter(|item| {
                item["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("Message Type: FINAL_ANSWER"))
            })
            .count()
            == 3
        {
            return vec![message("SEED_TASKS_DONE")];
        }
        if result("call_seed_one") {
            return vec![function_call(
                &format!("seed_wait_{}", input.len()),
                "wait_agent",
                &json!({"timeout_ms":10000}),
            )];
        }
        return ["one", "two", "three"]
            .into_iter()
            .map(|name| {
                function_call(
                    &format!("seed_{name}"),
                    "spawn_agent",
                    &json!({
                        "task_name":format!("seed_{name}"), "message":"Complete the seed task.", "fork_turns":"none"
                    }),
                )
            })
            .collect();
    }
    let child = guidance.contains("You are `/root/worker`");
    if child {
        if result("call_approve") {
            vec![message("CHILD_HOSTED_DONE")]
        } else if result("call_echo") {
            vec![function_call("approve", "approve", &json!({}))]
        } else if tool_names(body).contains(&"mcp__stub__echo".to_owned()) {
            vec![function_call("echo", "mcp__stub__echo", &json!({}))]
        } else {
            vec![function_call(
                "search",
                "tool_search",
                &json!({"paths":["mcp_stub"], "query":"echo"}),
            )]
        }
    } else if input.iter().any(|item| {
        item["content"]
            .as_str()
            .is_some_and(|text| text.contains("Message Type: FINAL_ANSWER") && text.contains("CHILD_HOSTED_DONE"))
    }) {
        vec![message("ROOT_HOSTED_DONE")]
    } else if result("call_spawn") {
        vec![function_call(
            &format!("wait_{}", input.len()),
            "wait_agent",
            &json!({"timeout_ms":10000}),
        )]
    } else {
        vec![function_call(
            "spawn",
            "spawn_agent",
            &json!({"task_name":"worker", "message":"Echo with MCP, then request approval.", "fork_turns":"none"}),
        )]
    }
}

#[tokio::test]
async fn stateless_client_search_exposes_a_partially_loaded_namespace_to_the_model() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes");
    let capture: Value = serde_yaml::from_str(
        &fs::read_to_string(
            directory.join("multi_agent/multi-agent-openai-reference-client-owned-tools-gpt-5.6-sol-nonstreaming.yaml"),
        )
        .unwrap(),
    )
    .unwrap();
    let tools = capture["turns"][0]["request"]["body"]["tools"].clone();
    let returned: Value =
        serde_json::from_str(&fs::read_to_string(directory.join("tool_search/returned_tools.json")).unwrap()).unwrap();
    for multi_agent in [false, true] {
        for stream in [false, true] {
            let (llm_url, requests) = spawn_llm(vec![
                upstream(
                    "up_search",
                    json!([function_call("search", "tool_search", &json!({"query":"time zone"}))]),
                ),
                upstream(
                    "up_timezone",
                    json!([function_call(
                        "timezone",
                        "agentic_ns__travel__get_timezone",
                        &json!({"city":"Paris"})
                    )]),
                ),
                upstream("up_done", json!([message("Europe/Paris")])),
            ])
            .await;
            let context = context(llm_url);
            let mut payload = request(&json!("Find the time zone for Paris."), &tools);
            payload.stream = stream;
            payload.multi_agent = multi_agent.then(|| serde_json::from_value(json!({"enabled":true})).unwrap());
            let mut history = vec![json!({"role":"user", "content":"Find the time zone for Paris."})];
            let first = run(payload.clone(), Arc::clone(&context)).await;
            let search = output(&first)
                .into_iter()
                .find(|item| item["type"] == "tool_search_call")
                .unwrap();
            history.extend(output(&first));
            history.push(json!({"type":"tool_search_output", "call_id":search["call_id"], "execution":"client", "status":"completed", "tools":returned}));
            payload.input = serde_json::from_value(json!(history)).unwrap();
            let second = run(payload.clone(), Arc::clone(&context)).await;
            let call = output(&second)
                .into_iter()
                .find(|item| item["type"] == "function_call")
                .unwrap();
            assert_eq!(call["name"], "get_timezone");
            assert_eq!(call["namespace"], "travel");
            history.extend(output(&second));
            history.push(json!({"type":"function_call_output", "call_id":call["call_id"], "output":"Europe/Paris"}));
            payload.input = serde_json::from_value(json!(history)).unwrap();
            run(payload, Arc::clone(&context)).await;
            let requests = requests.lock().await;
            for upstream in &requests[1..] {
                let timezone = upstream["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|tool| tool["name"] == "agentic_ns__travel__get_timezone")
                    .expect("the loaded namespace member must be offered to inference");
                assert_eq!(timezone["parameters"]["properties"]["city"]["type"], "string");
                assert!(!tool_names(upstream).contains(&"agentic_ns__travel__get_coordinates".to_owned()));
            }
        }
    }
}

async fn spawn_child_hosted_search_llm() -> (Arc<ExecutionContext>, Requests, JoinHandle<()>) {
    let requests: Requests = Arc::default();
    let captured = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |axum::Json(body): axum::Json<Value>| {
            let captured = Arc::clone(&captured);
            async move {
                captured.lock().await.push(body.clone());
                let items = child_hosted_search_output(&body);
                if body["stream"] == true {
                    ([("Content-Type", "text/event-stream")], sse("up_child", &items)).into_response()
                } else {
                    axum::Json(upstream("up_child", json!(items))).into_response()
                }
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let context = context(format!("http://{}", listener.local_addr().unwrap()));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (context, requests, task)
}

#[tokio::test]
async fn stateless_child_hosted_search_survives_a_client_tool_continuation() {
    for stream in [false, true] {
        let (mcp_url, calls, listings) = spawn_mcp().await;
        let (context, requests, task) = spawn_child_hosted_search_llm().await;
        let mut server = deferred_mcp(&mcp_url, None);
        server["allowed_tools"] = json!(["echo", "reverse"]);
        let approve = json!({"type":"function", "name":"approve", "defer_loading":true,
            "parameters":{"type":"object", "properties":{}}});
        let tools = json!([{"type":"tool_search"}, server, approve]);
        let mut seed = request(
            &json!("Complete three seed tasks."),
            &json!([
                {"type":"tool_search", "execution":"client"}, approve
            ]),
        );
        seed.stream = stream;
        seed.multi_agent = Some(serde_json::from_value(json!({"enabled":true, "max_concurrent_subagents":3})).unwrap());
        let first = run(seed.clone(), Arc::clone(&context)).await;
        let mut history = vec![json!({"role":"user", "content":"Complete three seed tasks."})];
        let search = output(&first)
            .into_iter()
            .find(|item| item["type"] == "tool_search_call")
            .unwrap();
        history.extend(output(&first));
        history.push(json!({"type":"tool_search_output", "call_id":search["call_id"],
            "execution":"client", "status":"completed", "tools":[approve]}));
        seed.input = serde_json::from_value(json!(history)).unwrap();
        let seed_items = output(&run(seed, Arc::clone(&context)).await);
        assert_eq!(
            seed_items
                .iter()
                .filter(|item| item["type"] == "message" && item["phase"] == "final_answer")
                .count(),
            4
        );
        history.extend(seed_items);
        history.push(json!({"role":"user", "content":"Delegate the MCP job."}));
        for turn in 0..2 {
            let mut payload = request(&json!(history), &tools);
            payload.stream = stream;
            payload.multi_agent = Some(serde_json::from_value(json!({"enabled":true})).unwrap());
            let response = run(payload, Arc::clone(&context)).await;
            let items = output(&response);
            if turn == 0 {
                for kind in ["tool_search_call", "tool_search_output", "mcp_call", "function_call"] {
                    let item = items.iter().find(|item| item["type"] == kind).unwrap();
                    assert_eq!(item["agent"]["agent_name"], "/root/worker", "{items:?}");
                }
                assert!(
                    items
                        .iter()
                        .any(|item| item["type"] == "function_call" && item["call_id"] == "call_approve")
                );
            } else {
                assert!(
                    items.iter().any(|item| item["type"] == "message"
                        && item["agent"]["agent_name"] == "/root"
                        && item["content"][0]["text"] == "ROOT_HOSTED_DONE"),
                    "{items:?}"
                );
                assert!(
                    items.iter().all(|item| !matches!(
                        item["type"].as_str(),
                        Some("tool_search_call" | "mcp_call" | "mcp_list_tools")
                    )),
                    "{items:?}"
                );
            }
            history.extend(items);
            history.push(json!({"type":"function_call_output", "call_id":"call_approve", "output":"approved"}));
        }
        assert_eq!(listings.load(Ordering::SeqCst), 1, "stream={stream}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "stream={stream}");
        let requests = requests.lock().await;
        let mut resumed_child = false;
        for body in requests.iter() {
            let input = body["input"].as_array().unwrap();
            let child = input.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("You are `/root/worker`");
            let loaded = tool_names(body).contains(&"mcp__stub__echo".to_owned());
            if !child {
                assert!(!loaded, "a child's search must not load tools for the root");
            } else if input
                .iter()
                .any(|item| item["call_id"] == "call_approve" && item["type"] == "function_call_output")
            {
                assert!(loaded, "the resumed child retains its loaded MCP tool");
                resumed_child = true;
            }
        }
        assert!(resumed_child, "the client result resumes the child");
        task.abort();
    }
}

#[tokio::test]
async fn stateless_tool_search_switches_from_client_to_hosted_mcp_with_full_history() {
    for stream in [false, true] {
        let (mcp_url, calls, listings) = spawn_mcp().await;
        let (llm_url, requests) = spawn_llm(vec![
            upstream(
                "up_client_search",
                json!([function_call(
                    "client_search",
                    "tool_search",
                    &json!({"query":"weather"})
                )]),
            ),
            upstream("up_client_done", json!([message("Client discovery complete.")])),
            upstream(
                "up_hosted_search",
                json!([function_call(
                    "hosted_search",
                    "tool_search",
                    &json!({"paths":["mcp_stub"], "query":"echo"})
                )]),
            ),
            upstream("up_mcp", json!([function_call("echo", "mcp__stub__echo", &json!({}))])),
            upstream("up_hosted_done", json!([message("Hosted MCP complete.")])),
        ])
        .await;
        let context = context(llm_url);
        let mut payload = request(
            &json!("Discover weather."),
            &json!([
                {"type":"tool_search", "execution":"client"}, deferred_weather()
            ]),
        );
        payload.stream = stream;
        payload.multi_agent = Some(serde_json::from_value(json!({"enabled":true})).unwrap());
        let mut history = vec![json!({"role":"user", "content":"Discover weather."})];
        let first = run(payload.clone(), Arc::clone(&context)).await;
        let search = output(&first)
            .into_iter()
            .find(|item| item["type"] == "tool_search_call")
            .unwrap();
        assert_eq!(search["execution"], "client");
        history.extend(output(&first));
        history.push(json!({"type":"tool_search_output", "call_id":search["call_id"],
            "execution":"client", "status":"completed", "tools":[deferred_weather()]}));
        payload.input = serde_json::from_value(json!(history)).unwrap();
        let completed = run(payload.clone(), Arc::clone(&context)).await;
        history.extend(output(&completed));
        history.push(json!({"role":"user", "content":"Continue with hosted MCP search."}));
        let mut server = deferred_mcp(&mcp_url, None);
        server["allowed_tools"] = json!(["echo", "reverse"]);
        payload.tools = Some(
            serde_json::from_value(json!([
                {"type":"tool_search"}, deferred_weather(), server
            ]))
            .unwrap(),
        );
        payload.input = serde_json::from_value(json!(history)).unwrap();
        let completed = run(payload, Arc::clone(&context)).await;
        let items = output(&completed);
        let hosted = items.iter().find(|item| item["type"] == "tool_search_call").unwrap();
        assert_eq!(hosted["execution"], "server");
        assert!(hosted["call_id"].is_null());
        assert!(
            items
                .iter()
                .any(|item| item["type"] == "mcp_call" && item["status"] == "completed")
        );
        assert_eq!(listings.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 5);
        for request in &requests[2..] {
            assert!(
                tool_names(request).contains(&"get_weather".to_owned()),
                "client loads must survive the switch"
            );
            assert!(
                request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["type"] == "function_call" && item["call_id"] == search["call_id"])
            );
        }
        assert!(!tool_names(&requests[2]).contains(&"mcp__stub__echo".to_owned()));
        assert!(tool_names(&requests[3]).contains(&"mcp__stub__echo".to_owned()));
    }
}

#[tokio::test]
async fn search_does_not_count_toward_max_tool_calls() {
    let (mcp_url, mcp_calls, _) = spawn_mcp().await;
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call("s1", "tool_search", &json!({"paths": ["mcp_stub"]}))]),
        ),
        upstream("up_2", json!([function_call("m1", "mcp__stub__echo", &json!({}))])),
        upstream(
            "up_3",
            json!([function_call("s2", "tool_search", &json!({"paths": ["mcp_stub"]}))]),
        ),
        upstream("up_4", json!([message("done")])),
    ])
    .await;
    let payload: RequestPayload = serde_json::from_value(json!({"model": "test", "input": "x", "store": false,
        "max_tool_calls": 1, "tools": [{"type": "tool_search"}, deferred_mcp(&mcp_url, None)]}))
    .expect("request");
    let response = run(payload, context(llm_url)).await;
    let kinds: Vec<_> = output(&response)
        .iter()
        .map(|item| item["type"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        kinds,
        [
            "mcp_list_tools",
            "tool_search_call",
            "tool_search_output",
            "mcp_call",
            "tool_search_call",
            "tool_search_output",
            "message"
        ]
    );
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 1);
    // After the call budget is spent, built-in tools are withheld but search stays.
    assert_eq!(tool_names(&requests.lock().await[3])[0], "tool_search");
}

const LISTING_FAILED: &str =
    "Error retrieving tool list from MCP server: 'stub'. Http status code: 424 (Failed Dependency)";

/// Recorded reference: any failed MCP tool listing fails the request, deferred or not.
#[tokio::test]
async fn unreachable_mcp_server_fails_the_request_like_openai() {
    for deferred in [true, false] {
        let (llm_url, requests) = spawn_llm(Vec::new()).await;
        let mut server = deferred_mcp("http://127.0.0.1:9/mcp", None);
        if !deferred {
            server.as_object_mut().expect("object").remove("defer_loading");
        }
        let payload = request(
            &json!("weather?"),
            &json!([{"type": "tool_search"}, server, deferred_weather()]),
        );
        let Err(error) = Box::pin(ExecuteRequest::new(payload, context(llm_url)).run()).await else {
            panic!("a failed MCP listing fails the request")
        };
        assert_eq!(error.http_status().as_u16(), 424, "deferred={deferred}");
        let body: Value = serde_json::from_slice(&error.into_response_body()).expect("error body");
        assert_eq!(
            body,
            json!({"error": {"message": LISTING_FAILED, "type": "external_connector_error",
                "code": "http_error", "param": "tools"}})
        );
        assert!(requests.lock().await.is_empty(), "the model is never called");
    }
}

/// Recorded reference (`mcp-failures`): an `allowed_tools` filter that matches nothing is not a failure. The
/// listing is empty and the request completes.
#[tokio::test]
async fn allowed_tools_matching_nothing_lists_no_tools_and_completes() {
    let (mcp_url, _, _) = spawn_mcp().await;
    let (llm_url, requests) = spawn_llm(vec![
        upstream("up_1", json!([message("first")])),
        upstream("up_2", json!([message("second")])),
    ])
    .await;
    for deferred in [true, false] {
        let mut server = deferred_mcp(&mcp_url, None);
        server["allowed_tools"] = json!(["no_such_tool"]);
        if !deferred {
            server.as_object_mut().expect("object").remove("defer_loading");
        }
        let tools = if deferred {
            json!([{"type": "tool_search"}, server, deferred_weather()])
        } else {
            json!([server])
        };
        let response = run(request(&json!("weather?"), &tools), context(llm_url.clone())).await;
        assert_eq!(response.status, "completed", "deferred={deferred}");
        let items = output(&response);
        assert_eq!(items[0]["type"], "mcp_list_tools");
        assert_eq!(items[0]["tools"], json!([]));
        assert!(items[0].get("error").is_none_or(Value::is_null));
    }
    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(
        tool_names(&requests[1]).iter().all(|name| !name.starts_with("mcp__")),
        "no MCP tool is offered"
    );
}

/// The Responses API does not list a server again while its listing is in context, so a continuation whose
/// server has since become unreachable is not failed by the gateway's fresh listing.
#[tokio::test]
async fn continuation_keeps_going_when_a_server_listed_earlier_fails() {
    let (mcp_url, _, _) = spawn_mcp().await;
    let (llm_url, requests) = spawn_llm(vec![
        upstream("up_1", json!([message("first")])),
        upstream("up_2", json!([message("second")])),
    ])
    .await;
    let context = durable_context(llm_url).await;
    let server = |url: &str| {
        json!({"type": "mcp", "server_label": "stub", "server_url": url,
        "require_approval": "never"})
    };
    let first: RequestPayload = serde_json::from_value(json!({"model": "test", "input": "x", "store": true,
        "tools": [server(&mcp_url)]}))
    .expect("first");
    let first = run(first, Arc::clone(&context)).await;
    assert_eq!(output(&first)[0]["type"], "mcp_list_tools");
    let second: RequestPayload = serde_json::from_value(json!({"model": "test", "input": "more", "store": true,
        "previous_response_id": first.id, "tools": [server("http://127.0.0.1:9/mcp")]}))
    .expect("second");
    let second = run(second, Arc::clone(&context)).await;
    assert_eq!(second.status, "completed");
    assert_eq!(
        requests.lock().await.len(),
        2,
        "the model is called for the continuation"
    );
}

#[tokio::test]
async fn streaming_unreachable_mcp_server_ends_with_error_and_response_failed() {
    let (llm_url, requests) = spawn_streaming_llm(Vec::new()).await;
    let mut server = deferred_mcp("http://127.0.0.1:9/mcp", None);
    server["authorization"] = json!("secret-token");
    let mut payload = request(
        &json!("weather?"),
        &json!([{"type": "tool_search"}, server, deferred_weather()]),
    );
    payload.stream = true;
    let events = run_streaming(payload, context(llm_url)).await;
    let kinds: Vec<_> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap_or_default())
        .collect();
    // The recorded reference sequence.
    assert_eq!(
        kinds,
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.mcp_list_tools.in_progress",
            "response.mcp_list_tools.failed",
            "error",
            "response.failed"
        ]
    );
    let sequence: Vec<_> = events
        .iter()
        .filter_map(|event| event["sequence_number"].as_u64())
        .collect();
    assert_eq!(sequence, (0..7).collect::<Vec<_>>());
    assert_eq!(events[2]["item"]["type"], "mcp_list_tools");
    assert_eq!(events[2]["item"]["server_label"], "stub");
    assert_eq!(events[2]["item"]["tools"], json!([]));
    assert_eq!(events[4]["item_id"], events[2]["item"]["id"]);
    let error = &events[5]["error"];
    assert_eq!(error["type"], "external_connector_error");
    assert_eq!(error["code"], "http_error");
    assert_eq!(error["param"], "tools");
    assert_eq!(error["message"], LISTING_FAILED);
    let failed = &events[6]["response"];
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"]["type"], "external_connector_error");
    assert_eq!(failed["error"]["param"], "tools");
    assert_eq!(failed["output"][0]["type"], "mcp_list_tools");
    assert!(failed["output"][0].get("error").is_none());
    // Every lifecycle frame echoes the public declarations, without request credentials.
    for frame in [&events[0]["response"], failed] {
        let tools: Vec<_> = frame["tools"]
            .as_array()
            .expect("tools echo")
            .iter()
            .map(|tool| tool["type"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(tools, ["tool_search", "mcp", "function"]);
        assert!(frame["tools"][1].get("authorization").is_none());
    }
    assert!(requests.lock().await.is_empty());
}

/// The upstream input of the last request, with each lowered call's `(name, call_id)` and each output's `call_id`.
async fn lowered_calls(requests: &tokio::sync::Mutex<Vec<Value>>) -> (Vec<(String, String)>, Vec<String>) {
    let requests = requests.lock().await;
    let input = requests.last().expect("replay request")["input"]
        .as_array()
        .expect("upstream input")
        .clone();
    assert!(
        input
            .iter()
            .all(|item| item["type"] != "mcp_call" && item["type"] != "tool_search_call"),
        "{input:?}"
    );
    let calls = input
        .iter()
        .filter(|item| item["type"] == "function_call")
        .map(|item| {
            (
                item["name"].as_str().unwrap_or_default().to_owned(),
                item["call_id"].to_string(),
            )
        })
        .collect();
    let outputs = input
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["call_id"].to_string())
        .collect();
    (calls, outputs)
}

/// A multi-agent response stores its hosted pair as output; a client replaying that output still loads the searched
/// function.
#[tokio::test]
async fn a_stored_multi_agent_search_replayed_as_input_loads_its_function() {
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call("s1", "tool_search", &json!({"paths": ["get_weather"]}))]),
        ),
        upstream(
            "up_2",
            json!([function_call("w1", "get_weather", &json!({"city": "Paris"}))]),
        ),
        upstream("up_3", json!([message("18C")])),
    ])
    .await;
    let context = durable_context(llm_url).await;
    let tools = json!([{"type": "tool_search"}, deferred_weather()]);
    let first: RequestPayload = serde_json::from_value(json!({"model": "test", "input": "weather?", "store": true,
        "multi_agent": {"enabled": true}, "tools": tools}))
    .expect("stored request");
    let first_output = output(&run(first, Arc::clone(&context)).await);

    let mut replay = vec![json!({"type": "message", "role": "user", "content": "weather?"})];
    replay.extend(first_output);
    replay.push(json!({"type": "function_call_output", "call_id": "call_w1", "output": "18C"}));
    let replay: RequestPayload =
        serde_json::from_value(json!({"model": "test", "input": replay, "store": false, "tools": tools}))
            .expect("replay request");
    let second = run(replay, context).await;
    assert_eq!(second.status, "completed");

    let (calls, outputs) = lowered_calls(&requests).await;
    let (_, search_id) = calls
        .iter()
        .find(|(name, _)| name == "tool_search")
        .expect("lowered search");
    assert!(outputs.contains(search_id), "{calls:?} {outputs:?}");
}

#[tokio::test]
async fn recorded_openai_hosted_transcript_replays_as_input() {
    // Items copied from the OpenAI reference cassette (validation, explicit-server leg).
    let replay = json!([
        {"type": "message", "role": "user", "content": "What is the current weather in Paris? Use a tool."},
        {"arguments": {"paths": ["get_weather"]}, "call_id": null, "execution": "server",
            "id": "tsc_0d445f45e3823cf6006ad089333e2087d095793b5e495531f5", "status": "completed",
            "type": "tool_search_call"},
        {"call_id": null, "execution": "server", "id": "tso_0d445f45e3823cf6006ad089336c8487d0af0bc5717e26cb51",
            "status": "completed", "type": "tool_search_output", "tools": [{"defer_loading": true,
            "description": "Get the current weather for a city", "name": "get_weather", "output_schema": null,
            "parameters": {"additionalProperties": false, "properties": {"city": {"type": "string"}},
                "required": ["city"], "type": "object"}, "strict": true, "type": "function"}]},
        {"arguments": "{\"city\":\"Paris\"}", "call_id": "call_ceuITMhusqvlrrLpN8muaRTb",
            "id": "fc_0d445f45e3823cf6006ad08933d50087d08d8ab5e86527e73b", "name": "get_weather",
            "namespace": "get_weather", "status": "completed", "type": "function_call"},
        {"type": "function_call_output", "call_id": "call_ceuITMhusqvlrrLpN8muaRTb", "output": "18C, cloudy"}
    ]);
    let (llm_url, requests) = spawn_llm(vec![upstream("up_1", json!([message("It is 18C and cloudy.")]))]).await;
    let response = run(
        request(
            &replay,
            &json!([{"type": "tool_search", "execution": "server"}, deferred_weather()]),
        ),
        context(llm_url),
    )
    .await;
    assert_eq!(output(&response)[0]["type"], "message");

    let requests = requests.lock().await;
    assert_eq!(
        tool_names(&requests[0]),
        ["tool_search", "get_weather"],
        "the replayed load persists"
    );
    let input = requests[0]["input"].as_array().expect("item input");
    let kinds: Vec<_> = input
        .iter()
        .map(|item| item["type"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        kinds,
        [
            "message",
            "function_call",
            "function_call_output",
            "function_call",
            "function_call_output"
        ]
    );
    // The hosted pair is paired by the call's item id, and the searched function
    // is called unqualified, as the model saw it.
    assert_eq!(input[1]["call_id"], input[2]["call_id"]);
    assert_eq!(input[1]["name"], "tool_search");
    assert_eq!(input[3]["name"], "get_weather");
    assert!(input[3].get("namespace").is_none_or(Value::is_null), "{}", input[3]);
}

/// Upstream SSE for a response whose output is `items` (function calls and messages).
fn sse(id: &str, items: &[Value]) -> String {
    let mut events = vec![
        json!({"type": "response.created", "response": {"id": id, "status": "in_progress", "output": []}}),
        json!({"type": "response.in_progress", "response": {"id": id, "status": "in_progress", "output": []}}),
    ];
    for (index, item) in items.iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        if item["type"] == "function_call" {
            added["arguments"] = json!("");
            events.push(json!({"type": "response.output_item.added", "output_index": index, "item": added}));
            events.push(
                json!({"type": "response.function_call_arguments.delta", "output_index": index,
                "item_id": item["id"], "delta": item["arguments"]}),
            );
            events.push(
                json!({"type": "response.function_call_arguments.done", "output_index": index,
                "item_id": item["id"], "name": item["name"], "arguments": item["arguments"]}),
            );
        } else {
            added["content"] = json!([]);
            let text = item["content"][0]["text"].clone();
            let part = json!({"type": "output_text", "text": "", "annotations": []});
            events.push(json!({"type": "response.output_item.added", "output_index": index, "item": added}));
            events.push(json!({"type": "response.content_part.added", "output_index": index,
                "item_id": item["id"], "content_index": 0, "part": part}));
            events.push(json!({"type": "response.output_text.delta", "output_index": index,
                "item_id": item["id"], "content_index": 0, "delta": text}));
            events.push(json!({"type": "response.output_text.done", "output_index": index,
                "item_id": item["id"], "content_index": 0, "text": text}));
            events.push(json!({"type": "response.content_part.done", "output_index": index,
                "item_id": item["id"], "content_index": 0, "part": item["content"][0]}));
        }
        events.push(json!({"type": "response.output_item.done", "output_index": index, "item": item}));
    }
    events.push(json!({"type": "response.completed",
        "response": {"id": id, "status": "completed", "output": items, "usage": null}}));
    let mut body = String::new();
    for (sequence, mut event) in events.into_iter().enumerate() {
        event["sequence_number"] = json!(sequence);
        writeln!(
            body,
            "event: {}\ndata: {event}\n",
            event["type"].as_str().unwrap_or_default()
        )
        .expect("writing to a String cannot fail");
    }
    body
}

async fn spawn_streaming_llm(bodies: Vec<String>) -> (String, Requests) {
    let bodies = Arc::new(Mutex::new(VecDeque::from(bodies)));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (route_bodies, route_requests) = (Arc::clone(&bodies), Arc::clone(&requests));
    let app = Router::new().route(
        "/v1/responses",
        post(move |body: Bytes| {
            let (bodies, requests) = (Arc::clone(&route_bodies), Arc::clone(&route_requests));
            async move {
                requests
                    .lock()
                    .await
                    .push(serde_json::from_slice(&body).expect("request JSON"));
                let body = bodies.lock().await.pop_front().expect("one body per request");
                axum::response::Response::builder()
                    .status(200)
                    .header("Content-Type", "text/event-stream")
                    .body(axum::body::Body::from(body))
                    .expect("mock response")
                    .into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock LLM");
    let address = listener.local_addr().expect("mock address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("mock server") });
    (format!("http://{address}"), requests)
}

async fn run_streaming(request: RequestPayload, context: Arc<ExecutionContext>) -> Vec<Value> {
    let Either::Right(mut stream) = ExecuteRequest::new(request, context)
        .run()
        .await
        .expect("streaming execution")
    else {
        panic!("streaming request returned JSON")
    };
    let mut events = Vec::new();
    while let Some(chunk) = stream.next().await {
        for line in chunk.lines() {
            if let Some(data) = line.strip_prefix("data: ")
                && data != "[DONE]"
            {
                events.push(serde_json::from_str(data).expect("event JSON"));
            }
        }
    }
    events
}

#[tokio::test]
async fn streaming_search_has_only_added_and_done_events_in_output_order() {
    let search = function_call("s1", "tool_search", &json!({"paths": ["get_weather"]}));
    let weather = function_call("w1", "get_weather", &json!({"city": "Paris"}));
    let (llm_url, _) = spawn_streaming_llm(vec![sse("up_1", &[search]), sse("up_2", &[weather])]).await;
    let mut payload = request(
        &json!("weather?"),
        &json!([{"type": "tool_search"}, deferred_weather()]),
    );
    payload.stream = true;
    let events = run_streaming(payload, context(llm_url)).await;

    let sequence: Vec<_> = events
        .iter()
        .filter_map(|event| event["sequence_number"].as_u64())
        .collect();
    assert!(sequence.windows(2).all(|pair| pair[1] == pair[0] + 1), "{sequence:?}");
    assert!(
        events
            .iter()
            .all(|event| !event["type"].as_str().unwrap_or_default().contains("tool_search")),
        "no dedicated tool_search event types"
    );
    let items: Vec<(String, u64, String, String)> = events
        .iter()
        .filter(|event| event["item"].is_object())
        .map(|event| {
            (
                event["type"].as_str().unwrap_or_default().to_owned(),
                event["output_index"].as_u64().unwrap_or(u64::MAX),
                event["item"]["type"].as_str().unwrap_or_default().to_owned(),
                event["item"]["status"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let expect =
        |kind: &str, index: u64, item: &str, status: &str| (kind.to_owned(), index, item.to_owned(), status.to_owned());
    assert_eq!(
        items,
        [
            expect("response.output_item.added", 0, "tool_search_call", "in_progress"),
            expect("response.output_item.done", 0, "tool_search_call", "completed"),
            expect("response.output_item.added", 1, "tool_search_output", "in_progress"),
            expect("response.output_item.done", 1, "tool_search_output", "completed"),
            expect("response.output_item.added", 2, "function_call", "in_progress"),
            expect("response.output_item.done", 2, "function_call", "completed"),
        ]
    );
    let added_call = events
        .iter()
        .find(|event| event["item"]["type"] == "tool_search_call")
        .expect("search call added");
    assert_eq!(added_call["item"]["arguments"], json!({}));
    let added_output = events
        .iter()
        .find(|event| event["item"]["type"] == "tool_search_output")
        .expect("search output added");
    assert_eq!(added_output["item"]["tools"][0]["name"], "get_weather");

    let completed = events.last().expect("terminal event");
    assert_eq!(completed["type"], "response.completed");
    let output = completed["response"]["output"].as_array().expect("terminal output");
    assert_eq!(output.len(), 3);
    assert_eq!(output[2]["namespace"], "get_weather");
}

async fn durable_context(llm_url: String) -> Arc<ExecutionContext> {
    durable_context_with_conversations(llm_url).await.0
}

async fn durable_context_with_conversations(llm_url: String) -> (Arc<ExecutionContext>, ConversationStore) {
    static DATABASE: AtomicU64 = AtomicU64::new(0);
    let url = format!(
        "sqlite:file:hosted_tool_search_{}?mode=memory&cache=shared",
        DATABASE.fetch_add(1, Ordering::Relaxed)
    );
    let pool = create_pool_with_schema(Some(&url)).await.expect("storage pool");
    let conversations = ConversationStore::new(Arc::clone(&pool));
    let context = Arc::new(ExecutionContext::new(
        ConversationHandler::new(conversations.clone()),
        ResponseHandler::new(ResponseStore::new(pool)),
        Arc::new(reqwest::Client::new()),
        llm_url,
    ));
    (context, conversations)
}

#[tokio::test]
async fn stored_search_continues_by_previous_response_id_without_offering_loaded_tools_again() {
    let (mcp_url, _, _) = spawn_mcp().await;
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call(
                "s1",
                "tool_search",
                &json!({"paths": ["get_weather", "mcp_stub"], "query": "e"})
            )]),
        ),
        upstream(
            "up_2",
            json!([function_call("w1", "get_weather", &json!({"city": "Paris"}))]),
        ),
        upstream(
            "up_3",
            json!([function_call(
                "s2",
                "tool_search",
                &json!({"paths": ["get_weather", "mcp_stub"]})
            )]),
        ),
        upstream("up_4", json!([message("done")])),
    ])
    .await;
    let context = durable_context(llm_url).await;
    let tools = json!([{"type": "tool_search"}, deferred_weather(), deferred_mcp(&mcp_url, None)]);
    let first: RequestPayload =
        serde_json::from_value(json!({"model": "test", "input": "weather?", "tools": tools, "store": true}))
            .expect("first request");
    let first = run(first, Arc::clone(&context)).await;
    let first_items = output(&first);
    assert_eq!(first_items[0]["type"], "mcp_list_tools");
    // "e" matches get_weather and both MCP tools ("echo", "Reverse").
    assert_eq!(first_items[2]["tools"].as_array().map(Vec::len), Some(2));
    assert_eq!(first_items[3]["type"], "function_call");

    let second: RequestPayload = serde_json::from_value(json!({
        "model": "test", "store": true, "previous_response_id": first.id,
        "input": [{"type": "function_call_output", "call_id": "call_w1", "output": "18C"}]
    }))
    .expect("second request");
    let second = run(second, Arc::clone(&context)).await;
    let second_items = output(&second);
    let kinds: Vec<_> = second_items
        .iter()
        .map(|item| item["type"].as_str().unwrap_or_default())
        .collect();
    // No new MCP listing on a continuation, and nothing new to load.
    assert_eq!(kinds, ["tool_search_call", "tool_search_output", "message"]);
    assert_eq!(second_items[1]["tools"], json!([]));

    let requests = requests.lock().await;
    // The continuation restores the loads from the stored public pair.
    assert_eq!(
        tool_names(&requests[2]),
        ["tool_search", "get_weather", "mcp__stub__echo", "mcp__stub__reverse"]
    );
    let input = requests[2]["input"].as_array().expect("item input");
    let search = input
        .iter()
        .position(|item| item["name"] == "tool_search")
        .expect("projected search call");
    assert_eq!(input[search]["call_id"], "tsc_s1");
    assert_eq!(input[search + 1]["type"], "function_call_output");
    assert_eq!(input[search + 1]["call_id"], "tsc_s1");
    assert_eq!(
        input.iter().filter(|item| item["name"] == "tool_search").count(),
        1,
        "the stored pair appears once"
    );
}

#[tokio::test]
async fn conversation_and_session_continuations_restore_hosted_loads() {
    for use_session in [false, true] {
        for store in [false, true] {
            let case = format!("session={use_session}, store={store}");
            let (llm_url, requests) = spawn_llm(vec![
                upstream(
                    "up_1",
                    json!([function_call("s1", "tool_search", &json!({"paths": ["get_weather"]}))]),
                ),
                upstream(
                    "up_2",
                    json!([function_call("w1", "get_weather", &json!({"city": "Paris"}))]),
                ),
                upstream("up_3", json!([message("18C")])),
            ])
            .await;
            let context = durable_context(llm_url).await;
            let conversation = context.conv_handler.create().await.expect("conversation");
            let session = ResponseSession::new(
                NonZeroUsize::new(100).expect("nonzero"),
                NonZeroUsize::new(100_000).expect("nonzero"),
            );
            let mut previous_id = None;
            let mut first_output = Vec::new();
            for turn in 0..2 {
                let answer = json!({"type": "function_call_output", "call_id": "call_w1", "output": "18C"});
                let input = match turn {
                    0 => json!("weather?"),
                    // An unstored conversation turn is not appended, so the client replays the first turn.
                    _ if !store && !use_session => {
                        let mut items = vec![json!({"type": "message", "role": "user", "content": "weather?"})];
                        items.extend(first_output.iter().cloned());
                        items.push(answer);
                        Value::Array(items)
                    }
                    _ => json!([answer]),
                };
                // A conversation does not inherit tools, so each turn declares them.
                let mut payload = json!({"model": "test", "input": input, "store": store,
                    "tools": [{"type": "tool_search"}, deferred_weather()]});
                if turn == 1 && use_session {
                    payload.as_object_mut().expect("object").remove("tools");
                }
                if use_session {
                    payload["previous_response_id"] = json!(previous_id);
                } else {
                    payload["conversation"] = json!(conversation.conversation_id);
                }
                let payload: RequestPayload = serde_json::from_value(payload).expect("request");
                let execution = ExecuteRequest::new(payload, Arc::clone(&context));
                let execution = if use_session {
                    execution.with_session(&session, None).expect("session")
                } else {
                    execution
                };
                let Either::Left(response) = execution.run().await.expect(&case) else {
                    panic!("blocking response expected")
                };
                session.wait_until_idle().await.expect("idle session");
                if turn == 0 {
                    first_output = output(&response);
                }
                previous_id = Some(response.id);
            }
            let requests = requests.lock().await;
            assert_eq!(tool_names(&requests[2]), ["tool_search", "get_weather"], "{case}");
            let input = requests[2]["input"].as_array().expect("item input");
            assert_eq!(
                input.iter().filter(|item| item["name"] == "tool_search").count(),
                1,
                "{case}: {input:?}"
            );
            assert_eq!(input.last().expect("output")["call_id"], "call_w1", "{case}");
        }
    }
}

fn lookup() -> Value {
    json!({"type": "function", "name": "lookup", "description": "Look something up",
        "parameters": {"type": "object", "properties": {}}})
}

/// `(event type, output index, item type)` for each item lifecycle event.
fn item_events(events: &[Value]) -> Vec<(String, u64, String)> {
    events
        .iter()
        .filter(|event| event["item"].is_object())
        .map(|event| {
            (
                event["type"]
                    .as_str()
                    .unwrap_or_default()
                    .trim_start_matches("response.")
                    .to_owned(),
                event["output_index"].as_u64().unwrap_or(u64::MAX),
                event["item"]["type"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn lifecycle(index: u64, item: &str) -> [(String, u64, String); 2] {
    [
        ("output_item.added".to_owned(), index, item.to_owned()),
        ("output_item.done".to_owned(), index, item.to_owned()),
    ]
}

#[tokio::test]
async fn streaming_items_after_a_search_in_the_same_round_shift_by_its_output() {
    let search = function_call("s1", "tool_search", &json!({"paths": ["get_weather"]}));
    let lookup_call = function_call("l1", "lookup", &json!({}));
    let (llm_url, _) = spawn_streaming_llm(vec![sse("up_1", &[search, lookup_call])]).await;
    let mut payload = request(
        &json!("x"),
        &json!([{"type": "tool_search"}, deferred_weather(), lookup()]),
    );
    payload.stream = true;
    payload.parallel_tool_calls = Some(true);
    let events = run_streaming(payload, context(llm_url)).await;

    assert_eq!(
        item_events(&events),
        [
            lifecycle(0, "tool_search_call"),
            lifecycle(1, "tool_search_output"),
            lifecycle(2, "function_call")
        ]
        .concat()
    );
    let completed = events.last().expect("terminal event");
    let output = completed["response"]["output"].as_array().expect("terminal output");
    let kinds: Vec<_> = output
        .iter()
        .map(|item| item["type"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(kinds, ["tool_search_call", "tool_search_output", "function_call"]);
    assert_eq!(output[2]["name"], "lookup");
}

#[tokio::test]
async fn streaming_two_searches_in_one_round_announce_items_in_output_order() {
    let first = function_call("s1", "tool_search", &json!({"paths": ["get_weather"]}));
    let second = function_call("s2", "tool_search", &json!({"paths": ["travel"]}));
    let (llm_url, _) = spawn_streaming_llm(vec![sse("up_1", &[first, second]), sse("up_2", &[message("done")])]).await;
    let mut payload = request(
        &json!("x"),
        &json!([{"type": "tool_search"}, deferred_weather(), travel()]),
    );
    payload.stream = true;
    payload.parallel_tool_calls = Some(true);
    let events = run_streaming(payload, context(llm_url)).await;

    assert_eq!(
        item_events(&events),
        [
            lifecycle(0, "tool_search_call"),
            lifecycle(1, "tool_search_output"),
            lifecycle(2, "tool_search_call"),
            lifecycle(3, "tool_search_output"),
            lifecycle(4, "message")
        ]
        .concat()
    );
}

#[tokio::test]
async fn streaming_search_with_an_eager_mcp_call_in_one_round_keeps_output_order() {
    let (mcp_url, calls, _) = spawn_mcp().await;
    let search = function_call("s1", "tool_search", &json!({"paths": ["get_weather"]}));
    let echo = function_call("e1", "mcp__stub__echo", &json!({}));
    let (llm_url, _) = spawn_streaming_llm(vec![sse("up_1", &[search, echo]), sse("up_2", &[message("done")])]).await;
    let mut server = deferred_mcp(&mcp_url, None);
    server.as_object_mut().expect("object").remove("defer_loading");
    let mut payload = request(
        &json!("x"),
        &json!([{"type": "tool_search"}, server, deferred_weather()]),
    );
    payload.stream = true;
    payload.parallel_tool_calls = Some(true);
    let events = run_streaming(payload, context(llm_url)).await;

    assert_eq!(
        item_events(&events),
        [
            lifecycle(0, "mcp_list_tools"),
            lifecycle(1, "tool_search_call"),
            lifecycle(2, "tool_search_output"),
            lifecycle(3, "mcp_call"),
            lifecycle(4, "message")
        ]
        .concat()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn streaming_searches_around_a_client_call_keep_output_order() {
    let first = function_call("s1", "tool_search", &json!({"paths": ["get_weather"]}));
    let client = function_call("l1", "lookup", &json!({}));
    let second = function_call("s2", "tool_search", &json!({"paths": ["travel"]}));
    let (llm_url, _) = spawn_streaming_llm(vec![sse("up_1", &[first, client, second])]).await;
    let mut payload = request(
        &json!("x"),
        &json!([{"type": "tool_search"}, deferred_weather(), travel(), lookup()]),
    );
    payload.stream = true;
    payload.parallel_tool_calls = Some(true);
    let events = run_streaming(payload, context(llm_url)).await;

    assert_eq!(
        item_events(&events),
        [
            lifecycle(0, "tool_search_call"),
            lifecycle(1, "tool_search_output"),
            lifecycle(2, "function_call"),
            lifecycle(3, "tool_search_call"),
            lifecycle(4, "tool_search_output")
        ]
        .concat()
    );
}

/// An MCP stub whose `tools/call` waits until `gate` opens.
async fn spawn_gated_mcp(gate: Arc<Notify>) -> String {
    let app = Router::new().route(
        "/mcp",
        post(move |axum::Json(request): axum::Json<Value>| {
            let gate = Arc::clone(&gate);
            async move {
                let id = request["id"].clone();
                if id.is_null() {
                    return axum::http::StatusCode::ACCEPTED.into_response();
                }
                let reply = match request["method"].as_str() {
                    Some("initialize") => json!({"result": {
                        "protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                        "serverInfo": {"name": "gated", "version": "0"}
                    }}),
                    Some("tools/list") => json!({"result": {"tools": [{"name": "slow", "description": "Slow call.",
                        "inputSchema": {"type": "object", "properties": {}}}]}}),
                    Some("tools/call") => {
                        gate.notified().await;
                        json!({"result": {"content": [{"type": "text", "text": "done"}], "isError": false}})
                    }
                    _ => json!({"error": {"code": -32601, "message": "method not found"}}),
                };
                let mut body = json!({"jsonrpc": "2.0", "id": id});
                body.as_object_mut()
                    .expect("object")
                    .extend(reply.as_object().expect("object").clone());
                axum::Json(body).into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind MCP");
    let address = listener.local_addr().expect("MCP address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("MCP server") });
    format!("http://{address}/mcp")
}

/// Streams one round of `round_items` beside a gated MCP server and asserts that the search output reaches the client,
/// and the gated call is announced in progress, while that call is still running.
async fn assert_search_output_does_not_wait_for_the_gated_call(round_items: Vec<Value>) {
    let gate = Arc::new(Notify::new());
    let mcp_url = spawn_gated_mcp(Arc::clone(&gate)).await;
    let (llm_url, _) = spawn_streaming_llm(vec![sse("up_1", &round_items), sse("up_2", &[message("done")])]).await;
    let server = json!({"type": "mcp", "server_label": "gated", "server_url": mcp_url, "require_approval": "never"});
    let mut payload = request(
        &json!("x"),
        &json!([{"type": "tool_search"}, server, deferred_weather(), lookup()]),
    );
    payload.stream = true;
    payload.parallel_tool_calls = Some(true);
    let Either::Right(mut stream) = ExecuteRequest::new(payload, context(llm_url))
        .run()
        .await
        .expect("streaming execution")
    else {
        panic!("streaming request returned JSON")
    };

    let mut seen = Vec::new();
    let mut gate_open = false;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("the search output must not wait for the gated MCP call")
    {
        for line in chunk.lines() {
            let Some(data) = line.strip_prefix("data: ").filter(|data| *data != "[DONE]") else {
                continue;
            };
            let event: Value = serde_json::from_str(data).expect("event JSON");
            let kind = event["type"].as_str().unwrap_or_default().to_owned();
            seen.push((
                kind.clone(),
                event["item"]["type"].as_str().unwrap_or_default().to_owned(),
            ));
            if kind == "response.mcp_call.in_progress" && !gate_open {
                assert!(
                    seen.contains(&("response.output_item.done".to_owned(), "tool_search_output".to_owned())),
                    "{seen:?}"
                );
                gate_open = true;
                gate.notify_one();
            }
        }
    }
    assert!(gate_open, "the MCP call was announced in progress: {seen:?}");
    assert_eq!(seen.last().map(|(kind, _)| kind.as_str()), Some("response.completed"));
}

/// A search's output is known before anything runs, so it reaches the client while a slower call of the same
/// round is still running, and that call is announced in progress right after it.
#[tokio::test]
async fn streaming_search_output_does_not_wait_for_a_slower_call_in_its_round() {
    assert_search_output_does_not_wait_for_the_gated_call(vec![
        function_call("s1", "tool_search", &json!({"paths": ["get_weather"]})),
        function_call("m1", "mcp__gated__slow", &json!({})),
    ])
    .await;
}

/// The same with a client call between them, whose frames the relay defers until the search before it completes.
#[tokio::test]
async fn streaming_search_output_does_not_wait_for_a_slower_call_after_a_client_call() {
    assert_search_output_does_not_wait_for_the_gated_call(vec![
        function_call("s1", "tool_search", &json!({"paths": ["get_weather"]})),
        function_call("l1", "lookup", &json!({})),
        function_call("m1", "mcp__gated__slow", &json!({})),
    ])
    .await;
}

/// A searched top-level function's call carries `namespace` equal to its own name in public output only. When the
/// function is async, the response continues into another round, and that round's model input must still name the
/// call plainly: the declared tool is `get_weather`, not a namespace member.
#[tokio::test]
async fn async_continuation_after_a_search_keeps_the_call_unqualified_for_the_model() {
    let mut weather = deferred_weather();
    weather["async"] = json!(true);
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call("s1", "tool_search", &json!({"paths": ["get_weather"]}))]),
        ),
        upstream(
            "up_2",
            json!([function_call("w1", "get_weather", &json!({"city": "Paris"}))]),
        ),
        upstream("up_3", json!([message("checking")])),
    ])
    .await;
    let response = run(
        request(&json!("weather?"), &json!([{"type": "tool_search"}, weather])),
        context(llm_url),
    )
    .await;

    let public_call = output(&response)
        .into_iter()
        .find(|item| item["type"] == "function_call")
        .expect("public weather call");
    assert_eq!(
        public_call["namespace"], "get_weather",
        "the public item mirrors the Responses API"
    );
    let requests = requests.lock().await;
    assert_eq!(requests.len(), 3, "the async call continues into another round");
    let continued_call = requests[2]["input"]
        .as_array()
        .expect("upstream input")
        .iter()
        .find(|item| item["type"] == "function_call" && item["call_id"] == "call_w1")
        .expect("the async call in the next round's input")
        .clone();
    assert_eq!(continued_call["name"], "get_weather", "{continued_call}");
    assert!(
        continued_call.get("namespace").is_none_or(Value::is_null),
        "{continued_call}"
    );
}

#[tokio::test]
async fn streaming_mcp_search_then_call_numbers_every_item() {
    let (mcp_url, _, _) = spawn_mcp().await;
    let (llm_url, _) = spawn_streaming_llm(vec![
        sse(
            "up_1",
            &[function_call(
                "s1",
                "tool_search",
                &json!({"paths": ["mcp_stub"], "query": "echo"}),
            )],
        ),
        sse("up_2", &[function_call("m1", "mcp__stub__echo", &json!({}))]),
        sse("up_3", &[message("done")]),
    ])
    .await;
    let mut payload = request(
        &json!("x"),
        &json!([{"type": "tool_search"}, deferred_mcp(&mcp_url, None)]),
    );
    payload.stream = true;
    let events = run_streaming(payload, context(llm_url)).await;
    assert_eq!(
        item_events(&events),
        [
            lifecycle(0, "mcp_list_tools"),
            lifecycle(1, "tool_search_call"),
            lifecycle(2, "tool_search_output"),
            lifecycle(3, "mcp_call"),
            lifecycle(4, "message"),
        ]
        .concat()
    );
}

#[tokio::test]
async fn operator_configured_server_can_default_to_deferred_with_a_catalog_description() {
    let (mcp_url, _, _) = spawn_mcp().await;
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call("s1", "tool_search", &json!({"paths": ["mcp_stub"]}))]),
        ),
        upstream("up_2", json!([message("done")])),
    ])
    .await;
    let entry: McpServerEntry = serde_json::from_value(json!({
        "url": mcp_url, "require_approval": "never", "defer_loading": true, "description": "Configured stub."
    }))
    .expect("configured server");
    let config = ToolRuntimeConfig {
        mcp_servers: HashMap::from([("stub".to_owned(), entry)]),
        ..ToolRuntimeConfig::default()
    };
    let mut context = (*context(llm_url)).clone();
    context.gateway_executors =
        GatewayExecutors::from_config(Arc::new(reqwest::Client::new()), &config).expect("gateway executors");
    // The request names the configured server without deferring it itself.
    let response = run(
        request(
            &json!("x"),
            &json!([{"type": "tool_search"}, {"type": "mcp", "server_label": "stub", "require_approval": "never"}]),
        ),
        Arc::new(context),
    )
    .await;
    let items = output(&response);
    assert_eq!(items[2]["tools"][0]["name"], "mcp_stub");
    assert_eq!(items[2]["tools"][0]["description"], "Configured stub.");
    assert_eq!(tool_names(&requests.lock().await[0]), ["tool_search"]);
}

#[tokio::test]
async fn multi_agent_root_runs_a_hosted_search_and_continues() {
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call("s1", "tool_search", &json!({"paths": ["get_weather"]}))]),
        ),
        upstream(
            "up_2",
            json!([function_call("w1", "get_weather", &json!({"city": "Paris"}))]),
        ),
        upstream("up_3", json!([message("18C")])),
    ])
    .await;
    let context = durable_context(llm_url).await;
    let first: RequestPayload = serde_json::from_value(json!({
        "model": "test", "input": "weather?", "store": true, "multi_agent": {"enabled": true},
        "tools": [{"type": "tool_search"}, deferred_weather()]
    }))
    .expect("first request");
    let first = run(first, Arc::clone(&context)).await;
    let kinds: Vec<_> = output(&first)
        .iter()
        .map(|item| item["type"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(kinds, ["tool_search_call", "tool_search_output", "function_call"]);

    let second: RequestPayload = serde_json::from_value(json!({
        "model": "test", "store": true, "previous_response_id": first.id,
        "input": [{"type": "function_call_output", "call_id": "call_w1", "output": "18C"}]
    }))
    .expect("second request");
    let second = run(second, Arc::clone(&context)).await;
    assert_eq!(output(&second).last().expect("answer")["type"], "message");

    let requests = requests.lock().await;
    for (round, request) in requests.iter().enumerate() {
        let input = request["input"].as_array().expect("item input");
        let searches: Vec<_> = input.iter().filter(|item| item["name"] == "tool_search").collect();
        let outputs = input
            .iter()
            .filter(|item| {
                item["type"] == "function_call_output"
                    && searches.iter().any(|search| search["call_id"] == item["call_id"])
            })
            .count();
        assert_eq!(
            searches.len(),
            outputs,
            "round {round}: every search call has its output: {input:?}"
        );
        let names = tool_names(request);
        assert_eq!(
            names.contains(&"get_weather".to_owned()),
            round > 0,
            "round {round}: {names:?}"
        );
    }
}

#[tokio::test]
async fn multi_agent_stores_the_hosted_pair_once() {
    let (llm_url, _) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call("s1", "tool_search", &json!({"paths": ["get_weather"]}))]),
        ),
        upstream("up_2", json!([message("done")])),
    ])
    .await;
    let (context, conversations) = durable_context_with_conversations(llm_url).await;
    let conversation = context.conv_handler.create().await.expect("conversation");
    let payload: RequestPayload = serde_json::from_value(json!({
        "model": "test", "input": "weather?", "store": true, "multi_agent": {"enabled": true},
        "conversation": conversation.conversation_id, "tools": [{"type": "tool_search"}, deferred_weather()]
    }))
    .expect("request");
    run(payload, Arc::clone(&context)).await;
    let items = conversations
        .rehydrate(&conversation.conversation_id)
        .await
        .expect("conversation items");
    let kinds: Vec<_> = items
        .iter()
        .map(|item| {
            let stored = String::try_from(item).expect("stored item JSON");
            serde_json::from_str::<Value>(&stored).expect("item")["type"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        kinds.iter().filter(|kind| *kind == "tool_search_call").count(),
        1,
        "{kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|kind| *kind == "tool_search_output").count(),
        1,
        "{kinds:?}"
    );
}

#[tokio::test]
async fn tool_choice_cannot_force_an_unloaded_mcp_tool() {
    let (mcp_url, _, _) = spawn_mcp().await;
    let (llm_url, requests) = spawn_llm(vec![upstream("up_1", json!([message("x")]))]).await;
    let payload: RequestPayload = serde_json::from_value(json!({
        "model": "test", "input": "x", "store": false,
        "tools": [{"type": "tool_search"}, deferred_mcp(&mcp_url, None)],
        "tool_choice": {"type": "function", "name": "mcp__stub__echo"}
    }))
    .expect("request");
    let Err(error) = Box::pin(ExecuteRequest::new(payload, context(llm_url)).run()).await else {
        panic!("an unloaded MCP tool cannot be forced")
    };
    assert_eq!(error.http_status().as_u16(), 400);
    assert!(error.to_string().contains("before its definition is loaded"), "{error}");
    assert!(requests.lock().await.is_empty());
}

#[tokio::test]
async fn continuation_offers_only_the_mcp_tools_not_loaded_yet() {
    let (mcp_url, _, _) = spawn_mcp().await;
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([function_call(
                "s1",
                "tool_search",
                &json!({"paths": ["mcp_stub"], "query": "echo"})
            )]),
        ),
        upstream("up_2", json!([message("first")])),
        upstream(
            "up_3",
            json!([function_call("s2", "tool_search", &json!({"paths": ["mcp_stub"]}))]),
        ),
        upstream("up_4", json!([message("second")])),
    ])
    .await;
    let context = durable_context(llm_url).await;
    let first: RequestPayload = serde_json::from_value(json!({"model": "test", "input": "x", "store": true,
        "tools": [{"type": "tool_search"}, deferred_mcp(&mcp_url, None)]}))
    .expect("first");
    let first = run(first, Arc::clone(&context)).await;
    let second: RequestPayload = serde_json::from_value(json!({"model": "test", "input": "more", "store": true,
        "previous_response_id": first.id}))
    .expect("second");
    let second = run(second, Arc::clone(&context)).await;
    let items = output(&second);
    assert_eq!(items[0]["type"], "tool_search_call", "no new listing: {items:?}");
    let members: Vec<_> = items[1]["tools"][0]["tools"]
        .as_array()
        .expect("members")
        .iter()
        .map(|member| member["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(members, ["reverse"]);
    let requests = requests.lock().await;
    assert_eq!(tool_names(&requests[2]), ["tool_search", "mcp__stub__echo"]);
    assert_eq!(
        tool_names(&requests[3]),
        ["tool_search", "mcp__stub__echo", "mcp__stub__reverse"]
    );
}

#[tokio::test]
async fn two_searches_in_one_round_each_get_a_call_and_output() {
    let (llm_url, requests) = spawn_llm(vec![
        upstream(
            "up_1",
            json!([
                function_call("s1", "tool_search", &json!({"paths": ["get_weather"]})),
                function_call(
                    "s2",
                    "tool_search",
                    &json!({"paths": ["travel"], "query": "coordinates"})
                )
            ]),
        ),
        upstream("up_2", json!([message("done")])),
    ])
    .await;
    let mut payload = request(
        &json!("x"),
        &json!([{"type": "tool_search"}, deferred_weather(), travel()]),
    );
    payload.parallel_tool_calls = Some(true);
    let response = run(payload, context(llm_url)).await;
    let ids: Vec<_> = output(&response)
        .iter()
        .map(|item| item["id"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(ids[..4], ["tsc_s1", "tso_s1", "tsc_s2", "tso_s2"]);
    let member = model_visible_namespace_member_name("travel", "get_coordinates");
    assert_eq!(
        tool_names(&requests.lock().await[1]),
        ["tool_search", "get_weather", member.as_str()]
    );
}

#[tokio::test]
async fn replayed_hosted_history_allows_parallel_tool_calls_without_a_declaration() {
    let (llm_url, _) = spawn_llm(vec![upstream("up_1", json!([message("ok")]))]).await;
    let payload: RequestPayload = serde_json::from_value(json!({
        "model": "test", "store": false, "parallel_tool_calls": true,
        "input": [
            {"type": "message", "role": "user", "content": "x"},
            {"type": "tool_search_call", "id": "tsc_1", "call_id": null, "execution": "server",
                "arguments": {"paths": []}, "status": "completed"},
            {"type": "tool_search_output", "id": "tso_1", "call_id": null, "execution": "server",
                "status": "completed", "tools": []}
        ]
    }))
    .expect("request");
    assert_eq!(output(&run(payload, context(llm_url)).await)[0]["type"], "message");
}

/// The model cannot call a deferred MCP tool before a search loads it: the response fails and the MCP server never
/// receives the call, blocking and streaming.
#[tokio::test]
async fn an_unloaded_deferred_mcp_tool_is_never_called() {
    let echo = function_call("e1", "mcp__stub__echo", &json!({}));
    let (mcp_url, calls, _) = spawn_mcp().await;
    let tools = json!([{"type": "tool_search"}, deferred_mcp(&mcp_url, None)]);

    let (llm_url, _) = spawn_llm(vec![upstream("up_1", json!([echo.clone()]))]).await;
    let blocking = Box::pin(ExecuteRequest::new(request(&json!("echo"), &tools), context(llm_url)).run()).await;
    assert!(blocking.is_err(), "the unloaded call fails the response");

    let (llm_url, _) = spawn_streaming_llm(vec![sse("up_1", &[echo])]).await;
    let mut streaming = request(&json!("echo"), &tools);
    streaming.stream = true;
    let events = run_streaming(streaming, context(llm_url)).await;
    let last = events.last().expect("a terminal event");
    assert_eq!(last["type"], "response.failed", "{last}");

    assert_eq!(calls.load(Ordering::SeqCst), 0, "no tools/call reached the MCP server");
}

/// A hosted search whose arguments are not JSON loads nothing. Streaming carries on with the next round, as it did
/// before hosted and client searches shared one tool kind; blocking still refuses the upstream response, as it did.
#[tokio::test]
async fn malformed_hosted_search_arguments_keep_their_blocking_and_streaming_answers() {
    let malformed = json!({"type": "function_call", "id": "fc_s1", "call_id": "call_s1", "name": "tool_search",
        "arguments": "not json", "status": "completed"});
    let tools = json!([{"type": "tool_search"}, deferred_weather()]);

    let (llm_url, _) = spawn_llm(vec![upstream("up_1", json!([malformed.clone()]))]).await;
    let blocking = Box::pin(ExecuteRequest::new(request(&json!("weather?"), &tools), context(llm_url)).run()).await;
    assert!(blocking.is_err(), "blocking validation refuses the call");

    let (llm_url, _) = spawn_streaming_llm(vec![
        sse("up_1", &[malformed]),
        sse("up_2", &[message("no tool loaded")]),
    ])
    .await;
    let mut streaming = request(&json!("weather?"), &tools);
    streaming.stream = true;
    let events = run_streaming(streaming, context(llm_url)).await;
    let last = events.last().expect("a terminal event");
    assert_eq!(last["type"], "response.completed", "{last}");
    let search_output = events
        .iter()
        .find(|event| event["type"] == "response.output_item.done" && event["item"]["type"] == "tool_search_output")
        .expect("the search answers");
    assert_eq!(search_output["item"]["tools"], json!([]));
}
