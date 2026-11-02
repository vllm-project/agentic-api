//! Gateway-run native web searches as the Messages client sees them (#409).
//!
//! A native `web_search_20250305` declaration runs in the gateway; the client
//! receives each executed call as a `server_tool_use` block and its
//! `web_search_tool_result`, over JSON and SSE. Sending the returned turn back
//! gives the upstream exactly the rounds it saw — the same ids and the same
//! `tool_result` text — without running the search again. One local listener
//! plays the scripted vLLM `/v1/messages`, the You.com search backend, and a
//! page to fetch.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use agentic_core::config::{DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS, WebFetchConfig};
use agentic_core::executor::{
    ConversationHandler, ExecutionContext, ExecutorError, MessagesRequestContext, MessagesResultKey, MessagesUpstream,
    ResponseHandler, run_messages_loop, run_messages_stream,
};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::{
    GatewayExecutorRegistration, ToolRegistry, WebFetchHandler, WebSearchHandler, registry_tools,
};
use agentic_core::types::messages::GatewayToolMap;
use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// The scripted upstream turns and the search backend.
#[derive(Clone, Default)]
struct Backend {
    turns: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    searches: Arc<Mutex<Vec<String>>>,
    search_fails: Arc<Mutex<bool>>,
}

fn assistant(content: &[Value], stop_reason: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": content, "stop_reason": stop_reason, "stop_sequence": null,
        "usage": {"input_tokens": 7, "output_tokens": 3}
    })
}

fn search_call(id: &str, query: &str) -> Value {
    json!({"type": "tool_use", "id": id, "name": "web_search", "input": {"query": query}})
}

fn text(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn sse_turn(message: &Value) -> String {
    let mut out = String::new();
    let mut push = |data: &Value| {
        writeln!(out, "event: {}\ndata: {data}\n", data["type"].as_str().unwrap()).unwrap();
    };
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    push(&json!({"type": "message_start", "message": start}));
    for (index, block) in message["content"].as_array().unwrap().iter().enumerate() {
        let mut initial = block.clone();
        let deltas = match block["type"].as_str().unwrap() {
            "tool_use" => {
                initial["input"] = json!({});
                vec![json!({"type": "input_json_delta", "partial_json": block["input"].to_string()})]
            }
            "thinking" => {
                initial["thinking"] = json!("");
                initial["signature"] = json!("");
                vec![
                    json!({"type": "thinking_delta", "thinking": block["thinking"]}),
                    json!({"type": "signature_delta", "signature": block["signature"]}),
                ]
            }
            _ => {
                initial["text"] = json!("");
                vec![json!({"type": "text_delta", "text": block["text"]})]
            }
        };
        push(&json!({"type": "content_block_start", "index": index, "content_block": initial}));
        for delta in deltas {
            push(&json!({"type": "content_block_delta", "index": index, "delta": delta}));
        }
        push(&json!({"type": "content_block_stop", "index": index}));
    }
    push(
        &json!({"type": "message_delta", "delta": {"stop_reason": message["stop_reason"], "stop_sequence": null},
        "usage": {"output_tokens": 3}}),
    );
    push(&json!({"type": "message_stop"}));
    out
}

async fn infer(State(backend): State<Backend>, Json(request): Json<Value>) -> Response {
    let round = {
        let mut requests = backend.requests.lock().unwrap();
        requests.push(request.clone());
        requests.len() - 1
    };
    let turn = backend.turns.lock().unwrap().get(round).cloned();
    let Some(turn) = turn else {
        return (StatusCode::BAD_GATEWAY, "script exhausted").into_response();
    };
    if request["stream"] == true {
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from(sse_turn(&turn)))
            .unwrap()
    } else {
        Json(turn).into_response()
    }
}

