//! The gateway-executed `web_fetch` tool through the Messages loops (#408).
//!
//! A native `web_fetch_20250910` declaration is rewritten for the upstream,
//! the fetch runs in the gateway against the page the user linked, the call
//! never reaches the client, and the documented refusals reach the model as
//! error `tool_result`s over the JSON and SSE loops alike. One local listener
//! plays both the mock vLLM `/v1/messages` and the pages the model may fetch.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use agentic_core::config::{DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS, WebFetchConfig};
use agentic_core::executor::{
    ConversationHandler, ExecutionContext, MessagesRequestContext, MessagesUpstream, ResponseHandler,
    run_messages_loop, run_messages_stream,
};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::registry_tools;
use agentic_core::tool::{GatewayExecutorRegistration, GatewayExecutors, ToolRegistry, WebFetchHandler};
use agentic_core::types::messages::GatewayToolMap;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// The mock upstream (scripted assistant turns) and the page origin.
#[derive(Clone, Default)]
struct Backend {
    /// Scripted assistant turns, one per upstream round.
    turns: Arc<Mutex<Vec<Value>>>,
    /// Upstream request bodies, in arrival order.
    requests: Arc<Mutex<Vec<Value>>>,
    /// Page paths the origin served, in arrival order.
    pages: Arc<Mutex<Vec<String>>>,
    /// The listener's port, for pages that name the origin by address.
    port: Arc<AtomicU16>,
}

fn assistant(content: &[Value], stop_reason: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": content, "stop_reason": stop_reason, "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 2}
    })
}

fn fetch_call(id: &str, url: &str) -> Value {
    json!({"type": "tool_use", "id": id, "name": "web_fetch", "input": {"url": url}})
}

fn done() -> Value {
    assistant(&[json!({"type": "text", "text": "Done."})], "end_turn")
}

fn sse_turn(message: &Value) -> String {
    let mut out = String::new();
    let mut push = |event: &str, data: &Value| {
        write!(out, "event: {event}\ndata: {data}\n\n").unwrap();
    };
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    push("message_start", &json!({"type": "message_start", "message": start}));
    for (index, block) in message["content"].as_array().unwrap().iter().enumerate() {
        let mut initial = block.clone();
        let delta = if block["type"] == "tool_use" {
            initial["input"] = json!({});
            json!({"type": "input_json_delta", "partial_json": block["input"].to_string()})
        } else {
            initial["text"] = json!("");
            json!({"type": "text_delta", "text": block["text"]})
        };
        push(
            "content_block_start",
            &json!({"type": "content_block_start", "index": index, "content_block": initial}),
        );
        push(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": index, "delta": delta}),
        );
        push(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        );
    }
    push(
        "message_delta",
        &json!({"type": "message_delta", "delta": {"stop_reason": message["stop_reason"], "stop_sequence": null},
            "usage": {"output_tokens": 2}}),
    );
    push("message_stop", &json!({"type": "message_stop"}));
    out
}

async fn infer(State(backend): State<Backend>, Json(request): Json<Value>) -> Response {
    let round = {
        let mut requests = backend.requests.lock().unwrap();
        requests.push(request.clone());
        requests.len() - 1
    };
    let turn = backend
        .turns
        .lock()
        .unwrap()
        .get(round)
        .cloned()
        .unwrap_or_else(|| json!({"type": "error", "error": {"type": "api_error", "message": "mock exhausted"}}));
    if request["stream"] == true {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from(sse_turn(&turn)))
            .unwrap()
    } else {
        Json(turn).into_response()
    }
}

