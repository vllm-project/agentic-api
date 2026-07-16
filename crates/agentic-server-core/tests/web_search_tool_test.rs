use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use agentic_core::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::{GatewayExecutor, WebSearchHandler};
use agentic_core::types::io::{OutputItem, ResponsesInput, ToolChoice};
use agentic_core::types::request_response::RequestPayload;
use agentic_core::types::tools::ResponsesTool;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use either::Either;
use futures::StreamExt;
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};

mod support;

#[derive(Debug)]
struct CapturedSearchRequest {
    api_key: String,
    body: serde_json::Value,
}

async fn spawn_mock_you() -> (
    String,
    mpsc::Receiver<CapturedSearchRequest>,
    tokio::task::JoinHandle<()>,
) {
    spawn_mock_you_with_response(
        StatusCode::OK,
        serde_json::json!({
            "results": {
                "web": [{
                    "url": "https://example.com/rust",
                    "title": "Rust async guide",
                    "description": "A useful guide",
                    "snippets": ["Use async carefully."]
                }],
                "news": []
            },
            "metadata": {
                "query": "rust async",
                "search_uuid": "search_123",
                "latency": 0.12
            }
        }),
    )
    .await
}

async fn spawn_mock_you_with_response(
    status: StatusCode,
    response_body: serde_json::Value,
) -> (
    String,
    mpsc::Receiver<CapturedSearchRequest>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::channel(16);
    let app = Router::new()
        .route(
            "/v1/search",
            post(
                move |State(tx): State<mpsc::Sender<CapturedSearchRequest>>,
                      headers: HeaderMap,
                      Json(body): Json<serde_json::Value>| async move {
                    let api_key = headers
                        .get("x-api-key")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    tx.send(CapturedSearchRequest { api_key, body }).await.unwrap();
                    (status, Json(response_body.clone()))
                },
            ),
        )
        .with_state(tx);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), rx, handle)
}

async fn spawn_mock_you_waiting_for_two_searches() -> (
    String,
    mpsc::Receiver<CapturedSearchRequest>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::channel(16);
    let started = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let app = Router::new()
        .route(
            "/v1/search",
            post(
                move |State((tx, started, notify)): State<(
                    mpsc::Sender<CapturedSearchRequest>,
                    Arc<AtomicUsize>,
                    Arc<Notify>,
                )>,
                      headers: HeaderMap,
                      Json(body): Json<serde_json::Value>| async move {
                    let api_key = headers
                        .get("x-api-key")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    tx.send(CapturedSearchRequest {
                        api_key,
                        body: body.clone(),
                    })
                    .await
                    .unwrap();
                    if started.fetch_add(1, Ordering::SeqCst) + 1 >= 2 {
                        notify.notify_waiters();
                    }
                    while started.load(Ordering::SeqCst) < 2 {
                        notify.notified().await;
                    }

                    let query = body["query"].as_str().unwrap_or("unknown");
                    let slug = query.replace(' ', "-");
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "results": {
                                "web": [{
                                    "url": format!("https://example.com/{slug}"),
                                    "title": format!("{query} guide")
                                }],
                                "news": []
                            },
                            "metadata": {"query": query}
                        })),
                    )
                },
            ),
        )
        .with_state((tx, started, notify));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), rx, handle)
}

#[tokio::test]
async fn web_search_handler_posts_to_you_and_formats_results() {
    let (base_url, mut captured, _handle) = spawn_mock_you().await;
    let handler =
        WebSearchHandler::with_api_key(Arc::new(reqwest::Client::new()), "secret-you-key".to_owned(), &base_url);

    let output = handler
        .execute(
            "call_search",
            "web_search",
            r#"{"query":"rust async","count":2}"#,
            &serde_json::json!({"type":"web_search_preview"}),
        )
        .await
        .unwrap();

    let request = captured.recv().await.expect("mock You.com should receive request");
    assert_eq!(request.api_key, "secret-you-key");
    assert_eq!(request.body["query"], "rust async");
    assert_eq!(request.body["count"], 2);

    assert_eq!(output.call_id, "call_search");
    let output_json: serde_json::Value = serde_json::from_str(&output.output).unwrap();
    assert_eq!(output_json["query"], "rust async");
    assert_eq!(output_json["results"]["web"][0]["url"], "https://example.com/rust");
    assert_eq!(output_json["metadata"]["search_uuid"], "search_123");
}