async fn search(State(backend): State<Backend>, uri: Uri) -> Response {
    let query = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .find(|(key, _)| key == "query")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    backend.searches.lock().unwrap().push(query.clone());
    if *backend.search_fails.lock().unwrap() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "search backend down").into_response();
    }
    Json(json!({
        "results": {
            "web": [
                {"url": format!("https://example.com/{query}"), "title": format!("About {query}"),
                 "description": "SEALED-DESCRIPTION", "snippets": [format!("{query}: the full snippet")],
                 "page_age": "2026-10-01"},
                {"url": "https://example.org/untitled", "snippets": ["SEALED-SNIPPET"]}
            ],
            "news": [{"url": "https://news.example/today", "title": "Today", "description": "SEALED-NEWS"}]
        },
        "metadata": {"query": query, "search_uuid": "uuid-1", "latency": 0.25}
    }))
    .into_response()
}

struct Harness {
    backend: Backend,
    url: String,
    exec_ctx: ExecutionContext,
}

impl Harness {
    fn script(&self, turns: Vec<Value>) {
        *self.backend.turns.lock().unwrap() = turns;
        self.backend.requests.lock().unwrap().clear();
    }

    fn upstream_requests(&self) -> Vec<Value> {
        self.backend.requests.lock().unwrap().clone()
    }

    fn searches(&self) -> Vec<String> {
        self.backend.searches.lock().unwrap().clone()
    }

    /// A gateway on the same backends that holds `key` (another replica, or
    /// this one after a restart).
    fn with_key(&self, key: MessagesResultKey) -> Self {
        Self {
            backend: self.backend.clone(),
            url: self.url.clone(),
            exec_ctx: self.exec_ctx.clone().with_messages_result_key(key),
        }
    }
}

const SECRET: &[u8] = b"an operator secret of at least thirty-two bytes";

fn configured_key() -> MessagesResultKey {
    MessagesResultKey::from_secret(SECRET).unwrap()
}

async fn harness() -> Harness {
    let backend = Backend::default();
    let app = Router::new()
        .route("/v1/messages", post(infer))
        .route("/v1/search", get(search))
        .route("/page", get(|| async { "PAGE-TEXT" }))
        .with_state(backend.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Arc::new(reqwest::Client::new());
    let exec_ctx = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::clone(&client),
        url.clone(),
    )
    .with_gateway_executor(Arc::new(WebSearchHandler::with_api_key(
        client,
        "test-key".to_owned(),
        &url,
    )))
    .with_messages_result_key(configured_key());
    Harness { backend, url, exec_ctx }
}

fn native_search() -> Value {
    json!({"type": "web_search_20250305", "name": "web_search"})
}

fn request(messages: &Value, tools: &Value, stream: bool) -> Value {
    json!({"model": "m", "max_tokens": 64, "stream": stream, "messages": messages, "tools": tools})
}

async fn registry(harness: &Harness, ctx: &MessagesRequestContext) -> ToolRegistry {
    let mut tools = registry_tools(ctx.tools(), &harness.exec_ctx.messages_gateway_tools);
    let mut executors = harness.exec_ctx.gateway_executors.clone();
    ToolRegistry::build_with_handlers(&mut tools, &mut executors)
        .await
        .unwrap()
}

/// Run one request; returns the content blocks the client assembles and the
/// final usage, or the error the gateway refused it with.
async fn run(harness: &Harness, request: Value) -> Result<(Value, Value), Box<ExecutorError>> {
    let stream = request["stream"] == true;
    let ctx = MessagesRequestContext::from_value(request).map_err(Box::new)?;
    let registry = registry(harness, &ctx).await;
    let upstream = MessagesUpstream::new(&harness.url, None, reqwest::header::HeaderMap::new());
    if !stream {
        let message = run_messages_loop(ctx, &registry, &harness.exec_ctx, &upstream)
            .await
            .map_err(Box::new)?
            .body;
        return Ok((message["content"].clone(), message["usage"].clone()));
    }
    let response = run_messages_stream(ctx, Arc::new(registry), Arc::new(harness.exec_ctx.clone()), upstream)
        .await
        .map_err(Box::new)?;
    let frames: Vec<String> = response.body.collect().await;
    let events: Vec<Value> = frames
        .concat()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    assert!(!events.iter().any(|event| event["type"] == "error"), "{events:?}");
    let usage = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .map(|event| event["usage"].clone())
        .unwrap();
    Ok((assembled(&events), usage))
}