async fn page(State(backend): State<Backend>, Path(name): Path<String>) -> Response {
    backend.pages.lock().unwrap().push(name.clone());
    let origin = format!("http://127.0.0.1:{}", backend.port.load(Ordering::Relaxed));
    match name.as_str() {
        "doc.html" => (
            [("content-type", "text/html; charset=utf-8")],
            "<html><head><title>Page Title</title></head><body><h1>Hello</h1><p>from the <b>page</b></p></body></html>",
        )
            .into_response(),
        "plain.txt" => ([("content-type", "text/plain")], "plain text body").into_response(),
        "links.html" => (
            [("content-type", "text/html")],
            format!("<html><body><p>Next: {origin}/page/plain.txt</p></body></html>"),
        )
            .into_response(),
        "redirect" => (StatusCode::FOUND, [("location", "/page/doc.html")]).into_response(),
        "redirect-to-ip" => (StatusCode::FOUND, [("location", format!("{origin}/page/doc.html"))]).into_response(),
        "redirect-to-credentials" => (
            StatusCode::FOUND,
            [(
                "location",
                format!("{}/page/doc.html", origin.replace("http://", "http://user:pw@")),
            )],
        )
            .into_response(),
        "loop" => (StatusCode::FOUND, [("location", "/page/loop")]).into_response(),
        "paper.pdf" => ([("content-type", "application/pdf")], "%PDF-1.4").into_response(),
        "busy" => (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

struct Harness {
    backend: Backend,
    url: String,
    exec_ctx: ExecutionContext,
}

impl Harness {
    /// Install the scripted assistant turns, one per upstream round.
    fn script(&self, turns: Vec<Value>) {
        *self.backend.turns.lock().unwrap() = turns;
    }

    fn page_url(&self, name: &str) -> String {
        format!("{}/page/{name}", self.url)
    }

    /// Page paths the origin served, in arrival order.
    fn served_pages(&self) -> Vec<String> {
        self.backend.pages.lock().unwrap().clone()
    }

    /// Upstream request bodies, in arrival order.
    fn upstream_requests(&self) -> Vec<Value> {
        self.backend.requests.lock().unwrap().clone()
    }

    /// The `tool_result` blocks the loop fed back in the upstream request `round`.
    fn fed_back_results(&self, round: usize) -> Vec<Value> {
        let requests = self.upstream_requests();
        let messages = requests[round]["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        assert_eq!(last["role"], "user");
        last["content"].as_array().unwrap().clone()
    }
}

async fn harness(config: &WebFetchConfig) -> Harness {
    let backend = Backend::default();
    let app = Router::new()
        .route("/v1/messages", post(infer))
        .route("/page/{name}", get(page))
        .with_state(backend.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    backend.port.store(addr.port(), Ordering::Relaxed);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("http://{addr}");
    let exec_ctx = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        url.clone(),
    )
    .with_gateway_executor(GatewayExecutorRegistration::WebFetch(Arc::new(
        WebFetchHandler::from_config(config, DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS),
    )));
    Harness { backend, url, exec_ctx }
}

fn loopback_config() -> WebFetchConfig {
    WebFetchConfig::default().with_allow_private_networks(true)
}

fn request(user_text: &str, tool: &Value, stream: bool) -> Value {
    json!({
        "model": "m", "max_tokens": 64, "stream": stream,
        "messages": [{"role": "user", "content": user_text}],
        "tools": [tool]
    })
}

fn native_fetch(extra: &Value) -> Value {
    let mut tool = json!({"type": "web_fetch_20250910", "name": "web_fetch"});
    for (key, value) in extra.as_object().unwrap() {
        tool[key] = value.clone();
    }
    tool
}

async fn registry(harness: &Harness, ctx: &MessagesRequestContext) -> ToolRegistry {
    let mut tools = registry_tools(ctx.tools(), &GatewayToolMap::default());
    let mut executors = harness.exec_ctx.gateway_executors.clone();
    ToolRegistry::build_with_handlers(&mut tools, &mut executors)
        .await
        .expect("registry")
}

/// Run the JSON loop and return the client-visible message.
async fn run_json(harness: &Harness, request: Value) -> Value {
    let ctx = MessagesRequestContext::from_value(request).expect("context");
    let registry = registry(harness, &ctx).await;
    let upstream = MessagesUpstream::new(&harness.url, None, reqwest::header::HeaderMap::new());
    run_messages_loop(ctx, &registry, &harness.exec_ctx, &upstream)
        .await
        .expect("loop")
        .body
}

/// Run the SSE loop and return the concatenated client-visible frames.
async fn run_sse(harness: &Harness, request: Value) -> String {
    let ctx = MessagesRequestContext::from_value(request).expect("context");
    let registry = registry(harness, &ctx).await;
    let upstream = MessagesUpstream::new(&harness.url, None, reqwest::header::HeaderMap::new());
    let response = run_messages_stream(ctx, Arc::new(registry), Arc::new(harness.exec_ctx.clone()), upstream)
        .await
        .expect("stream");
    let mut body = response.body;
    let mut out = String::new();
    while let Some(frame) = body.next().await {
        out.push_str(&frame);
    }
    out
}

fn result_content(result: &Value) -> Value {
    serde_json::from_str(result["content"].as_str().expect("string tool_result content")).expect("JSON content")
}

#[tokio::test]
async fn web_fetch_runs_in_the_gateway_and_stays_hidden_over_json() {
    let harness = harness(&loopback_config()).await;
    let page_url = harness.page_url("doc.html");
    harness.script(vec![
        assistant(
            &[
                json!({"type": "text", "text": "Fetching."}),
                fetch_call("t1", &page_url),
            ],
            "tool_use",
        ),
        done(),
    ]);

    let message = run_json(
        &harness,
        request(
            &format!("Summarize {page_url} please"),
            &native_fetch(&json!({"max_uses": 2})),
            false,
        ),
    )
    .await;

    assert_eq!(message["stop_reason"], "end_turn", "{message}");
    assert_eq!(message["content"], json!([{"type": "text", "text": "Done."}]));
    assert!(
        !message.to_string().contains("web_fetch"),
        "gateway calls stay hidden: {message}"
    );
    assert_eq!(harness.served_pages(), vec!["doc.html"]);

    let requests = harness.upstream_requests();
    assert_eq!(requests.len(), 2);
    let upstream_tool = &requests[0]["tools"][0];
    assert_eq!(upstream_tool["name"], "web_fetch");
    assert!(upstream_tool.get("type").is_none(), "native type rewritten for vLLM");
    assert_eq!(upstream_tool["input_schema"]["required"], json!(["url"]));

    let results = harness.fed_back_results(1);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_use_id"], "t1");
    assert_eq!(results[0]["is_error"], false);
    let content = result_content(&results[0]);
    assert_eq!(content["type"], "web_fetch_result");
    assert_eq!(content["url"], page_url);
    assert_eq!(content["title"], "Page Title");
    assert_eq!(content["content_type"], "text/html");
    assert_eq!(content["content"], "Hello\nfrom the page");
    assert_eq!(content["truncated"], false);
}

#[tokio::test]
async fn web_fetch_runs_in_the_gateway_and_stays_hidden_over_sse() {
    let harness = harness(&loopback_config()).await;
    let page_url = harness.page_url("plain.txt");
    harness.script(vec![assistant(&[fetch_call("t1", &page_url)], "tool_use"), done()]);

    let body = run_sse(
        &harness,
        request(&format!("Read {page_url}"), &native_fetch(&json!({})), true),
    )
    .await;

    assert_eq!(body.matches("event: message_start").count(), 1, "{body}");
    assert_eq!(body.matches("event: message_stop").count(), 1, "{body}");
    assert!(body.contains("Done."), "{body}");
    assert!(!body.contains("web_fetch"), "gateway tool_use suppressed: {body}");
    assert!(!body.contains("event: error"), "{body}");
    assert_eq!(harness.served_pages(), vec!["plain.txt"]);
    let results = harness.fed_back_results(1);
    let content = result_content(&results[0]);
    assert_eq!(content["content_type"], "text/plain");
    assert_eq!(content["content"], "plain text body");
    assert!(content.get("title").is_none());
}

#[tokio::test]
async fn a_url_from_an_earlier_fetch_result_can_be_fetched_next() {
    let harness = harness(&loopback_config()).await;
    let links = harness.page_url("links.html");
    let plain = harness.page_url("plain.txt");
    harness.script(vec![
        assistant(&[fetch_call("t1", &links)], "tool_use"),
        assistant(&[fetch_call("t2", &plain)], "tool_use"),
        done(),
    ]);

    run_json(
        &harness,
        request(&format!("Follow {links}"), &native_fetch(&json!({})), false),
    )
    .await;

    assert_eq!(harness.served_pages(), vec!["links.html", "plain.txt"]);
    let first = result_content(&harness.fed_back_results(1)[0]);
    assert_eq!(first["content"], format!("Next: {plain}"));
    let second = result_content(&harness.fed_back_results(2)[0]);
    assert_eq!(
        second["type"], "web_fetch_result",
        "the fetched page put the url in context: {second}"
    );
    assert_eq!(second["content"], "plain text body");
}

#[tokio::test]
async fn a_url_the_conversation_never_showed_is_refused_and_still_charged() {
    let harness = harness(&loopback_config()).await;
    let unseen = harness.page_url("doc.html");
    let seen = harness.page_url("plain.txt");
    harness.script(vec![
        assistant(&[fetch_call("t1", &unseen)], "tool_use"),
        assistant(&[fetch_call("t2", &seen)], "tool_use"),
        done(),
    ]);

    let message = run_json(
        &harness,
        request(
            &format!("Look at {seen}"),
            &native_fetch(&json!({"max_uses": 1})),
            false,
        ),
    )
    .await;

    assert_eq!(message["content"][0]["text"], "Done.");
    assert!(harness.served_pages().is_empty(), "nothing was fetched");
    let first = harness.fed_back_results(1);
    let refused = result_content(&first[0]);
    assert_eq!(refused["type"], "web_fetch_tool_result_error");
    assert_eq!(refused["error_code"], "url_not_in_prior_context");
    assert_eq!(first[0]["is_error"], true);
    let exhausted = result_content(&harness.fed_back_results(2)[0]);
    assert_eq!(
        exhausted["error_code"], "max_uses_exceeded",
        "the refused fetch used the only allowed fetch"
    );
}

#[tokio::test]
async fn max_uses_counts_every_admitted_fetch_including_failures() {
    let harness = harness(&loopback_config()).await;
    let missing = harness.page_url("missing");
    let good = harness.page_url("doc.html");
    harness.script(vec![
        assistant(&[fetch_call("t1", &missing), fetch_call("t2", &good)], "tool_use"),
        assistant(
            &[
                fetch_call("t3", &good),
                json!({"type": "tool_use", "id": "t4", "name": "web_fetch", "input": {}}),
            ],
            "tool_use",
        ),
        done(),
    ]);

    run_json(
        &harness,
        request(
            &format!("Check {missing} and {good}"),
            &native_fetch(&json!({"max_uses": 2})),
            false,
        ),
    )
    .await;

    assert_eq!(harness.served_pages(), vec!["missing", "doc.html"]);
    let first = harness.fed_back_results(1);
    assert_eq!(result_content(&first[0])["error_code"], "url_not_accessible");
    assert_eq!(first[0]["is_error"], true);
    assert_eq!(result_content(&first[1])["type"], "web_fetch_result");
    assert_eq!(first[1]["is_error"], false);
    let second = harness.fed_back_results(2);
    assert_eq!(
        result_content(&second[0])["error_code"],
        "max_uses_exceeded",
        "the failed fetch counted"
    );
    let unparseable = result_content(&second[1]);
    assert_eq!(
        unparseable["error_code"], "invalid_tool_input",
        "arguments without a url are answered by the handler, not charged: {unparseable}"
    );
}

#[tokio::test]
async fn private_addresses_are_refused_before_any_connection_unless_allowed() {
    let harness = harness(&WebFetchConfig::default()).await;
    let page_url = harness.page_url("doc.html");
    let by_name = page_url.replace("127.0.0.1", "localhost");
    harness.script(vec![
        assistant(&[fetch_call("t1", &page_url), fetch_call("t2", &by_name)], "tool_use"),
        done(),
    ]);

    run_json(
        &harness,
        request(&format!("{page_url} and {by_name}"), &native_fetch(&json!({})), false),
    )
    .await;

    assert!(harness.served_pages().is_empty(), "no connection was made");
    for result in harness.fed_back_results(1) {
        assert_eq!(result["is_error"], true);
        assert_eq!(result_content(&result)["error_code"], "url_not_allowed", "{result}");
    }
}

#[tokio::test]
async fn domain_filters_apply_to_every_redirect_hop() {
    let harness = harness(&loopback_config()).await;
    let base = harness.url.replace("127.0.0.1", "localhost");
    let same_host = format!("{base}/page/redirect");
    let hops_to_ip = format!("{base}/page/redirect-to-ip");
    harness.script(vec![
        assistant(
            &[fetch_call("t1", &same_host), fetch_call("t2", &hops_to_ip)],
            "tool_use",
        ),
        done(),
    ]);

    run_json(
        &harness,
        request(
            &format!("{same_host} {hops_to_ip}"),
            &native_fetch(&json!({"allowed_domains": ["localhost"]})),
            false,
        ),
    )
    .await;

    let results = harness.fed_back_results(1);
    let followed = result_content(&results[0]);
    assert_eq!(followed["type"], "web_fetch_result", "{followed}");
    assert!(
        followed["url"].as_str().unwrap().ends_with("/page/doc.html"),
        "final url after redirect"
    );
    let refused = result_content(&results[1]);
    assert_eq!(
        refused["error_code"], "url_not_allowed",
        "a hop to a host outside the allowlist: {refused}"
    );
    let mut pages = harness.served_pages();
    pages.sort();
    assert_eq!(
        pages,
        vec!["doc.html", "redirect", "redirect-to-ip"],
        "the refused hop was never requested; the two calls ran concurrently"
    );
}

#[tokio::test]
async fn redirect_hops_are_revalidated_and_bounded() {
    let harness = harness(&loopback_config()).await;
    let to_credentials = harness.page_url("redirect-to-credentials");
    let endless = harness.page_url("loop");
    harness.script(vec![
        assistant(
            &[fetch_call("t1", &to_credentials), fetch_call("t2", &endless)],
            "tool_use",
        ),
        done(),
    ]);

    run_json(
        &harness,
        request(&format!("{to_credentials} {endless}"), &native_fetch(&json!({})), false),
    )
    .await;

    let results = harness.fed_back_results(1);
    let credentials = result_content(&results[0]);
    assert_eq!(credentials["error_code"], "url_not_allowed", "{credentials}");
    assert_eq!(
        credentials["message"], "redirect target refused: url carries credentials",
        "{credentials}"
    );
    let looped = result_content(&results[1]);
    assert_eq!(looped["error_code"], "url_not_accessible", "{looped}");
    assert_eq!(looped["message"], "more than 5 redirects", "{looped}");
    let pages = harness.served_pages();
    assert_eq!(
        pages.iter().filter(|page| *page == "redirect-to-credentials").count(),
        1
    );
    assert_eq!(
        pages.iter().filter(|page| *page == "loop").count(),
        6,
        "the first request plus five redirects"
    );
    assert_eq!(pages.len(), 7, "the credentialed target was never requested: {pages:?}");
}

#[tokio::test]
async fn unsupported_content_and_rate_limits_report_their_codes() {
    let harness = harness(&loopback_config()).await;
    let pdf = harness.page_url("paper.pdf");
    let busy = harness.page_url("busy");
    harness.script(vec![
        assistant(&[fetch_call("t1", &pdf), fetch_call("t2", &busy)], "tool_use"),
        done(),
    ]);

    run_json(
        &harness,
        request(&format!("{pdf} {busy}"), &native_fetch(&json!({})), false),
    )
    .await;

    let results = harness.fed_back_results(1);
    assert_eq!(result_content(&results[0])["error_code"], "unsupported_content_type");
    assert_eq!(result_content(&results[1])["error_code"], "too_many_requests");
}

#[tokio::test]
async fn a_client_function_named_web_fetch_stays_client_owned() {
    let harness = harness(&loopback_config()).await;
    let page_url = harness.page_url("doc.html");
    harness.script(vec![assistant(&[fetch_call("t1", &page_url)], "tool_use")]);
    let request = json!({
        "model": "m", "max_tokens": 64,
        "messages": [{"role": "user", "content": page_url}],
        "tools": [
            {"type": "web_search_20250305", "name": "web_search"},
            {"name": "web_fetch", "description": "my own fetcher",
             "input_schema": {"type": "object", "properties": {"url": {"type": "string"}}}}
        ]
    });

    let message = run_json(&harness, request).await;

    assert_eq!(message["stop_reason"], "tool_use");
    assert_eq!(
        message["content"][0]["name"], "web_fetch",
        "the client's call is returned to the client"
    );
    assert!(harness.served_pages().is_empty());
    let upstream_tool = &harness.upstream_requests()[0]["tools"][1];
    assert_eq!(
        upstream_tool["description"], "my own fetcher",
        "the client's declaration is forwarded as is"
    );
}

#[tokio::test]
async fn a_disabled_executor_fails_registry_construction() {
    let ctx = MessagesRequestContext::from_value(request("x", &native_fetch(&json!({})), false)).unwrap();
    let mut tools = registry_tools(ctx.tools(), &GatewayToolMap::default());
    let error = ToolRegistry::build_with_handlers(&mut tools, &mut GatewayExecutors::default())
        .await
        .expect_err("a disabled executor must not register a placeholder");
    assert!(error.to_string().contains("web_fetch is disabled"), "{error}");
}