#[tokio::test]
async fn web_search_handler_requires_base_url() {
    let handler = WebSearchHandler::with_api_key(Arc::new(reqwest::Client::new()), "secret-you-key".to_owned(), "");

    let err = handler
        .execute(
            "call_search",
            "web_search",
            r#"{"query":"rust async"}"#,
            &serde_json::json!({"type":"web_search_preview"}),
        )
        .await
        .unwrap_err();

    assert_eq!(
        err.to_string(),
        "invalid tool config: YOU_API_BASE_URL must be set to use the web_search tool"
    );
}

fn web_search_function_call_response() -> support::MockResponse {
    support::MockResponse::Json(
        serde_json::json!({
            "id": "resp_tool_call",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [{
                "id": "fc_search",
                "type": "function_call",
                "call_id": "call_search",
                "name": "web_search",
                "arguments": "{\"query\":\"rust async\",\"count\":2}",
                "status": "completed"
            }],
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
        .to_string(),
    )
}

fn two_web_search_function_call_response() -> support::MockResponse {
    support::MockResponse::Json(
        serde_json::json!({
            "id": "resp_two_tool_calls",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [
                {
                    "id": "fc_search_1",
                    "type": "function_call",
                    "call_id": "call_search_1",
                    "name": "web_search",
                    "arguments": "{\"query\":\"rust async\",\"count\":2}",
                    "status": "completed"
                },
                {
                    "id": "fc_search_2",
                    "type": "function_call",
                    "call_id": "call_search_2",
                    "name": "web_search",
                    "arguments": "{\"query\":\"tokio streams\",\"count\":2}",
                    "status": "completed"
                }
            ],
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
        .to_string(),
    )
}

fn many_web_search_function_call_response(count: usize) -> support::MockResponse {
    let output: Vec<serde_json::Value> = (0..count)
        .map(|index| {
            serde_json::json!({
                "id": format!("fc_search_{index}"),
                "type": "function_call",
                "call_id": format!("call_search_{index}"),
                "name": "web_search",
                "arguments": format!(r#"{{"query":"rust async {index}","count":2}}"#),
                "status": "completed"
            })
        })
        .collect();
    support::MockResponse::Json(
        serde_json::json!({
            "id": "resp_many_tool_calls",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": output,
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
        .to_string(),
    )
}

fn malformed_web_search_function_call_response() -> support::MockResponse {
    support::MockResponse::Json(
        serde_json::json!({
            "id": "resp_bad_tool_call",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [{
                "id": "fc_search_bad",
                "type": "function_call",
                "call_id": "call_search_bad",
                "name": "web_search",
                "arguments": "{not json",
                "status": "completed"
            }],
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
        .to_string(),
    )
}

fn sse_response(events: impl IntoIterator<Item = serde_json::Value>) -> support::MockResponse {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&serde_json::to_string(&event).unwrap());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    support::MockResponse::Sse(body)
}

fn web_search_function_call_sse_response() -> support::MockResponse {
    sse_response([
        serde_json::json!({
            "type": "response.created",
            "response": {"id": "resp_tool_call", "status": "in_progress", "usage": null}
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "fc_search",
                "type": "function_call",
                "call_id": "call_search",
                "name": "web_search",
                "arguments": "",
                "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.done",
            "item_id": "fc_search",
            "output_index": 0,
            "call_id": "call_search",
            "name": "web_search",
            "arguments": "{\"query\":\"rust async\",\"count\":2}"
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_tool_call", "status": "completed", "usage": null}
        }),
    ])
}

fn web_search_function_call_sse_response_with_name_only_on_done() -> support::MockResponse {
    sse_response([
        serde_json::json!({
            "type": "response.created",
            "response": {"id": "resp_tool_call", "status": "in_progress", "usage": null}
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "fc_search",
                "type": "function_call",
                "call_id": "call_search",
                "arguments": "",
                "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_search",
            "output_index": 0,
            "call_id": "call_search",
            "delta": "{\"query\":\"rust async\",\"count\":2}"
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.done",
            "item_id": "fc_search",
            "output_index": 0,
            "call_id": "call_search",
            "name": "web_search",
            "arguments": "{\"query\":\"rust async\",\"count\":2}"
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_tool_call", "status": "completed", "usage": null}
        }),
    ])
}

fn text_sse_response(text: &str) -> support::MockResponse {
    sse_response([
        serde_json::json!({
            "type": "response.created",
            "response": {"id": "resp_final", "status": "in_progress", "usage": null}
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "msg_final",
                "type": "message",
                "role": "assistant",
                "status": "in_progress",
                "content": []
            }
        }),
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg_final",
            "output_index": 0,
            "content_index": 0,
            "delta": text
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_final", "status": "completed", "usage": null}
        }),
    ])
}

fn mixed_web_search_and_client_function_response() -> support::MockResponse {
    support::MockResponse::Json(
        serde_json::json!({
            "id": "resp_mixed_tool_call",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [
                {
                    "id": "fc_search",
                    "type": "function_call",
                    "call_id": "call_search",
                    "name": "web_search",
                    "arguments": "{\"query\":\"rust async\",\"count\":2}",
                    "status": "completed"
                },
                {
                    "id": "fc_weather",
                    "type": "function_call",
                    "call_id": "call_weather",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"San Francisco\"}",
                    "status": "completed"
                }
            ],
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
        .to_string(),
    )
}

fn text_response_with_usage(text: &str, input_tokens: i64, output_tokens: i64) -> support::MockResponse {
    let id_suffix = text.replace(' ', "_");
    support::MockResponse::Json(
        serde_json::json!({
            "id": format!("resp_upstream_{id_suffix}"),
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [{
                "id": format!("msg_upstream_{id_suffix}"),
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": []
                }]
            }],
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": input_tokens + output_tokens,
                "input_tokens_details": {"cached_tokens": 1},
                "output_tokens_details": {"reasoning_tokens": 2}
            },
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
        .to_string(),
    )
}

fn web_search_function_call_response_with_usage(input_tokens: i64, output_tokens: i64) -> support::MockResponse {
    support::MockResponse::Json(
        serde_json::json!({
            "id": "resp_tool_call",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [{
                "id": "fc_search",
                "type": "function_call",
                "call_id": "call_search",
                "name": "web_search",
                "arguments": "{\"query\":\"rust async\",\"count\":2}",
                "status": "completed"
            }],
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": input_tokens + output_tokens,
                "input_tokens_details": {"cached_tokens": 3},
                "output_tokens_details": {"reasoning_tokens": 4}
            },
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
        .to_string(),
    )
}

async fn build_exec_ctx(llm_url: &str, you_url: String) -> Arc<ExecutionContext> {
    let pool = support::setup_pool().await;
    let conv_handler = ConversationHandler::new(ConversationStore::new(Arc::clone(&pool)));
    let resp_handler = ResponseHandler::new(ResponseStore::new(pool));
    let client = Arc::new(reqwest::Client::new());
    Arc::new(
        ExecutionContext::new(conv_handler, resp_handler, Arc::clone(&client), llm_url.to_owned())
            .with_gateway_executor(Arc::new(WebSearchHandler::with_api_key(
                client,
                "secret-you-key".to_owned(),
                &you_url,
            ))),
    )
}

#[tokio::test]
async fn execute_runs_web_search_and_sends_tool_output_back_to_model() {
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you().await;
    let llm = support::MockServer::start_deque(vec![
        web_search_function_call_response(),
        support::text_response("Use async carefully."),
    ])
    .await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up rust async".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search]),
        tool_choice: None,
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = ExecuteRequest::new(payload, Arc::clone(&exec_ctx)).run().await.unwrap();
    let Either::Left(response) = result else {
        panic!("expected non-streaming response");
    };

    let request = captured_you.recv().await.expect("mock You.com should receive request");
    assert_eq!(request.body["query"], "rust async");

    let request_bodies = llm.request_bodies().await;
    assert_eq!(request_bodies.len(), 2);
    assert_eq!(request_bodies[0]["tools"][0]["name"], "web_search");
    assert_eq!(request_bodies[0]["max_output_tokens"], 1024);
    let second_input = request_bodies[1]["input"]
        .as_array()
        .expect("second request input array");
    let tool_output = second_input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("second request includes web_search output");
    assert_eq!(tool_output["call_id"], "call_search");
    assert!(
        tool_output["output"]
            .as_str()
            .unwrap()
            .contains("https://example.com/rust")
    );

    let response_output = serde_json::to_value(&response.output).unwrap();
    let output_items = response_output.as_array().unwrap();
    assert!(
        !output_items
            .iter()
            .any(|item| item["type"] == "function_call" && item["name"] == "web_search"),
        "raw web_search function calls must stay internal"
    );
    let web_search_call = output_items
        .iter()
        .find(|item| item["type"] == "web_search_call")
        .expect("response output should include web_search_call item");
    assert_eq!(web_search_call["status"], "completed");
    assert_eq!(web_search_call["action"]["type"], "search");
    assert_eq!(web_search_call["action"]["query"], "rust async");
    assert_eq!(
        web_search_call["action"]["sources"][0]["url"],
        "https://example.com/rust"
    );
    assert_eq!(web_search_call["action"]["sources"][0]["title"], "Rust async guide");
    assert!(
        response
            .output
            .iter()
            .any(|item| matches!(item, OutputItem::Message(_)))
    );
}

#[tokio::test]
async fn execute_relaxes_forced_tool_choice_after_web_search_result() {
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you().await;
    let llm = support::MockServer::start_deque(vec![
        web_search_function_call_response(),
        support::text_response("Use async carefully."),
    ])
    .await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up rust async".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search]),
        tool_choice: Some(ToolChoice::Required),
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = ExecuteRequest::new(payload, Arc::clone(&exec_ctx)).run().await.unwrap();
    assert!(matches!(result, Either::Left(_)));
    captured_you.recv().await.expect("mock You.com should receive request");

    let request_bodies = llm.request_bodies().await;
    assert_eq!(request_bodies.len(), 2);
    assert_eq!(request_bodies[0]["tool_choice"], "required");
    assert!(request_bodies[1].get("tool_choice").is_none());
}

#[tokio::test]
async fn execute_returns_mixed_client_tool_calls_without_followup_model_request() {
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you().await;
    let llm = support::MockServer::start_deque(vec![
        mixed_web_search_and_client_function_response(),
        support::text_response("continued after mixed tools"),
    ])
    .await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let client_function: ResponsesTool = serde_json::from_value(serde_json::json!({
        "type": "function",
        "name": "get_weather",
        "parameters": {
            "type": "object",
            "properties": {"city": {"type": "string"}}
        }
    }))
    .unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up rust async and weather".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search, client_function]),
        tool_choice: None,
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = ExecuteRequest::new(payload, Arc::clone(&exec_ctx)).run().await.unwrap();
    let Either::Left(response) = result else {
        panic!("expected non-streaming response");
    };

    assert_eq!(llm.request_bodies().await.len(), 1);
    let search_request = captured_you.recv().await.expect("mock You.com should receive request");
    assert_eq!(search_request.body["query"], "rust async");
    let response_output = serde_json::to_value(&response.output).unwrap();
    let output_items = response_output.as_array().unwrap();
    assert_eq!(output_items[0]["type"], "web_search_call");
    assert_eq!(output_items[0]["action"]["query"], "rust async");
    assert!(
        !output_items
            .iter()
            .any(|item| item["type"] == "function_call" && item["name"] == "web_search"),
        "raw web_search function calls must stay internal"
    );
    let function_names: Vec<&str> = response
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::FunctionCall(call) => Some(call.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(function_names, ["get_weather"]);

    let continuation_payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("continue".to_owned()),
        instructions: None,
        previous_response_id: Some(response.id),
        conversation_id: None,
        tools: None,
        tool_choice: None,
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };
    let continuation = ExecuteRequest::new(continuation_payload, exec_ctx).run().await.unwrap();
    assert!(matches!(continuation, Either::Left(_)));
    let request_bodies = llm.request_bodies().await;
    assert_eq!(request_bodies.len(), 2);
    let second_input = request_bodies[1]["input"]
        .as_array()
        .expect("second request input array");
    let tool_output = second_input
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "call_search")
        .expect("continuation includes persisted web_search output");
    assert!(
        tool_output["output"]
            .as_str()
            .unwrap()
            .contains("https://example.com/rust")
    );
}

#[tokio::test]
async fn web_search_rejects_incompatible_domain_filters_before_calling_you() {
    let (base_url, mut captured, _handle) = spawn_mock_you().await;
    let handler =
        WebSearchHandler::with_api_key(Arc::new(reqwest::Client::new()), "secret-you-key".to_owned(), &base_url);

    let err = handler
        .execute(
            "call_search",
            "web_search",
            r#"{"query":"rust async","exclude_domains":["example.com"]}"#,
            &serde_json::json!({
                "type": "web_search_preview",
                "filters": {"allowed_domains": ["rust-lang.org"]}
            }),
        )
        .await
        .expect_err("allowed_domains and exclude_domains should be rejected");

    assert!(err.to_string().contains("include_domains cannot be combined"));
    assert!(captured.try_recv().is_err());
}

#[tokio::test]
async fn execute_accumulates_usage_across_web_search_model_rounds() {
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you().await;
    let llm = support::MockServer::start_deque(vec![
        web_search_function_call_response_with_usage(10, 5),
        text_response_with_usage("Use async carefully.", 7, 3),
    ])
    .await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up rust async".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search]),
        tool_choice: None,
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = ExecuteRequest::new(payload, exec_ctx).run().await.unwrap();
    let Either::Left(response) = result else {
        panic!("expected non-streaming response");
    };
    captured_you.recv().await.expect("mock You.com should receive request");

    let usage = response.usage.expect("usage should be present");
    assert_eq!(usage.input_tokens, 17);
    assert_eq!(usage.output_tokens, 8);
    assert_eq!(usage.total_tokens, 25);
    assert_eq!(usage.input_tokens_details.cached_tokens, 4);
    assert_eq!(usage.output_tokens_details.reasoning_tokens, 6);
}

#[tokio::test]
async fn stream_emits_web_search_lifecycle_events_before_final_payload() {
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you().await;
    let llm = support::MockServer::start_deque(vec![
        web_search_function_call_sse_response(),
        text_sse_response("Use async carefully."),
    ])
    .await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up rust async".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search]),
        tool_choice: None,
        stream: true,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = ExecuteRequest::new(payload, Arc::clone(&exec_ctx)).run().await.unwrap();
    let Either::Right(stream) = result else {
        panic!("expected streaming response");
    };
    let chunks: Vec<String> = stream.collect().await;
    captured_you.recv().await.expect("mock You.com should receive request");

    let json_events: Vec<serde_json::Value> = chunks
        .iter()
        .filter_map(|chunk| {
            let data = chunk.trim_end_matches('\n').strip_prefix("data: ")?;
            if data == "[DONE]" {
                return None;
            }
            serde_json::from_str(data).ok()
        })
        .collect();
    let event_types: Vec<&str> = json_events.iter().filter_map(|event| event["type"].as_str()).collect();
    assert!(event_types.contains(&"response.created"));
    assert!(event_types.contains(&"response.output_text.delta"));
    assert!(
        !json_events
            .iter()
            .any(|event| event["item"]["type"] == "function_call" && event["item"]["name"] == "web_search"),
        "internal web_search function call events should not be forwarded"
    );
    let expected_types = [
        "response.output_item.added",
        "response.web_search_call.in_progress",
        "response.web_search_call.searching",
        "response.web_search_call.completed",
        "response.output_item.done",
    ];
    let mut last_index = 0;
    for expected in expected_types {
        let index = event_types
            .iter()
            .enumerate()
            .skip(last_index)
            .find_map(|(index, actual)| (*actual == expected).then_some(index))
            .unwrap_or_else(|| panic!("missing streaming event {expected}; got {event_types:?}"));
        last_index = index + 1;
    }

    let completed_event = json_events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("stream should include response.completed event");
    let output = completed_event["response"]["output"].as_array().unwrap();
    assert!(output.iter().any(|item| item["type"] == "web_search_call"));
    assert!(output.iter().any(|item| item["type"] == "message"));
}

#[tokio::test]
async fn stream_hides_web_search_function_events_when_name_arrives_on_done() {
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you().await;
    let llm = support::MockServer::start_deque(vec![
        web_search_function_call_sse_response_with_name_only_on_done(),
        text_sse_response("Use async carefully."),
    ])
    .await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up rust async".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search]),
        tool_choice: None,
        stream: true,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = ExecuteRequest::new(payload, Arc::clone(&exec_ctx)).run().await.unwrap();
    let Either::Right(stream) = result else {
        panic!("expected streaming response");
    };
    let chunks: Vec<String> = stream.collect().await;
    captured_you.recv().await.expect("mock You.com should receive request");

    let json_events: Vec<serde_json::Value> = chunks
        .iter()
        .filter_map(|chunk| {
            let data = chunk.trim_end_matches('\n').strip_prefix("data: ")?;
            if data == "[DONE]" {
                return None;
            }
            serde_json::from_str(data).ok()
        })
        .collect();
    assert!(
        !json_events.iter().any(|event| {
            event["item"]["type"] == "function_call"
                || event["type"] == "response.function_call_arguments.delta"
                || event["type"] == "response.function_call_arguments.done"
        }),
        "internal web_search function call stream events should stay hidden"
    );
    let event_types: Vec<&str> = json_events.iter().filter_map(|event| event["type"].as_str()).collect();
    assert!(event_types.contains(&"response.web_search_call.in_progress"));
    assert!(event_types.contains(&"response.web_search_call.completed"));
    let completed_event = json_events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("stream should include response.completed event");
    let output = completed_event["response"]["output"].as_array().unwrap();
    assert!(output.iter().any(|item| item["type"] == "web_search_call"));
    assert!(output.iter().any(|item| item["type"] == "message"));
}

#[tokio::test]
async fn execute_runs_multiple_web_search_calls_concurrently() {
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you_waiting_for_two_searches().await;
    let llm = support::MockServer::start_deque(vec![
        two_web_search_function_call_response(),
        support::text_response("Use async carefully."),
    ])
    .await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up rust async and tokio streams".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search]),
        tool_choice: None,
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = tokio::time::timeout(Duration::from_secs(2), ExecuteRequest::new(payload, exec_ctx).run())
        .await
        .expect("gateway calls should execute concurrently instead of waiting on the first search")
        .unwrap();
    assert!(matches!(result, Either::Left(_)));

    let mut queries = Vec::new();
    for _ in 0..2 {
        let request = captured_you.recv().await.expect("mock You.com should receive request");
        queries.push(request.body["query"].as_str().unwrap().to_owned());
    }
    queries.sort();
    assert_eq!(queries, ["rust async", "tokio streams"]);
}

#[tokio::test]
async fn execute_feeds_web_search_execution_errors_back_to_model() {
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you_with_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        serde_json::json!({"error": "search backend down"}),
    )
    .await;
    let llm = support::MockServer::start_deque(vec![
        web_search_function_call_response(),
        support::text_response("I could not search live web results."),
    ])
    .await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up rust async".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search]),
        tool_choice: None,
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = ExecuteRequest::new(payload, exec_ctx).run().await.unwrap();
    assert!(matches!(result, Either::Left(_)));
    captured_you.recv().await.expect("mock You.com should receive request");

    let request_bodies = llm.request_bodies().await;
    assert_eq!(request_bodies.len(), 2);
    let second_input = request_bodies[1]["input"]
        .as_array()
        .expect("second request input array");
    let tool_output = second_input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("second request includes web_search error output");
    let output_json: serde_json::Value = serde_json::from_str(tool_output["output"].as_str().unwrap()).unwrap();
    assert!(
        output_json["error"]
            .as_str()
            .unwrap()
            .contains("You.com search returned 500 Internal Server Error")
    );
}

#[tokio::test]
async fn execute_returns_incomplete_after_max_gateway_tool_rounds() {
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you().await;
    let llm_responses = std::iter::repeat_with(web_search_function_call_response)
        .take(10)
        .collect();
    let llm = support::MockServer::start_deque(llm_responses).await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up rust async".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search]),
        tool_choice: None,
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    // Budget exhausted while the model keeps requesting tools → the response is
    // surfaced as status: "incomplete" (Responses API semantics), not an error.
    let result = ExecuteRequest::new(payload, exec_ctx)
        .run()
        .await
        .expect("hitting the round cap returns an incomplete response, not an error");
    let Either::Left(response) = result else {
        panic!("non-streaming request should return a payload");
    };
    assert_eq!(response.status, "incomplete");
    assert_eq!(
        response.incomplete_details.and_then(|d| d.reason).as_deref(),
        Some("gateway tool execution exceeded 10 rounds")
    );
    for _ in 0..10 {
        captured_you.recv().await.expect("mock You.com should receive request");
    }
    assert!(captured_you.try_recv().is_err());
    assert_eq!(llm.request_bodies().await.len(), 10);
}

#[tokio::test]
async fn execute_feeds_invalid_web_search_arguments_back_to_model() {
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you().await;
    let llm = support::MockServer::start_deque(vec![
        malformed_web_search_function_call_response(),
        support::text_response("I could not run that search."),
    ])
    .await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up rust async".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search]),
        tool_choice: None,
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = ExecuteRequest::new(payload, exec_ctx).run().await.unwrap();
    assert!(matches!(result, Either::Left(_)));
    assert!(
        captured_you.try_recv().is_err(),
        "invalid arguments should not call You.com"
    );

    let request_bodies = llm.request_bodies().await;
    assert_eq!(request_bodies.len(), 2);
    let second_input = request_bodies[1]["input"]
        .as_array()
        .expect("second request input array");
    let tool_output = second_input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("second request includes failed web_search output");
    let output_json: serde_json::Value = serde_json::from_str(tool_output["output"].as_str().unwrap()).unwrap();
    assert!(
        output_json["error"]
            .as_str()
            .unwrap()
            .contains("web_search arguments must be valid JSON")
    );
}

#[tokio::test]
async fn execute_runs_large_gateway_fanout_without_hard_cap() {
    // A single turn requesting 9 gateway calls (more than the concurrency
    // window of 5) must all execute — the sliding window drains them safely,
    // with no hard per-round count cap that fails the request.
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you().await;
    let llm = support::MockServer::start_deque(vec![
        many_web_search_function_call_response(9),
        support::text_response("done with all searches"),
    ])
    .await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up many things".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search]),
        tool_choice: None,
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = ExecuteRequest::new(payload, exec_ctx)
        .run()
        .await
        .expect("large fan-out executes; no hard cap error");
    let Either::Left(response) = result else {
        panic!("non-streaming request should return a payload");
    };
    assert_eq!(response.status, "completed");

    let mut searches = 0;
    while captured_you.try_recv().is_ok() {
        searches += 1;
    }
    assert_eq!(
        searches, 9,
        "all 9 gateway calls execute despite the concurrency window of 5"
    );
    assert_eq!(llm.request_bodies().await.len(), 2);
}

#[tokio::test]
async fn stream_error_events_escape_error_messages() {
    let app = Router::new().route(
        "/v1/responses",
        post(|| async {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"bad \"quoted\" upstream response"})),
            )
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let pool = agentic_core::storage::create_pool_with_schema(None).await.unwrap();
    let conv_handler = ConversationHandler::new(ConversationStore::new(Arc::clone(&pool)));
    let resp_handler = ResponseHandler::new(ResponseStore::new(pool));
    let client = Arc::new(reqwest::Client::new());
    let exec_ctx = Arc::new(ExecutionContext::new(
        conv_handler,
        resp_handler,
        client,
        format!("http://{addr}"),
    ));
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("hi".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: None,
        tool_choice: None,
        stream: true,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = ExecuteRequest::new(payload, exec_ctx).run().await.unwrap();
    let Either::Right(stream) = result else {
        panic!("expected streaming response");
    };
    let chunks: Vec<String> = stream.collect().await;
    let error_chunk = chunks
        .iter()
        .find(|chunk| chunk.starts_with("data: {") && chunk.contains("\"error\""))
        .expect("stream should include error event");
    let json = error_chunk.trim().strip_prefix("data: ").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(json).expect("error event should be valid JSON");
    assert_eq!(parsed["type"], "error");
    let error = parsed["error"]["message"].as_str().unwrap();
    assert!(error.contains(r#"bad \"quoted\" upstream response"#), "{error}");
}

fn web_search_function_call_response_with_id(call_id: &str) -> support::MockResponse {
    support::MockResponse::Json(
        serde_json::json!({
            "id": format!("resp_{call_id}"),
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [{
                "id": format!("fc_{call_id}"),
                "type": "function_call",
                "call_id": call_id,
                "name": "web_search",
                "arguments": "{\"query\":\"rust async\",\"count\":2}",
                "status": "completed"
            }],
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
        .to_string(),
    )
}

#[tokio::test]
async fn incomplete_turn_persists_a_consistent_conversation_for_continuation() {
    // 10 web_search rounds -> incomplete, then one text turn for the continuation.
    // Each round emits a UNIQUE call_id (call_r0..call_r9) so the FINAL round's
    // call is individually detectable in the persisted conversation.
    let (you_url, _captured_you, _you_handle) = spawn_mock_you().await;
    let mut llm_responses: Vec<support::MockResponse> = (0..10)
        .map(|round| web_search_function_call_response_with_id(&format!("call_r{round}")))
        .collect();
    llm_responses.push(support::text_response("continued after incomplete"));
    let llm = support::MockServer::start_deque(llm_responses).await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up rust async".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search]),
        tool_choice: None,
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = ExecuteRequest::new(payload, Arc::clone(&exec_ctx)).run().await.unwrap();
    let Either::Left(response) = result else {
        panic!("non-streaming request should return a payload");
    };
    assert_eq!(response.status, "incomplete");

    // Continue from the incomplete response — rehydrates the persisted turn.
    let continuation_payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("continue".to_owned()),
        instructions: None,
        previous_response_id: Some(response.id),
        conversation_id: None,
        tools: None,
        tool_choice: None,
        stream: false,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };
    let _ = ExecuteRequest::new(continuation_payload, exec_ctx).run().await.unwrap();

    // The continuation's upstream request carries the rehydrated conversation.
    // Every function_call must pair with a function_call_output — no dangling call.
    let request_bodies = llm.request_bodies().await;
    let continuation_input = request_bodies
        .last()
        .expect("continuation request")
        .get("input")
        .and_then(|input| input.as_array())
        .expect("continuation input is an array");
    let call_ids: std::collections::HashSet<&str> = continuation_input
        .iter()
        .filter(|item| item["type"] == "function_call")
        .filter_map(|item| item["call_id"].as_str())
        .collect();
    let output_call_ids: std::collections::HashSet<&str> = continuation_input
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .filter_map(|item| item["call_id"].as_str())
        .collect();

    // The FINAL round (call_r9) executed but the loop stopped on the budget. Its
    // call and output must both be persisted — guards the Incomplete arm skipping
    // the append, which would drop call_r9's output while its call is in output.
    assert!(
        call_ids.contains("call_r9"),
        "final-round gateway call must be persisted, got calls {call_ids:?}"
    );
    assert!(
        output_call_ids.contains("call_r9"),
        "final-round gateway call has no matching output in the persisted conversation"
    );
    for cid in &call_ids {
        assert!(
            output_call_ids.contains(cid),
            "gateway function_call {cid} has no matching output in the persisted conversation"
        );
    }
}

#[tokio::test]
async fn stream_returns_incomplete_after_max_gateway_tool_rounds() {
    // Streaming counterpart of the blocking cap test: past the round budget over
    // an SSE stream, the final streamed payload must carry status "incomplete".
    let (you_url, mut captured_you, _you_handle) = spawn_mock_you().await;
    let llm_responses = std::iter::repeat_with(web_search_function_call_sse_response)
        .take(10)
        .collect();
    let llm = support::MockServer::start_deque(llm_responses).await;
    let exec_ctx = build_exec_ctx(llm.url(), you_url).await;
    let web_search: ResponsesTool = serde_json::from_value(serde_json::json!({"type": "web_search_preview"})).unwrap();
    let payload = RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text("look up rust async".to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: Some(vec![web_search]),
        tool_choice: None,
        stream: true,
        store: true,
        include: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(1024),
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        cache_salt: None,
    };

    let result = ExecuteRequest::new(payload, exec_ctx).run().await.unwrap();
    let Either::Right(stream) = result else {
        panic!("expected streaming response");
    };
    let chunks: Vec<String> = stream.collect().await;

    let final_event = chunks
        .iter()
        .filter_map(|chunk| {
            let data = chunk.trim_end_matches('\n').strip_prefix("data: ")?;
            (data != "[DONE]").then(|| serde_json::from_str::<serde_json::Value>(data).ok())?
        })
        .next_back()
        .expect("stream should carry a final response payload");
    // The terminal SSE event wraps the payload: {"type":"response.incomplete","response":{...}}
    let response = &final_event["response"];
    assert_eq!(response["status"], "incomplete");
    assert_eq!(
        response["incomplete_details"]["reason"],
        "gateway tool execution exceeded 10 rounds"
    );
    for _ in 0..10 {
        captured_you.recv().await.expect("mock You.com should receive request");
    }
    assert_eq!(llm.request_bodies().await.len(), 10);
}