/// The content a client assembles from the stream's events.
fn assembled(events: &[Value]) -> Value {
    let mut blocks: Vec<Value> = Vec::new();
    let mut inputs: Vec<String> = Vec::new();
    for event in events {
        let Some(index) = event["index"].as_u64().map(|index| usize::try_from(index).unwrap()) else {
            continue;
        };
        match event["type"].as_str().unwrap() {
            "content_block_start" => {
                assert_eq!(index, blocks.len(), "contiguous indices: {events:?}");
                blocks.push(event["content_block"].clone());
                inputs.push(String::new());
            }
            "content_block_delta" => {
                let (field, piece) = match event["delta"]["type"].as_str().unwrap() {
                    "text_delta" => ("text", "text"),
                    "thinking_delta" => ("thinking", "thinking"),
                    "signature_delta" => ("signature", "signature"),
                    "input_json_delta" => {
                        inputs[index].push_str(event["delta"]["partial_json"].as_str().unwrap());
                        continue;
                    }
                    other => panic!("unexpected delta {other}"),
                };
                let joined = format!(
                    "{}{}",
                    blocks[index][field].as_str().unwrap(),
                    event["delta"][piece].as_str().unwrap()
                );
                blocks[index][field] = json!(joined);
            }
            _ => {}
        }
    }
    for (block, input) in blocks.iter_mut().zip(inputs) {
        if !input.is_empty() {
            block["input"] = serde_json::from_str(&input).unwrap();
        }
    }
    Value::Array(blocks)
}

/// Each block's type with the id it carries: a compact view of the turn.
fn shape(content: &Value) -> Vec<String> {
    content
        .as_array()
        .unwrap()
        .iter()
        .map(|block| match block["type"].as_str().unwrap() {
            "server_tool_use" => format!("server_tool_use {}", block["id"].as_str().unwrap()),
            "web_search_tool_result" => format!(
                "web_search_tool_result {} {}",
                block["tool_use_id"].as_str().unwrap(),
                block["content"]["error_code"].as_str().map_or_else(
                    || format!("{} results", block["content"].as_array().unwrap().len()),
                    str::to_owned
                )
            ),
            "text" => format!("text {}", block["text"].as_str().unwrap()),
            "thinking" => format!("thinking {}", block["thinking"].as_str().unwrap()),
            "web_fetch_tool_result" => format!("web_fetch_tool_result {}", block["tool_use_id"].as_str().unwrap()),
            other => format!("{other} {}", block["id"].as_str().unwrap_or_default()),
        })
        .collect()
}

/// Send the returned turn back with a follow-up question, and check that the
/// upstream receives exactly the rounds the first request produced.
async fn assert_replays_the_original_rounds(harness: &Harness, first: &Value, content: &Value, stream: bool) {
    let live = harness.upstream_requests().last().unwrap()["messages"].clone();
    let searched = harness.searches().len();
    harness.script(vec![assistant(&[text("You're welcome.")], "end_turn")]);
    let mut messages = first["messages"].as_array().unwrap().clone();
    messages.push(json!({"role": "assistant", "content": content}));
    messages.push(json!({"role": "user", "content": "Thanks."}));

    let (follow_up, _) = run(harness, request(&json!(messages), &first["tools"], stream))
        .await
        .unwrap();

    assert_eq!(shape(&follow_up), ["text You're welcome."]);
    let sent = harness.upstream_requests()[0]["messages"].clone();
    let sent = sent.as_array().unwrap();
    let live = live.as_array().unwrap();
    assert_eq!(
        sent[..live.len()],
        live[..],
        "the same ids and tool_result text as the live rounds"
    );
    // The final round is everything after the last result block.
    let final_round: Vec<Value> = content
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .take_while(|block| !block["type"].as_str().unwrap().ends_with("_tool_result"))
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    assert_eq!(sent[live.len()], json!({"role": "assistant", "content": final_round}));
    assert_eq!(sent[live.len() + 1], json!({"role": "user", "content": "Thanks."}));
    assert_eq!(sent.len(), live.len() + 2);
    assert_eq!(harness.searches().len(), searched, "the search is not run again");
}

