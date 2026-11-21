//! Gateway-made server tool blocks sent back over HTTP (#409): `/v1/messages`
//! and `/v1/messages/count_tokens` give the upstream the rounds it saw, on the
//! gateway loop and on the transparent proxy, and refuse altered content.
#[allow(dead_code)]
mod common;

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use agentic_core::executor::{ConversationHandler, ExecutionContext, MessagesResultKey, ResponseHandler};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::WebSearchHandler;
use agentic_core::types::messages::{GatewayToolMap, UpstreamServerTools};
use axum::extract::{OriginalUri, State};
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use http::StatusCode;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::TcpListener;

/// The scripted upstream, its token counter, and the search backend.
#[derive(Clone, Default)]
struct Backend {
    turns: Arc<Mutex<Vec<Value>>>,
    /// `(path, body)` of every inference and token-count request.
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    /// How often the gateway search provider was called.
    search_hits: Arc<AtomicUsize>,
}

impl Backend {
    fn requests(&self) -> Vec<(String, Value)> {
        self.requests.lock().unwrap().clone()
    }

    fn search_hits(&self) -> usize {
        self.search_hits.load(Ordering::Relaxed)
    }

    fn script(&self, turns: Vec<Value>) {
        *self.turns.lock().unwrap() = turns;
        self.requests.lock().unwrap().clear();
    }
}

async fn upstream(
    State(backend): State<Backend>,
    OriginalUri(uri): OriginalUri,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let path = uri.path().to_owned();
    let streaming = body.get("stream").and_then(Value::as_bool) == Some(true);
    let inference = {
        let mut requests = backend.requests.lock().unwrap();
        requests.push((path.clone(), body));
        requests.iter().filter(|(path, _)| path == "/v1/messages").count()
    };
    if path.ends_with("/count_tokens") {
        return Json(json!({"input_tokens": 42})).into_response();
    }
    let turn = backend.turns.lock().unwrap().get(inference - 1).cloned();
    let turn = turn.unwrap_or_else(|| json!({"type": "error", "error": {"type": "api_error", "message": "exhausted"}}));
    if streaming {
        return axum::response::Response::builder()
            .header(http::header::CONTENT_TYPE, "text/event-stream")
            .body(axum::body::Body::from(server_tool_sse(&turn)))
            .unwrap();
    }
    Json(turn).into_response()
}

async fn search(State(backend): State<Backend>, uri: Uri) -> impl IntoResponse {
    backend.search_hits.fetch_add(1, Ordering::Relaxed);
    let query = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .find(|(key, _)| key == "query")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    Json(
        json!({"results": {"web": [{"url": format!("https://example.com/{query}"), "title": "Result",
        "description": "the page text"}]}}),
    )
}

/// A single-message Anthropic stream for the JSON `turn` a scripted upstream
/// would answer: every content block verbatim, then its terminal.
fn server_tool_sse(turn: &Value) -> String {
    let mut out = String::new();
    out.push_str("event: message_start\ndata: ");
    out.push_str(
        &json!({
            "type": "message_start",
            "message": {"id": turn["id"], "type": "message", "role": "assistant",
                        "content": [], "model": "test-model", "stop_reason": null,
                        "usage": {"input_tokens": 5, "output_tokens": 1}}
        })
        .to_string(),
    );
    out.push_str("\n\n");
    for (index, block) in turn["content"].as_array().unwrap().iter().enumerate() {
        let start = if block["type"] == "text" {
            json!({"type": "content_block_start", "index": index,
                   "content_block": {"type": "text", "text": ""}})
        } else {
            json!({"type": "content_block_start", "index": index, "content_block": block})
        };
        out.push_str("data: ");
        out.push_str(&start.to_string());
        out.push_str("\n\n");
        let delta = if block["type"] == "text" {
            json!({"type": "content_block_delta", "index": index,
                   "delta": {"type": "text_delta", "text": block["text"].as_str().unwrap_or("")}})
        } else {
            json!({"type": "content_block_delta", "index": index,
                   "delta": {"type": "input_json_delta", "partial_json": ""}})
        };
        out.push_str("data: ");
        out.push_str(&delta.to_string());
        out.push_str("\n\n");
        writeln!(out, "data: {}\n", json!({"type": "content_block_stop", "index": index})).unwrap();
    }
    out.push_str("data: ");
    out.push_str(
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": turn["stop_reason"], "stop_sequence": Value::Null},
            "usage": {"output_tokens": 3}
        })
        .to_string(),
    );
    out.push_str("\n\n");
    out.push_str("data: ");
    out.push_str(&json!({"type": "message_stop"}).to_string());
    out.push_str("\n\n");
    out
}

