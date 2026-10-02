//! Forced tool choice must allow the answer after a gateway-executed call.
#[allow(dead_code)]
mod common;

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentic_core::executor::{ExecutionContext, normalize_native_web_search_for_upstream};
use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

struct Server(tokio::task::JoinHandle<()>);

impl Server {
    async fn stop(mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone)]
struct Upstream {
    requests: Arc<Mutex<Vec<Value>>>,
    searches: Arc<AtomicUsize>,
    search_fails: bool,
    wanted_searches: usize,
    mixed_calls: bool,
    tool_stop_reason: &'static str,
}

fn tool_call(name: &str, round: usize) -> Value {
    json!({"type":"tool_use", "id":format!("call_{round}_{name}"), "name":name, "input":{"query":"proof"}})
}

fn encode_message(message: &Value) -> String {
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"]["output_tokens"] = json!(0);
    let mut events = vec![json!({"type":"message_start", "message":start})];
    for (index, block) in message["content"].as_array().unwrap().iter().enumerate() {
        let mut initial = block.clone();
        let delta = if block["type"] == "tool_use" {
            initial["input"] = json!({});
            json!({"type":"input_json_delta", "partial_json":block["input"].to_string()})
        } else {
            initial["text"] = json!("");
            json!({"type":"text_delta", "text":block["text"]})
        };
        events.extend([
            json!({"type":"content_block_start", "index":index, "content_block":initial}),
            json!({"type":"content_block_delta", "index":index, "delta":delta}),
            json!({"type":"content_block_stop", "index":index}),
        ]);
    }
    events.extend([
        json!({"type":"message_delta", "delta":{"stop_reason":message["stop_reason"], "stop_sequence":null}, "usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ]);
    let mut encoded = String::new();
    for event in events {
        write!(encoded, "event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).unwrap();
    }
    encoded
}

async fn infer(State(state): State<Upstream>, Json(request): Json<Value>) -> Response {
    state.requests.lock().await.push(request.clone());
    let history = request["messages"].as_array().unwrap();
    let rounds = (history.len() - 1) / 2;
    let mut answer = "no search requested".to_owned();
    if rounds > 0 {
        // Inspect the real downstream input, including the successfully
        // executed search output and its link to the assistant's call.
        let result = &history.last().unwrap()["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], history[history.len() - 2]["content"][0]["id"]);
        assert_eq!(result["is_error"], state.search_fails);
        let output = result["content"].as_str().unwrap();
        assert!(output.contains(if state.search_fails {
            "tool execution failed"
        } else {
            "SEARCH_PROOF_雪"
        }));
        answer = if state.search_fails {
            "handled search failure".to_owned()
        } else {
            let search: Value = serde_json::from_str(output).unwrap();
            search["results"]["web"][0]["description"].as_str().unwrap().to_owned()
        };
    }
    let kind = request["tool_choice"]["type"].as_str().unwrap_or("auto");
    let named = request["tool_choice"]["name"].as_str().unwrap_or("web_search");
    // vLLM maps `any` to required and `tool` to a named function on *each*
    // Messages request. A forced request therefore cannot return plain text,
    // even when its history already contains a successful tool result.
    let call = kind == "any" || kind == "tool" || (kind != "none" && rounds < state.wanted_searches);
    let content = if call {
        let mut calls = vec![tool_call(if kind == "tool" { named } else { "web_search" }, rounds)];
        if state.mixed_calls {
            calls.push(tool_call(
                if kind == "tool" && named == "client_echo" {
                    "web_search"
                } else {
                    "client_echo"
                },
                rounds,
            ));
        }
        calls
    } else {
        vec![json!({"type":"text", "text":answer})]
    };
    let message = json!({"id":"msg_choice", "type":"message", "role":"assistant", "model":"test-model",
        "content":content, "stop_reason":if call {state.tool_stop_reason} else {"end_turn"}, "stop_sequence":null,
        "usage":{"input_tokens":5,"output_tokens":3}});
    if request["stream"] == true {
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(encode_message(&message)))
            .unwrap()
    } else {
        Json(message).into_response()
    }
}

fn assert_client_message(body: &str, stream: bool, client_owned: bool, expected_answer: &str, tool_stop_reason: &str) {
    assert!(
        !body.contains("\"name\":\"web_search\""),
        "gateway calls must remain hidden: {body}"
    );
    let stop = if client_owned && tool_stop_reason == "end_turn" {
        "tool_use"
    } else if client_owned {
        tool_stop_reason
    } else {
        "end_turn"
    };
    if stream {
        let events: Vec<Value> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.last().unwrap()["type"], "message_stop", "{body}");
        for kind in ["message_start", "message_delta", "message_stop"] {
            assert_eq!(events.iter().filter(|event| event["type"] == kind).count(), 1, "{body}");
        }
        assert!(
            events.iter().any(|event| event["delta"]["stop_reason"] == stop),
            "{body}"
        );
        if client_owned {
            assert!(
                events
                    .iter()
                    .any(|event| event["content_block"]["name"] == "client_echo"),
                "{body}"
            );
        } else {
            assert!(
                events.iter().any(|event| event["delta"]["text"] == expected_answer),
                "{body}"
            );
        }
    } else {
        let message: Value = serde_json::from_str(body).unwrap();
        assert_eq!(message["stop_reason"], stop, "{body}");
        if client_owned {
            assert_eq!(message["content"].as_array().unwrap().len(), 1);
            assert_eq!(message["content"][0]["name"], "client_echo");
            assert_eq!(message["content"][0]["input"], json!({"query":"proof"}));
        } else {
            assert_eq!(message["content"][0]["text"], expected_answer);
        }
    }
}

fn assert_upstream_rounds(rounds: &[Value], expected_request: &Value) {
    assert_eq!(
        &rounds[0], expected_request,
        "first inference must retain the original selector"
    );
    let mut expected_choice = expected_request.get("tool_choice").cloned();
    if let Some(choice) = &mut expected_choice
        && (choice["type"] == "any" || choice["type"] == "tool")
    {
        if choice["type"] == "tool" {
            choice.as_object_mut().unwrap().remove("name");
        }
        choice["type"] = json!("auto");
    }
    for round in &rounds[1..] {
        assert_eq!(round.get("tool_choice"), expected_choice.as_ref());
        let mut unchanged = round.clone();
        unchanged["messages"] = expected_request["messages"].clone();
        if let Some(choice) = expected_request.get("tool_choice") {
            unchanged["tool_choice"] = choice.clone();
        }
        assert_eq!(
            &unchanged, expected_request,
            "only history and fulfilled choice may change"
        );
    }
}

async fn check_choice(choice: Option<Value>, stream: bool, search_fails: bool, wanted_searches: usize, mixed: bool) {
    check_choice_with_stop(choice, stream, search_fails, wanted_searches, mixed, "tool_use").await;
}

async fn check_choice_with_stop(
    choice: Option<Value>,
    stream: bool,
    search_fails: bool,
    wanted_searches: usize,
    mixed: bool,
    tool_stop_reason: &'static str,
) {
    let upstream = Upstream {
        requests: Arc::default(),
        searches: Arc::default(),
        search_fails,
        wanted_searches,
        mixed_calls: mixed,
        tool_stop_reason,
    };
    let app = Router::new()
        .route("/v1/messages", post(infer))
        .route("/v1/search", get(|State(state): State<Upstream>| async move {
            state.searches.fetch_add(1, Ordering::SeqCst);
            if state.search_fails {
                return (http::StatusCode::SERVICE_UNAVAILABLE, "search unavailable").into_response();
            }
            Json(json!({"results":{"web":[{"url":"https://example.com/proof", "title":"proof", "description":"SEARCH_PROOF_雪"}]}})).into_response()
        }))
        .with_state(upstream.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let provider = Server(tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }));
    let directory = tempfile::tempdir().unwrap();
    let mut config = common::test_config(&url);
    config.db_url = Some(format!("sqlite://{}", directory.path().join("state.db").display()));
    config.tools.web_search.api_key = Some("local-key".to_owned());
    config.tools.web_search.base_url = Some(url);
    let context = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let mut state = common::test_state(&config);
    state.exec_ctx = Arc::clone(&context);
    let (url, gateway) = common::spawn_gateway(state).await;
    let gateway = Server(gateway);
    let mut request = json!({"model":"test-model", "max_tokens":64, "stream":stream,
        "messages":[{"role":"user", "content":"Search for proof"}],
        "system":[{"type":"text", "text":"preserve", "cache_control":{"type":"ephemeral"}}],
        "tools":[{"type":"web_search_20250305", "name":"web_search"},
                 {"name":"client_echo", "input_schema":{"type":"object"}}]});
    if let Some(choice) = &choice {
        request["tool_choice"] = choice.clone();
    }
    let mut expected_request = request.clone();
    assert!(normalize_native_web_search_for_upstream(&mut expected_request).unwrap());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let client_owned = mixed
        || choice
            .as_ref()
            .is_some_and(|choice| choice["type"] == "tool" && choice["name"] == "client_echo");
    let no_tools = choice.as_ref().is_some_and(|choice| choice["type"] == "none");
    let expected_searches = if client_owned || no_tools { 0 } else { wanted_searches };
    let expected_rounds = expected_searches + 1;
    let expected_answer = if no_tools {
        "no search requested"
    } else if search_fails {
        "handled search failure"
    } else {
        "SEARCH_PROOF_雪"
    };
    // Repeat on the same gateway to ensure the relaxed choice is request-local.
    for request_number in 0..2 {
        let response = client
            .post(format!("{url}/v1/messages"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        let body = response.text().await.unwrap();
        assert_client_message(&body, stream, client_owned, expected_answer, tool_stop_reason);
        let requests = upstream.requests.lock().await;
        assert_eq!(requests.len(), (request_number + 1) * expected_rounds);
        let rounds = &requests[request_number * expected_rounds..];
        assert_upstream_rounds(rounds, &expected_request);
    }
    assert_eq!(upstream.searches.load(Ordering::SeqCst), expected_searches * 2);
    for table in ["responses", "items", "conversations"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(context.storage_pool().unwrap())
            .await
            .unwrap();
        assert_eq!(count, 0, "Messages must not persist {table}");
    }
    gateway.stop().await;
    provider.stop().await;
    context.storage_pool().unwrap().close().await;
}

#[tokio::test]
async fn named_choice_finishes_http_search() {
    check_choice(
        Some(json!({"type":"tool", "name":"web_search", "disable_parallel_tool_use":true})),
        false,
        false,
        1,
        false,
    )
    .await;
}

#[tokio::test]
async fn any_choice_finishes_http_search() {
    check_choice(
        Some(json!({"type":"any", "disable_parallel_tool_use":false})),
        false,
        false,
        1,
        false,
    )
    .await;
}

#[tokio::test]
async fn named_choice_finishes_streaming_search() {
    check_choice(
        Some(json!({"type":"tool", "name":"web_search", "disable_parallel_tool_use":false})),
        true,
        false,
        1,
        false,
    )
    .await;
}

#[tokio::test]
async fn any_choice_finishes_streaming_search() {
    check_choice(
        Some(json!({"type":"any", "disable_parallel_tool_use":true})),
        true,
        false,
        1,
        false,
    )
    .await;
}

#[tokio::test]
async fn named_choice_end_turn_executes_the_selected_gateway_call() {
    for stream in [false, true] {
        check_choice_with_stop(
            Some(json!({"type":"tool", "name":"web_search", "disable_parallel_tool_use":true})),
            stream,
            false,
            1,
            false,
            "end_turn",
        )
        .await;
    }
}

#[tokio::test]
async fn named_choice_end_turn_preserves_client_tool_ownership() {
    for stream in [false, true] {
        for (name, mixed) in [("client_echo", false), ("web_search", true), ("client_echo", true)] {
            check_choice_with_stop(
                Some(json!({"type":"tool", "name":name})),
                stream,
                false,
                1,
                mixed,
                "end_turn",
            )
            .await;
        }
    }
}

#[tokio::test]
async fn client_tool_truncation_and_other_stops_are_preserved() {
    for stream in [false, true] {
        for reason in ["max_tokens", "stop_sequence", "pause_turn", "future"] {
            for mixed in [false, true] {
                check_choice_with_stop(
                    Some(json!({"type":"tool", "name":"client_echo"})),
                    stream,
                    false,
                    1,
                    mixed,
                    reason,
                )
                .await;
            }
        }
    }
}

#[tokio::test]
async fn any_choice_preserves_name_extension() {
    for stream in [false, true] {
        check_choice(
            Some(json!({"type":"any", "name":"web_search", "disable_parallel_tool_use":true})),
            stream,
            false,
            1,
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn any_choice_name_does_not_select_a_client_tool() {
    for stream in [false, true] {
        check_choice(
            Some(json!({"type":"any", "name":"client_echo", "disable_parallel_tool_use":false})),
            stream,
            false,
            1,
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn unforced_choices_keep_their_behavior() {
    for stream in [false, true] {
        for choice in [
            None,
            Some(Value::Null),
            Some(json!({"type":"auto", "disable_parallel_tool_use":true})),
            Some(json!({"type":"none"})),
        ] {
            check_choice(choice, stream, false, 1, false).await;
        }
    }
}

#[tokio::test]
async fn forced_choice_can_finish_after_tool_failure() {
    for stream in [false, true] {
        check_choice(
            Some(json!({"type":"tool", "name":"web_search"})),
            stream,
            true,
            1,
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn client_choices_and_mixed_rounds_remain_terminal() {
    for stream in [false, true] {
        check_choice(
            Some(json!({"type":"tool", "name":"client_echo"})),
            stream,
            false,
            1,
            false,
        )
        .await;
        check_choice(Some(json!({"type":"any"})), stream, false, 1, true).await;
    }
}

#[tokio::test]
async fn fulfilled_choice_still_allows_another_search() {
    for stream in [false, true] {
        check_choice(
            Some(json!({"type":"tool", "name":"web_search"})),
            stream,
            false,
            2,
            false,
        )
        .await;
    }
}