#[tokio::test]
async fn native_search_calls_are_shown_and_replay_to_the_original_rounds() {
    for stream in [false, true] {
        let harness = harness().await;
        harness.script(vec![
            assistant(&[text("Searching."), search_call("t1", "rust")], "tool_use"),
            assistant(&[text("Done.")], "end_turn"),
        ]);
        let first = request(
            &json!([{"role": "user", "content": "What is new in Rust?"}]),
            &json!([native_search()]),
            stream,
        );

        let (content, usage) = run(&harness, first.clone()).await.unwrap();

        assert_eq!(
            shape(&content),
            [
                "text Searching.",
                "server_tool_use srvtoolu_gw_t1",
                "web_search_tool_result srvtoolu_gw_t1 3 results",
                "text Done."
            ],
            "stream={stream}"
        );
        assert_eq!(content[1]["name"], "web_search");
        assert_eq!(content[1]["input"], json!({"query": "rust"}));
        let results = content[2]["content"].as_array().unwrap();
        assert_eq!(results[0]["url"], "https://example.com/rust");
        assert_eq!(results[0]["title"], "About rust");
        assert_eq!(results[0]["page_age"], "2026-10-01");
        assert_eq!(results[1]["title"], "");
        assert_eq!(results[1]["page_age"], Value::Null);
        assert_eq!(results[2]["url"], "https://news.example/today");
        assert!(
            results.iter().all(|result| result["type"] == "web_search_result"
                && !result["encrypted_content"].as_str().unwrap().is_empty())
        );
        let shown = content.to_string();
        for sealed in [
            "SEALED-DESCRIPTION",
            "SEALED-SNIPPET",
            "SEALED-NEWS",
            "the full snippet",
            "uuid-1",
        ] {
            assert!(!shown.contains(sealed), "{sealed} travels only sealed: {shown}");
        }
        assert_eq!(
            usage["server_tool_use"],
            json!({"web_search_requests": 1}),
            "stream={stream}"
        );
        assert_eq!(usage["input_tokens"], 14, "both rounds count");

        // The model read the results without the provider's bookkeeping.
        let fed_back = harness.upstream_requests()[1]["messages"][2]["content"][0].clone();
        assert_eq!(fed_back["tool_use_id"], "t1");
        assert_eq!(fed_back["is_error"], false);
        let read: Value = serde_json::from_str(fed_back["content"].as_str().unwrap()).unwrap();
        assert_eq!(read["query"], "rust");
        assert_eq!(read["queries"], json!(["rust"]));
        assert_eq!(read["results"]["web"][0]["description"], "SEALED-DESCRIPTION");
        assert_eq!(read["results"]["news"][0]["description"], "SEALED-NEWS");
        assert!(read.get("metadata").is_none());

        assert_replays_the_original_rounds(&harness, &first, &content, stream).await;
        assert_eq!(harness.searches(), ["rust"]);
    }
}

#[tokio::test]
async fn parallel_and_sequential_calls_keep_their_rounds() {
    for stream in [false, true] {
        let harness = harness().await;
        harness.script(vec![
            assistant(&[search_call("t1", "alpha"), search_call("t2", "beta")], "tool_use"),
            assistant(&[text("Next."), search_call("t3", "gamma")], "tool_use"),
            assistant(&[text("Done.")], "end_turn"),
        ]);
        let first = request(
            &json!([{"role": "user", "content": "Compare."}]),
            &json!([native_search()]),
            stream,
        );

        let (content, usage) = run(&harness, first.clone()).await.unwrap();

        assert_eq!(
            shape(&content),
            [
                "server_tool_use srvtoolu_gw_t1",
                "server_tool_use srvtoolu_gw_t2",
                "web_search_tool_result srvtoolu_gw_t1 3 results",
                "web_search_tool_result srvtoolu_gw_t2 3 results",
                "text Next.",
                "server_tool_use srvtoolu_gw_t3",
                "web_search_tool_result srvtoolu_gw_t3 3 results",
                "text Done."
            ],
            "stream={stream}"
        );
        assert_eq!(usage["server_tool_use"]["web_search_requests"], 3);
        assert_replays_the_original_rounds(&harness, &first, &content, stream).await;
    }
}