fn assistant(content: &[Value], stop_reason: &str) -> Value {
    json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "test-model", "content": content,
        "stop_reason": stop_reason, "stop_sequence": null, "usage": {"input_tokens": 5, "output_tokens": 2}})
}

struct Gateway {
    url: String,
    backend: Backend,
}

async fn gateway() -> Gateway {
    gateway_with(None, &GatewayToolMap::default()).await
}

/// A gateway whose upstream runs `upstream_types` itself, with `aliases` as
/// the operator alias map.
async fn gateway_with(upstream_types: Option<&str>, aliases: &GatewayToolMap) -> Gateway {
    let backend = Backend::default();
    let app = Router::new()
        .route("/v1/messages", post(upstream))
        .route("/v1/messages/count_tokens", post(upstream))
        .route("/v1/search", get(search))
        .with_state(backend.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let config = common::test_config(&backend_url);
    let state = common::test_state(&config);
    let client = Arc::new(reqwest::Client::new());
    let exec_ctx = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::clone(&client),
        backend_url.clone(),
    )
    .with_gateway_executor(Arc::new(WebSearchHandler::with_api_key(
        client,
        "local-key".to_owned(),
        &backend_url,
    )))
    .with_messages_result_key(MessagesResultKey::from_secret(b"an operator secret of thirty-two+ bytes").unwrap());
    let mut state = state;
    state.exec_ctx = Arc::new({
        let mut ctx = exec_ctx;
        ctx.messages_gateway_tools = aliases.clone();
        ctx.messages_upstream_server_tools = match upstream_types {
            Some(raw) => UpstreamServerTools::parse(raw).unwrap(),
            None => UpstreamServerTools::default(),
        };
        ctx
    });
    let (url, _server) = common::spawn_gateway(state).await;
    Gateway { url, backend }
}

async fn post_json(url: &str, body: &Value) -> (StatusCode, Value) {
    let response = reqwest::Client::new().post(url).json(body).send().await.unwrap();
    let status = response.status();
    (status, response.json().await.unwrap())
}

fn native_search() -> Value {
    json!({"type": "web_search_20250305", "name": "web_search"})
}

