//! How the gateway answers malformed tool-search history, on the direct path (`store: false` input)
//! and on a `previous_response_id` continuation.
//!
//! The pending-call ledger checks how client search calls and outputs pair, on both paths; the
//! search history pairing checks the rest (item ids, hosted pairs, output status), and the tool
//! catalog applies it in history order. Both paths therefore answer alike.
//! A request without search items still skips the ledger on the direct path, as before.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agentic_core::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler};
use agentic_core::storage::{ConversationStore, ResponseStore, create_pool_with_schema};
use agentic_core::types::request_response::RequestPayload;
use axum::Router;
use axum::body::Bytes;
use axum::response::IntoResponse;
use axum::routing::post;
use either::Either;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// A mock upstream that answers every request with a short assistant message.
async fn spawn_llm(count: usize) -> String {
    let responses: VecDeque<Value> = (0..count)
        .map(|n| {
            json!({"id": format!("up_{n}"), "object": "response", "status": "completed", "model": "test",
                "created_at": 0, "output": [{"type": "message", "id": format!("msg_{n}"), "role": "assistant",
                "status": "completed", "content": [{"type": "output_text", "text": "ok", "annotations": []}]}]})
        })
        .collect();
    let responses = Arc::new(Mutex::new(responses));
    let app = Router::new().route(
        "/v1/responses",
        post(move |_body: Bytes| {
            let responses = Arc::clone(&responses);
            async move {
                let response = responses.lock().await.pop_front().expect("one response per request");
                axum::response::Response::builder()
                    .status(200)
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(response.to_string()))
                    .expect("mock response")
                    .into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock LLM");
    let address = listener.local_addr().expect("mock address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("mock server") });
    format!("http://{address}")
}

async fn durable_context(llm_url: String) -> Arc<ExecutionContext> {
    static DATABASE: AtomicU64 = AtomicU64::new(0);
    let url = format!(
        "sqlite:file:tool_search_pairing_{}?mode=memory&cache=shared",
        DATABASE.fetch_add(1, Ordering::Relaxed)
    );
    let pool = create_pool_with_schema(Some(&url)).await.expect("storage pool");
    Arc::new(ExecutionContext::new(
        ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
        ResponseHandler::new(ResponseStore::new(pool)),
        Arc::new(reqwest::Client::new()),
        llm_url,
    ))
}

fn deferred_weather() -> Value {
    json!({"type": "function", "name": "get_weather", "description": "Get weather",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
        "strict": true, "defer_loading": true})
}

fn client_tools() -> Value {
    json!([{"type": "tool_search", "execution": "client", "description": "Search", "parameters":
        {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}}, deferred_weather()])
}

fn hosted_tools() -> Value {
    json!([{"type": "tool_search"}, deferred_weather()])
}

fn plain_tools() -> Value {
    json!([{"type": "function", "name": "get_weather", "parameters": {"type": "object"}}])
}

fn client_call(id: &str, call_id: &str) -> Value {
    json!({"type": "tool_search_call", "id": id, "call_id": call_id, "execution": "client",
        "arguments": {"query": "weather"}, "status": "completed"})
}

fn client_output(call_id: &str, status: &str) -> Value {
    json!({"type": "tool_search_output", "call_id": call_id, "execution": "client", "status": status,
        "tools": [deferred_weather()]})
}

fn hosted_call(id: &str) -> Value {
    json!({"type": "tool_search_call", "id": id, "call_id": null, "execution": "server",
        "arguments": {"paths": ["get_weather"]}, "status": "completed"})
}

fn hosted_output(id: &str, status: &str) -> Value {
    json!({"type": "tool_search_output", "id": id, "call_id": null, "execution": "server", "status": status,
        "tools": [deferred_weather()]})
}

fn function_call(call_id: &str) -> Value {
    json!({"type": "function_call", "id": format!("fc_{call_id}"), "call_id": call_id, "name": "get_weather",
        "arguments": "{\"city\":\"Paris\"}", "status": "completed"})
}

fn function_output(call_id: &str) -> Value {
    json!({"type": "function_call_output", "call_id": call_id, "output": "18C"})
}

const INVALID_CONFIG: &str = "tool error: invalid tool config: ";

/// One malformed history and today's answer on each path: `OK`, or the error's status, `param`
/// and message.
struct Case {
    name: &'static str,
    input: Value,
    tools: Value,
    direct: Answer,
    continuation: Answer,
}

#[derive(Debug, PartialEq)]
enum Answer {
    Ok,
    Rejected {
        code: &'static str,
        param: Option<String>,
        message: String,
    },
}

fn config(message: &str) -> Answer {
    Answer::Rejected {
        code: "invalid_request_error",
        param: None,
        message: format!("{INVALID_CONFIG}{message}"),
    }
}

fn invalid(message: &str) -> Answer {
    Answer::Rejected {
        code: "invalid_request_error",
        param: None,
        message: format!("invalid request: {message}"),
    }
}

fn on_input(message: &str) -> Answer {
    Answer::Rejected {
        code: "invalid_request_error",
        param: Some("input".to_owned()),
        message: message.to_owned(),
    }
}

/// Admission rejects an input that repeats an item id before any history check runs, as `OpenAI` does.
fn duplicate_item(id: &str) -> Answer {
    Answer::Rejected {
        code: "unsupported_value",
        param: Some("input".to_owned()),
        message: format!("Duplicate item found with id {id}. Remove duplicate items from your input and try again."),
    }
}

const UNRESOLVED: &str = "unresolved tool_search_call requires a matching completed tool_search_output";
const INCOMPLETE: &str = "tool_search_output must be completed before it may load tool definitions";

/// Client-executed searches, where the client pairs call and output by `call_id`.
fn client_cases() -> Vec<Case> {
    vec![
        Case {
            name: "duplicate item id",
            input: json!([
                client_call("tsc_a", "call_1"),
                client_output("call_1", "completed"),
                client_call("tsc_a", "call_2"),
                client_output("call_2", "completed")
            ]),
            tools: client_tools(),
            direct: duplicate_item("tsc_a"),
            continuation: duplicate_item("tsc_a"),
        },
        Case {
            name: "duplicate client call_id",
            input: json!([
                client_call("tsc_a", "call_1"),
                client_output("call_1", "completed"),
                client_call("tsc_b", "call_1"),
                client_output("call_1", "completed")
            ]),
            tools: client_tools(),
            direct: invalid("duplicate call_id 'call_1' in tool_search_call"),
            continuation: invalid("duplicate call_id 'call_1' in tool_search_call"),
        },
        Case {
            name: "duplicate client output",
            input: json!([
                client_call("tsc_a", "call_1"),
                client_output("call_1", "completed"),
                client_output("call_1", "completed")
            ]),
            tools: client_tools(),
            direct: invalid("tool_search_output references call_id 'call_1' without a pending call"),
            continuation: invalid("tool_search_output references call_id 'call_1' without a pending call"),
        },
        Case {
            name: "client output without its call",
            input: json!([client_output("call_missing", "completed")]),
            tools: client_tools(),
            direct: on_input("No tool call found for function call output with call_id call_missing."),
            continuation: on_input("No tool call found for function call output with call_id call_missing."),
        },
        Case {
            name: "incomplete client output",
            input: json!([client_call("tsc_a", "call_1"), client_output("call_1", "incomplete")]),
            tools: client_tools(),
            direct: config(INCOMPLETE),
            continuation: config(INCOMPLETE),
        },
        Case {
            name: "client output before its call",
            input: json!([client_output("call_1", "completed"), client_call("tsc_a", "call_1")]),
            tools: client_tools(),
            direct: on_input("No tool call found for function call output with call_id call_1."),
            continuation: on_input("No tool call found for function call output with call_id call_1."),
        },
        Case {
            name: "client output for another call",
            input: json!([client_call("tsc_a", "call_1"), client_output("call_2", "completed")]),
            tools: client_tools(),
            direct: on_input("No tool call found for function call output with call_id call_2."),
            continuation: on_input("No tool call found for function call output with call_id call_2."),
        },
        Case {
            name: "client call without its output",
            input: json!([client_call("tsc_a", "call_1")]),
            tools: client_tools(),
            direct: on_input("No tool output found for function call call_1."),
            continuation: on_input("No tool output found for function call call_1."),
        },
        Case {
            name: "search history then an unanswered function call",
            input: json!([
                client_call("tsc_a", "call_1"),
                client_output("call_1", "completed"),
                function_call("call_f")
            ]),
            tools: client_tools(),
            direct: on_input("No tool output found for function call call_f."),
            continuation: on_input("No tool output found for function call call_f."),
        },
    ]
}

/// Hosted searches (`call_id: null`), and a function call without any search.
fn hosted_and_plain_cases() -> Vec<Case> {
    vec![
        Case {
            name: "hosted call id equal to a client call_id",
            input: json!([
                client_call("tsc_c", "tsc_h"),
                client_output("tsc_h", "completed"),
                hosted_call("tsc_h"),
                hosted_output("tso_h", "completed")
            ]),
            tools: hosted_tools(),
            direct: config("duplicate tool_search_call call_id"),
            continuation: config("duplicate tool_search_call call_id"),
        },
        Case {
            name: "hosted call id equal to a function call's call_id",
            input: json!([
                hosted_call("tsc_h"),
                hosted_output("tso_h", "completed"),
                function_call("tsc_h"),
                function_output("tsc_h")
            ]),
            tools: hosted_tools(),
            direct: config("duplicate tool_search_call call_id"),
            continuation: config("duplicate tool_search_call call_id"),
        },
        Case {
            name: "hosted output without a preceding hosted call",
            input: json!([hosted_output("tso_x", "completed")]),
            tools: hosted_tools(),
            direct: config("hosted tool_search_output has no preceding hosted tool_search_call"),
            continuation: config("hosted tool_search_output has no preceding hosted tool_search_call"),
        },
        Case {
            name: "incomplete hosted output",
            input: json!([hosted_call("tsc_h"), hosted_output("tso_h", "incomplete")]),
            tools: hosted_tools(),
            direct: config(INCOMPLETE),
            continuation: config(INCOMPLETE),
        },
        Case {
            name: "hosted call without its output",
            input: json!([hosted_call("tsc_h")]),
            tools: hosted_tools(),
            direct: config(UNRESOLVED),
            continuation: config(UNRESOLVED),
        },
        // A history with two defects gets the answer of the earlier one: history is checked in order.
        Case {
            name: "an open hosted call, then a call before its function is loaded",
            input: json!([hosted_call("tsc_h"), function_call("call_f"), function_output("call_f")]),
            tools: hosted_tools(),
            direct: config("request history calls a function before its definition is loaded"),
            continuation: config("request history calls a function before its definition is loaded"),
        },
        Case {
            name: "a hosted output with no call, then an incomplete one",
            input: json!([
                hosted_output("tso_x", "completed"),
                hosted_call("tsc_h"),
                hosted_output("tso_h", "incomplete")
            ]),
            tools: hosted_tools(),
            direct: config("hosted tool_search_output has no preceding hosted tool_search_call"),
            continuation: config("hosted tool_search_output has no preceding hosted tool_search_call"),
        },
        Case {
            name: "unanswered function call without tool search",
            input: json!([function_call("call_f")]),
            tools: plain_tools(),
            direct: Answer::Ok,
            continuation: on_input("No tool output found for function call call_f."),
        },
    ]
}

async fn answer(body: Value, context: Arc<ExecutionContext>) -> Answer {
    let request: RequestPayload = serde_json::from_value(body).expect("public request");
    match Box::pin(ExecuteRequest::new(request, context).run()).await {
        Ok(Either::Left(_)) => Answer::Ok,
        Ok(Either::Right(_)) => panic!("blocking request returned a stream"),
        Err(error) => {
            assert_eq!(error.http_status().as_u16(), 400);
            assert_eq!(error.error_type(), "invalid_request_error");
            Answer::Rejected {
                code: error.error_code(),
                param: error.error_param().map(ToOwned::to_owned),
                message: error.error_message(),
            }
        }
    }
}

/// Stores a first response, then sends `input` as its continuation.
async fn continuation_answer(input: &Value, tools: &Value) -> Answer {
    let context = durable_context(spawn_llm(2).await).await;
    let first: RequestPayload =
        serde_json::from_value(json!({"model": "test", "input": "hi", "tools": tools, "store": true}))
            .expect("first request");
    let Ok(Either::Left(first)) = Box::pin(ExecuteRequest::new(first, Arc::clone(&context)).run()).await else {
        panic!("first request completes");
    };
    answer(
        json!({"model": "test", "input": input, "tools": tools, "store": true, "previous_response_id": first.id}),
        context,
    )
    .await
}

#[tokio::test]
async fn malformed_search_history_gets_the_same_answer_on_each_path() {
    for case in client_cases().into_iter().chain(hosted_and_plain_cases()) {
        let direct = answer(
            json!({"model": "test", "input": case.input, "tools": case.tools, "store": false}),
            durable_context(spawn_llm(1).await).await,
        )
        .await;
        assert_eq!(direct, case.direct, "direct path: {}", case.name);
        let continuation = continuation_answer(&case.input, &case.tools).await;
        assert_eq!(continuation, case.continuation, "continuation: {}", case.name);
    }
}

#[test]
fn malformed_search_item_shapes_fail_to_parse() {
    let blank_item_id = json!([client_call(" ", "call_1"), client_output("call_1", "completed")]);
    let blank_call_id = json!([client_call("tsc_a", " "), client_output(" ", "completed")]);
    let hosted_with_call_id = json!([{"type": "tool_search_call", "id": "tsc_h", "call_id": "x",
        "execution": "server", "arguments": {}, "status": "completed"}]);
    for input in [blank_item_id, blank_call_id, hosted_with_call_id] {
        let parsed = serde_json::from_value::<RequestPayload>(
            json!({"model": "test", "input": input, "tools": client_tools(), "store": false}),
        );
        assert!(parsed.is_err(), "{input}");
    }
}