/// A block after a native call in the same round keeps its place: the stream
/// holds it until the round's end, then shows the call, the block, the result.
#[tokio::test]
async fn a_block_after_a_native_call_keeps_its_place() {
    for stream in [false, true] {
        let harness = harness().await;
        harness.script(vec![
            assistant(
                &[text("Before."), search_call("t1", "rust"), text("After.")],
                "tool_use",
            ),
            assistant(&[text("Done.")], "end_turn"),
        ]);
        let first = request(
            &json!([{"role": "user", "content": "Go."}]),
            &json!([native_search()]),
            stream,
        );

        let (content, _) = run(&harness, first.clone()).await.unwrap();

        assert_eq!(
            shape(&content),
            [
                "text Before.",
                "server_tool_use srvtoolu_gw_t1",
                "text After.",
                "web_search_tool_result srvtoolu_gw_t1 3 results",
                "text Done."
            ],
            "stream={stream}"
        );
        assert_replays_the_original_rounds(&harness, &first, &content, stream).await;
    }
}

/// The final round follows the shown rounds whatever its content: an empty
/// list, a bare string, or none at all.
#[tokio::test]
async fn a_final_round_without_blocks_keeps_the_shown_rounds() {
    for (case, content, stream) in [
        ("empty list", Some(json!([])), false),
        ("empty list", Some(json!([])), true),
        ("bare string", Some(json!("Done.")), false),
        ("no content", None, false),
    ] {
        let harness = harness().await;
        let mut last = assistant(&[], "end_turn");
        match content {
            Some(content) => last["content"] = content,
            None => {
                last.as_object_mut().unwrap().remove("content");
            }
        }
        harness.script(vec![assistant(&[search_call("t1", "rust")], "tool_use"), last]);

        let (content, usage) = run(
            &harness,
            request(
                &json!([{"role": "user", "content": "Go."}]),
                &json!([native_search()]),
                stream,
            ),
        )
        .await
        .unwrap();

        let mut expected = vec![
            "server_tool_use srvtoolu_gw_t1",
            "web_search_tool_result srvtoolu_gw_t1 3 results",
        ];
        if case == "bare string" {
            expected.push("text Done.");
        }
        assert_eq!(shape(&content), expected, "{case} stream={stream}");
        assert_eq!(usage["server_tool_use"]["web_search_requests"], 1, "{case}");
    }
}

/// A reply that is not a message object is refused like malformed JSON, also
/// after an executed round.
#[tokio::test]
async fn a_reply_that_is_not_an_object_is_refused() {
    let harness = harness().await;
    harness.script(vec![
        assistant(&[search_call("t1", "rust")], "tool_use"),
        json!(["not", "a", "message"]),
    ]);

    let error = run(
        &harness,
        request(
            &json!([{"role": "user", "content": "Go."}]),
            &json!([native_search()]),
            false,
        ),
    )
    .await
    .unwrap_err();

    assert!(matches!(*error, ExecutorError::JsonError(_)), "{error:?}");
}

/// Thinking blocks keep their place and signature in the shown turn, and the
/// replayed rounds carry them back to the upstream unchanged.
#[tokio::test]
async fn thinking_blocks_keep_their_place_across_rounds() {
    let thinking = |text: &str, signature: &str| json!({"type": "thinking", "thinking": text, "signature": signature});
    for stream in [false, true] {
        let harness = harness().await;
        harness.script(vec![
            assistant(
                &[thinking("Search first.", "sig-1"), search_call("t1", "rust")],
                "tool_use",
            ),
            assistant(&[thinking("Now answer.", "sig-2"), text("Done.")], "end_turn"),
        ]);
        let first = request(
            &json!([{"role": "user", "content": "Go."}]),
            &json!([native_search()]),
            stream,
        );

        let (content, _) = run(&harness, first.clone()).await.unwrap();

        assert_eq!(
            shape(&content),
            [
                "thinking Search first.",
                "server_tool_use srvtoolu_gw_t1",
                "web_search_tool_result srvtoolu_gw_t1 3 results",
                "thinking Now answer.",
                "text Done."
            ],
            "stream={stream}"
        );
        assert_eq!(content[0]["signature"], "sig-1");
        assert_eq!(content[3]["signature"], "sig-2");
        assert_replays_the_original_rounds(&harness, &first, &content, stream).await;
    }
}