/// One gateway-run search, returned to the client: the request it answered,
/// the content the client received, and the messages of the upstream's last
/// round.
async fn searched_turn(gateway: &Gateway) -> (Value, Value, Value) {
    gateway.backend.script(vec![
        assistant(
            &[json!({"type": "tool_use", "id": "toolu_1", "name": "web_search", "input": {"query": "rust"}})],
            "tool_use",
        ),
        assistant(&[json!({"type": "text", "text": "Done."})], "end_turn"),
    ]);
    let first = json!({"model": "test-model", "max_tokens": 64,
        "messages": [{"role": "user", "content": "Search."}], "tools": [native_search()]});
    let (status, message) = post_json(&format!("{}/v1/messages", gateway.url), &first).await;
    assert_eq!(status, StatusCode::OK, "{message}");
    let types: Vec<&str> = message["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["server_tool_use", "web_search_tool_result", "text"]);
    let live = gateway.backend.requests().last().unwrap().1["messages"].clone();
    (first, message["content"].clone(), live)
}

fn follow_up(first: &Value, content: &Value, tools: Option<Value>) -> Value {
    let mut request = first.clone();
    let messages = request["messages"].as_array_mut().unwrap();
    messages.push(json!({"role": "assistant", "content": content}));
    messages.push(json!({"role": "user", "content": "Thanks."}));
    match tools {
        Some(tools) => request["tools"] = tools,
        None => {
            request.as_object_mut().unwrap().remove("tools");
        }
    }
    request
}

/// The upstream received the live rounds, then the final answer and the new question.
fn assert_rounds_restored(sent: &Value, live: &Value) {
    assert!(
        !sent.to_string().contains("srvtoolu_gw_"),
        "no gateway block reaches the upstream"
    );
    let sent = sent.as_array().unwrap();
    let live = live.as_array().unwrap();
    assert_eq!(sent[..live.len()], live[..], "the rounds the upstream saw");
    assert_eq!(sent[live.len()]["content"], json!([{"type": "text", "text": "Done."}]));
    assert_eq!(sent[live.len() + 1]["content"], "Thanks.");
}

#[tokio::test]
async fn returned_server_tool_blocks_reach_the_upstream_as_its_rounds() {
    let gateway = gateway().await;
    let (first, content, live) = searched_turn(&gateway).await;
    assert_eq!(live[1]["content"][0]["id"], "toolu_1");

    // Counting tokens sees what the upstream would: the rounds and the
    // rewritten declaration.
    gateway.backend.script(Vec::new());
    let (status, counted) = post_json(
        &format!("{}/v1/messages/count_tokens", gateway.url),
        &follow_up(&first, &content, Some(json!([native_search()]))),
    )
    .await;
    assert_eq!((status, counted), (StatusCode::OK, json!({"input_tokens": 42})));
    let (path, sent) = gateway.backend.requests()[0].clone();
    assert_eq!(path, "/v1/messages/count_tokens");
    assert_rounds_restored(&sent["messages"], &live);
    assert!(sent["tools"][0].get("type").is_none(), "native declaration rewritten");

    // A request that no longer declares a gateway tool takes the transparent
    // proxy, and its history still reaches the upstream as the rounds.
    gateway.backend.script(vec![assistant(
        &[json!({"type": "text", "text": "Welcome."})],
        "end_turn",
    )]);
    let (status, message) = post_json(
        &format!("{}/v1/messages", gateway.url),
        &follow_up(&first, &content, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{message}");
    assert_eq!(message["content"], json!([{"type": "text", "text": "Welcome."}]));
    let (_, sent) = gateway.backend.requests()[0].clone();
    assert_rounds_restored(&sent["messages"], &live);
    assert!(sent.get("tools").is_none(), "the proxied body is otherwise unchanged");

    // And the gateway loop does the same.
    gateway.backend.script(vec![assistant(
        &[json!({"type": "text", "text": "Welcome."})],
        "end_turn",
    )]);
    let (status, _) = post_json(
        &format!("{}/v1/messages", gateway.url),
        &follow_up(&first, &content, Some(json!([native_search()]))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_rounds_restored(&gateway.backend.requests()[0].1["messages"], &live);
}

#[tokio::test]
async fn altered_server_tool_content_is_refused_on_every_path() {
    let gateway = gateway().await;
    let (first, mut content, _) = searched_turn(&gateway).await;
    content[1]["content"][0]["encrypted_content"] =
        json!("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    for (path, tools) in [
        ("/v1/messages", Some(json!([native_search()]))),
        ("/v1/messages", None),
        ("/v1/messages/count_tokens", Some(json!([native_search()]))),
    ] {
        gateway.backend.script(Vec::new());
        let (status, error) = post_json(&format!("{}{path}", gateway.url), &follow_up(&first, &content, tools)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {error}");
        assert_eq!(error["type"], "error");
        assert_eq!(error["error"]["type"], "invalid_request_error");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("was modified or was not issued by this gateway"),
            "{path}: {error}"
        );
        assert!(
            gateway.backend.requests().is_empty(),
            "{path}: nothing reached the upstream"
        );
    }
}

/// The blocks an upstream running web search itself returns, with its own id.
fn upstream_search_blocks() -> Vec<Value> {
    vec![
        json!({"type": "server_tool_use", "id": "srvtoolu_01real", "name": "web_search",
               "input": {"query": "rust"}}),
        json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_01real", "content": [
            {"type": "web_search_result", "url": "https://example.com/rust", "title": "Rust",
             "encrypted_content": "the upstream keeps this"}]}),
    ]
}

async fn post_raw(url: &str, body: &Value) -> (StatusCode, String, String) {
    let response = reqwest::Client::new().post(url).json(body).send().await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    (status, content_type, response.text().await.unwrap())
}

#[tokio::test]
async fn upstream_run_native_search_passes_through_unchanged_without_the_provider() {
    let gateway = gateway_with(Some("web_search_20250305"), &GatewayToolMap::default()).await;
    let mut blocks = upstream_search_blocks();
    blocks.push(json!({"type": "text", "text": "Answer."}));
    gateway.backend.script(vec![assistant(&blocks, "end_turn")]);

    let request = json!({"model": "test-model", "max_tokens": 64,
        "messages": [{"role": "user", "content": "Search."}],
        "tools": [native_search()]});
    let (status, message) = post_json(&format!("{}/v1/messages", gateway.url), &request).await;
    assert_eq!(status, StatusCode::OK, "{message}");
    assert_eq!(message["content"], Value::Array(blocks.clone()));
    assert_eq!(message["stop_reason"], "end_turn");

    let recorded = gateway.backend.requests();
    assert_eq!(recorded.len(), 1, "one transparent inference, no gateway loop");
    let (path, sent) = &recorded[0];
    assert_eq!(path, "/v1/messages");
    assert_eq!(sent["tools"][0], native_search(), "the declaration is not rewritten");
    assert_eq!(sent["messages"], request["messages"], "the body is otherwise untouched");
    assert_eq!(gateway.backend.search_hits(), 0, "the gateway provider never ran");

    // count_tokens applies the same decision.
    gateway.backend.script(Vec::new());
    let (status, counted) = post_json(&format!("{}/v1/messages/count_tokens", gateway.url), &request).await;
    assert_eq!((status, counted), (StatusCode::OK, json!({"input_tokens": 42})));
    let (_, counted_body) = &gateway.backend.requests()[0];
    assert_eq!(counted_body["tools"][0], native_search());

    // A follow-up returning the upstream's blocks forwards them unchanged too.
    let mut next = request.clone();
    next["messages"] = json!([
        {"role": "user", "content": "Search."},
        {"role": "assistant", "content": blocks},
        {"role": "user", "content": "And?"}
    ]);
    gateway
        .backend
        .script(vec![assistant(&[json!({"type": "text", "text": "More."})], "end_turn")]);
    let (status, message) = post_json(&format!("{}/v1/messages", gateway.url), &next).await;
    assert_eq!(status, StatusCode::OK, "{message}");
    assert_eq!(message["content"], json!([{"type": "text", "text": "More."}]));
    let (_, sent) = gateway.backend.requests()[0].clone();
    assert_eq!(
        sent["messages"], next["messages"],
        "upstream blocks round-trip verbatim"
    );
    assert!(!sent.to_string().contains("srvtoolu_gw_"));
    assert_eq!(gateway.backend.search_hits(), 0);
}

#[tokio::test]
async fn upstream_run_native_search_streams_through_unchanged() {
    let gateway = gateway_with(Some("web_search_20250305"), &GatewayToolMap::default()).await;
    let mut blocks = upstream_search_blocks();
    blocks.push(json!({"type": "text", "text": "Answer."}));
    gateway.backend.script(vec![assistant(&blocks, "end_turn")]);

    let request = json!({"model": "test-model", "max_tokens": 64, "stream": true,
        "messages": [{"role": "user", "content": "Search."}],
        "tools": [native_search()]});
    let (status, content_type, body) = post_raw(&format!("{}/v1/messages", gateway.url), &request).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/event-stream"), "{content_type}");
    assert!(body.contains("srvtoolu_01real"), "{body}");
    assert!(body.contains("web_search_tool_result"), "{body}");
    assert!(body.contains("the upstream keeps this"));
    assert_eq!(gateway.backend.search_hits(), 0);
}

#[tokio::test]
async fn mixed_owners_run_only_the_gateway_call_and_replay_only_its_blocks() {
    let aliases = GatewayToolMap::from_pairs([("Search", "web_search")]);
    let gateway = gateway_with(Some("web_search_20250305"), &aliases).await;
    let upstream_blocks = upstream_search_blocks();
    let alias_call = json!({"type": "tool_use", "id": "toolu_g", "name": "Search",
                            "input": {"query": "rust"}});
    let mut first_turn = upstream_blocks.clone();
    first_turn.push(alias_call.clone());
    gateway.backend.script(vec![
        assistant(&first_turn, "tool_use"),
        assistant(&[json!({"type": "text", "text": "Done."})], "end_turn"),
    ]);

    let request = json!({"model": "test-model", "max_tokens": 64,
        "messages": [{"role": "user", "content": "Search twice."}],
        "tools": [native_search(),
                  {"name": "Search", "description": "gateway search",
                   "input_schema": {"type": "object", "properties": {}}}]});
    let (status, message) = post_json(&format!("{}/v1/messages", gateway.url), &request).await;
    assert_eq!(status, StatusCode::OK, "{message}");
    let types: Vec<&str> = message["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        ["server_tool_use", "web_search_tool_result", "text"],
        "the upstream blocks show; the gateway alias call stays hidden"
    );
    assert_eq!(message["content"][0]["id"], "srvtoolu_01real");
    assert_eq!(gateway.backend.search_hits(), 1, "only the alias call ran the provider");

    let recorded = gateway.backend.requests();
    assert_eq!(recorded.len(), 2, "the gateway loop ran one continuation round");
    assert_eq!(
        recorded[0].1["tools"][0],
        native_search(),
        "upstream declaration forwarded"
    );
    assert_eq!(recorded[0].1["tools"][1]["name"], "Search");
    let round_two = &recorded[1].1["messages"];
    assert_eq!(
        round_two[1]["content"],
        Value::Array(first_turn.clone()),
        "the continuation keeps the upstream blocks and the gateway tool_use"
    );
    assert_eq!(round_two[2]["content"][0]["tool_use_id"], "toolu_g");
    assert!(
        !round_two.to_string().contains("srvtoolu_gw_"),
        "the alias call was never shown, so it is not replayed"
    );

    // A later turn carries only what the client saw: the upstream blocks,
    // proxied verbatim because they carry the upstream's id.
    let mut follow_up = request.clone();
    follow_up["messages"] = json!([
        {"role": "user", "content": "Search twice."},
        {"role": "assistant", "content": message["content"].clone()},
        {"role": "user", "content": "Thanks."}
    ]);
    follow_up.as_object_mut().unwrap().remove("tools");
    gateway.backend.script(vec![assistant(
        &[json!({"type": "text", "text": "Welcome."})],
        "end_turn",
    )]);
    let (status, _) = post_json(&format!("{}/v1/messages", gateway.url), &follow_up).await;
    assert_eq!(status, StatusCode::OK);
    let (_, sent) = gateway.backend.requests()[0].clone();
    assert_eq!(sent["messages"], follow_up["messages"]);
    assert_eq!(gateway.backend.search_hits(), 1, "no provider call on the follow-up");
}
