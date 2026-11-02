//! Gateway-made server tool blocks sent back over HTTP (#409): `/v1/messages`
//! and `/v1/messages/count_tokens` give the upstream the rounds it saw, on the
//! gateway loop and on the transparent proxy, and refuse altered content.
#[allow(dead_code)]
mod common;

use std::sync::{Arc, Mutex};

use agentic_core::executor::{ConversationHandler, ExecutionContext, MessagesResultKey, ResponseHandler};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::WebSearchHandler;
use axum::extract::{OriginalUri, State};
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use http::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// The scripted upstream, its token counter, and the search backend.
#[derive(Clone, Default)]
struct Backend {
    turns: Arc<Mutex<Vec<Value>>>,
    /// `(path, body)` of every inference and token-count request.
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Backend {
    fn requests(&self) -> Vec<(String, Value)> {
        self.requests.lock().unwrap().clone()
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
) -> impl IntoResponse {
    let path = uri.path().to_owned();
    let inference = {
        let mut requests = backend.requests.lock().unwrap();
        requests.push((path.clone(), body));
        requests.iter().filter(|(path, _)| path == "/v1/messages").count()
    };
    if path.ends_with("/count_tokens") {
        return Json(json!({"input_tokens": 42}));
    }
    let turn = backend.turns.lock().unwrap().get(inference - 1).cloned();
    Json(turn.unwrap_or_else(|| json!({"type": "error", "error": {"type": "api_error", "message": "exhausted"}})))
}

async fn search(uri: Uri) -> impl IntoResponse {
    let query = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .find(|(key, _)| key == "query")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    Json(
        json!({"results": {"web": [{"url": format!("https://example.com/{query}"), "title": "Result",
        "description": "the page text"}]}}),
    )
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
    let mut state = common::test_state(&config);
    let client = Arc::new(reqwest::Client::new());
    state.exec_ctx = Arc::new(
        ExecutionContext::new(
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
        .with_messages_result_key(MessagesResultKey::from_secret(b"an operator secret of thirty-two+ bytes").unwrap()),
    );
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