/// A native call in a round the gateway does not run stays hidden: a round cut
/// off by `max_tokens`, or one that ends its turn instead of asking for the
/// call. The blocks around it keep their order.
#[tokio::test]
async fn a_native_call_in_a_round_that_does_not_run_stays_hidden() {
    for stop_reason in ["max_tokens", "end_turn"] {
        for stream in [false, true] {
            let harness = harness().await;
            harness.script(vec![assistant(
                &[text("Let me search."), search_call("t1", "rust"), text("More.")],
                stop_reason,
            )]);

            let (content, usage) = run(
                &harness,
                request(
                    &json!([{"role": "user", "content": "Go."}]),
                    &json!([native_search()]),
                    stream,
                ),
            )
            .await
            .unwrap();

            assert_eq!(
                shape(&content),
                ["text Let me search.", "text More."],
                "{stop_reason} stream={stream}"
            );
            assert!(usage.get("server_tool_use").is_none(), "{usage}");
            assert!(harness.searches().is_empty(), "the search never ran");
        }
    }
}

/// A native call and a hidden gateway call in one round both run: here a
/// native `web_fetch` and a plain function named `web_search`. Only the native
/// call is shown and counted, so the returned turn replays that round with the
/// native call alone, its `tool_result` unchanged.
#[tokio::test]
async fn a_hidden_call_beside_a_native_call_is_not_replayed() {
    for stream in [false, true] {
        let mut harness = harness().await;
        harness.exec_ctx = harness
            .exec_ctx
            .clone()
            .with_gateway_executor(GatewayExecutorRegistration::WebFetch(Arc::new(
                WebFetchHandler::from_config(
                    &WebFetchConfig::default().with_allow_private_networks(true),
                    DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS,
                ),
            )));
        let page = format!("{}/page", harness.url);
        let fetch_call = json!({"type": "tool_use", "id": "t1", "name": "web_fetch", "input": {"url": page}});
        harness.script(vec![
            assistant(&[fetch_call, search_call("t2", "rust")], "tool_use"),
            assistant(&[text("Done.")], "end_turn"),
        ]);
        let tools = json!([
            {"type": "web_fetch_20250910", "name": "web_fetch"},
            {"name": "web_search", "input_schema": {"type": "object"}}
        ]);
        let first = request(
            &json!([{"role": "user", "content": format!("Read {page}, then search.")}]),
            &tools,
            stream,
        );

        let (content, usage) = run(&harness, first.clone()).await.unwrap();

        assert_eq!(
            shape(&content),
            [
                "server_tool_use srvtoolu_gw_t1",
                "web_fetch_tool_result srvtoolu_gw_t1",
                "text Done."
            ],
            "stream={stream}"
        );
        assert_eq!(content[1]["content"]["content"]["source"]["data"], "PAGE-TEXT");
        assert_eq!(usage["server_tool_use"], json!({"web_fetch_requests": 1}));
        assert_eq!(harness.searches(), ["rust"], "the hidden search ran");
        let live = harness.upstream_requests()[1]["messages"].clone();
        assert_eq!(live[2]["content"][1]["tool_use_id"], "t2");

        harness.script(vec![assistant(&[text("You're welcome.")], "end_turn")]);
        let mut messages = first["messages"].as_array().unwrap().clone();
        messages.push(json!({"role": "assistant", "content": content}));
        messages.push(json!({"role": "user", "content": "Thanks."}));
        run(&harness, request(&json!(messages), &tools, stream)).await.unwrap();

        let sent = harness.upstream_requests()[0]["messages"].clone();
        assert_eq!(
            sent[1]["content"],
            json!([live[1]["content"][0]]),
            "the native call alone"
        );
        assert_eq!(
            sent[2]["content"],
            json!([live[2]["content"][0]]),
            "its tool_result unchanged"
        );
        assert_eq!(sent[3], json!({"role": "assistant", "content": [text("Done.")]}));
    }
}

/// A gateway call that shares a round with a client call does not run, so it
/// stays hidden; the client call reaches the client as before.
#[tokio::test]
async fn a_native_call_beside_a_client_call_stays_hidden() {
    for stream in [false, true] {
        let harness = harness().await;
        let client_call = json!({"type": "tool_use", "id": "c1", "name": "lookup", "input": {"key": "k"}});
        harness.script(vec![assistant(
            &[text("Both."), search_call("t1", "rust"), client_call.clone()],
            "tool_use",
        )]);
        let tools = json!([native_search(), {"name": "lookup", "input_schema": {"type": "object"}}]);

        let (content, usage) = run(
            &harness,
            request(&json!([{"role": "user", "content": "Go."}]), &tools, stream),
        )
        .await
        .unwrap();

        assert_eq!(shape(&content), ["text Both.", "tool_use c1"], "stream={stream}");
        assert_eq!(content[1]["input"], json!({"key": "k"}));
        assert!(usage.get("server_tool_use").is_none(), "{usage}");
        assert!(harness.searches().is_empty(), "the search never ran");
    }
}

/// Calls the client declared as ordinary functions stay hidden: an operator
/// alias, and a plain function named `web_search`.
#[tokio::test]
async fn alias_and_plain_function_calls_stay_hidden() {
    for (name, tool) in [
        (
            "WebSearch",
            json!({"name": "WebSearch", "input_schema": {"type": "object"}}),
        ),
        (
            "web_search",
            json!({"name": "web_search", "input_schema": {"type": "object"}}),
        ),
    ] {
        for stream in [false, true] {
            let mut harness = harness().await;
            harness.exec_ctx.messages_gateway_tools = GatewayToolMap::from_env_str("WebSearch=web_search");
            harness.script(vec![
                assistant(
                    &[json!({"type": "tool_use", "id": "t1", "name": name, "input": {"query": "rust"}})],
                    "tool_use",
                ),
                assistant(&[text("Done.")], "end_turn"),
            ]);

            let (content, usage) = run(
                &harness,
                request(&json!([{"role": "user", "content": "Go."}]), &json!([tool]), stream),
            )
            .await
            .unwrap();

            assert_eq!(shape(&content), ["text Done."], "{name} stream={stream}");
            assert!(usage.get("server_tool_use").is_none(), "{usage}");
            assert_eq!(harness.searches(), ["rust"], "the gateway still ran it");
        }
    }
}

#[tokio::test]
async fn a_failed_search_shows_its_documented_code() {
    for stream in [false, true] {
        let harness = harness().await;
        *harness.backend.search_fails.lock().unwrap() = true;
        harness.script(vec![
            assistant(&[search_call("t1", "rust")], "tool_use"),
            assistant(&[text("Sorry.")], "end_turn"),
        ]);
        let first = request(
            &json!([{"role": "user", "content": "Go."}]),
            &json!([native_search()]),
            stream,
        );

        let (content, usage) = run(&harness, first.clone()).await.unwrap();

        assert_eq!(
            shape(&content),
            [
                "server_tool_use srvtoolu_gw_t1",
                "web_search_tool_result srvtoolu_gw_t1 unavailable",
                "text Sorry."
            ]
        );
        assert!(
            !content.to_string().contains("search backend down"),
            "executor detail is logged only"
        );
        assert!(
            usage.get("server_tool_use").is_none(),
            "a failed search is not billed: {usage}"
        );
        let fed_back = harness.upstream_requests()[1]["messages"][2]["content"][0].clone();
        assert_eq!(fed_back["is_error"], true);
        assert_eq!(
            fed_back["content"],
            "web_search is unavailable; search did not complete"
        );
        *harness.backend.search_fails.lock().unwrap() = false;
        assert_replays_the_original_rounds(&harness, &first, &content, stream).await;
    }
}

/// The search content does not open unless it is exactly what the gateway
/// sealed for that call: any change is refused before the upstream is called.
#[tokio::test]
async fn altered_search_content_is_refused() {
    let harness = harness().await;
    harness.script(vec![
        assistant(&[search_call("t1", "rust")], "tool_use"),
        assistant(&[text("Done.")], "end_turn"),
    ]);
    let first = request(
        &json!([{"role": "user", "content": "Go."}]),
        &json!([native_search()]),
        false,
    );
    let (content, _) = run(&harness, first.clone()).await.unwrap();

    let mut flipped = content.clone();
    let envelope = flipped[1]["content"][0]["encrypted_content"]
        .as_str()
        .unwrap()
        .to_owned();
    let last = if envelope.ends_with('A') { "B" } else { "A" };
    flipped[1]["content"][0]["encrypted_content"] = json!(format!("{}{last}", &envelope[..envelope.len() - 1]));
    let mut reordered = content.clone();
    reordered[1]["content"].as_array_mut().unwrap().swap(0, 1);
    let mut requery = content.clone();
    requery[0]["input"]["query"] = json!("go");
    let mut forged = content.clone();
    forged[1]["content"][0]["encrypted_content"] = json!("bm90IGFuIGVudmVsb3Bl");
    for stream in [false, true] {
        for (name, altered) in [
            ("flipped", &flipped),
            ("reordered", &reordered),
            ("requery", &requery),
            ("forged", &forged),
        ] {
            harness.script(vec![assistant(&[text("unused")], "end_turn")]);
            let mut messages = first["messages"].as_array().unwrap().clone();
            messages.push(json!({"role": "assistant", "content": altered}));
            messages.push(json!({"role": "user", "content": "Thanks."}));

            let error = run(&harness, request(&json!(messages), &first["tools"], stream))
                .await
                .unwrap_err();

            assert!(
                matches!(*error, ExecutorError::InvalidRequest(_)),
                "{name} stream={stream}: {error:?}"
            );
            assert!(
                error
                    .to_string()
                    .contains("was modified or was not issued by this gateway"),
                "{name}: {error}"
            );
            assert!(
                harness.upstream_requests().is_empty(),
                "{name}: nothing reached the upstream"
            );
        }
    }
}

/// Results sealed under a key this gateway no longer holds replay as a note,
/// so a restart without a configured key never breaks a conversation; a
/// gateway holding the same configured key replays them exactly.
#[tokio::test]
async fn the_key_decides_whether_results_replay() {
    let harness = harness().await;
    harness.script(vec![
        assistant(&[search_call("t1", "rust")], "tool_use"),
        assistant(&[text("Done.")], "end_turn"),
    ]);
    let first = request(
        &json!([{"role": "user", "content": "Go."}]),
        &json!([native_search()]),
        false,
    );
    let (content, _) = run(&harness, first.clone()).await.unwrap();

    let replica = harness.with_key(configured_key());
    assert_replays_the_original_rounds(&replica, &first, &content, false).await;

    let restarted =
        harness.with_key(MessagesResultKey::from_secret(b"another operator secret, also 32+ bytes").unwrap());
    restarted.script(vec![assistant(&[text("Searching again.")], "end_turn")]);
    let mut messages = first["messages"].as_array().unwrap().clone();
    messages.push(json!({"role": "assistant", "content": content}));
    messages.push(json!({"role": "user", "content": "And now?"}));
    run(&restarted, request(&json!(messages), &first["tools"], true))
        .await
        .unwrap();

    let sent = restarted.upstream_requests()[0]["messages"].clone();
    assert_eq!(sent[1]["content"][0]["id"], "t1", "the round is restored");
    let note = &sent[2]["content"][0];
    assert_eq!(note["tool_use_id"], "t1");
    assert_eq!(note["is_error"], true);
    assert!(
        note["content"].as_str().unwrap().contains("no longer available"),
        "{note}"
    );
}
