//! A Messages request whose model turn calls the gateway's `web_search`
//! executor twice, followed by a final text turn. Shared by the trace and
//! metrics tests.

// Each test binary uses only the offers and search backends it needs.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::sync::Arc;

use agentic_core::executor::{ExecutionContext, MessagesRequestContext};
use agentic_core::tool::{ToolRegistry, WebSearchHandler, registry_tools};
use agentic_core::types::messages::{GatewayToolMap, ToolParam};
use axum::response::IntoResponse as _;
use serde_json::{Value, json};

use crate::support::MockResponse;

/// The model's search queries. They must never become span or metric data.
pub const QUERIES: [&str; 2] = ["private query one", "private query two"];

/// How the request offers search to the model.
#[derive(Clone, Copy)]
pub enum Offer {
    /// The native tool under `max_uses: 1`: the first call runs and the
    /// second is refused at admission.
    Native,
    /// A client `WebSearch` function the operator aliases to the gateway's
    /// `web_search` executor: both calls run.
    Alias,
}

impl Offer {
    const fn name(self) -> &'static str {
        match self {
            Self::Native => "web_search",
            Self::Alias => "WebSearch",
        }
    }

    fn tools(self) -> Value {
        match self {
            Self::Native => json!([{"type": "web_search_20250305", "name": "web_search", "max_uses": 1}]),
            Self::Alias => json!([{"name": "WebSearch", "description": "Search the web", "input_schema": {
                "type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}}]),
        }
    }
}

pub fn request(stream: bool, offer: Offer) -> MessagesRequestContext {
    MessagesRequestContext::from_value(json!({
        "model": "test-model", "max_tokens": 64, "stream": stream,
        "messages": [{"role": "user", "content": "search twice"}],
        "tools": offer.tools()
    }))
    .unwrap()
}

fn sse(events: &[Value]) -> MockResponse {
    let mut body = String::new();
    for event in events {
        writeln!(body, "event: {}\ndata: {event}\n", event["type"].as_str().unwrap()).unwrap();
    }
    MockResponse::Sse(body)
}

fn message_start() -> Value {
    json!({"type": "message_start", "message": {
        "id": "msg_upstream", "type": "message", "role": "assistant", "model": "test-model",
        "content": [], "stop_reason": null, "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 0}}})
}

/// The model turn that calls the offered search tool once per query.
pub fn tool_round(stream: bool, offer: Offer) -> MockResponse {
    let calls = QUERIES
        .iter()
        .enumerate()
        .map(|(index, query)| (format!("toolu_{index}"), json!({"query": query})));
    if !stream {
        let content: Vec<_> = calls
            .map(|(id, input)| json!({"type": "tool_use", "id": id, "name": offer.name(), "input": input}))
            .collect();
        return MockResponse::Json(
            json!({
                "id": "msg_upstream", "type": "message", "role": "assistant", "model": "test-model",
                "content": content, "stop_reason": "tool_use", "stop_sequence": null,
                "usage": {"input_tokens": 3, "output_tokens": 2}
            })
            .to_string(),
        );
    }
    let mut events = vec![message_start()];
    for (index, (id, input)) in calls.enumerate() {
        events.extend([
            json!({"type": "content_block_start", "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": offer.name(), "input": {}}}),
            json!({"type": "content_block_delta", "index": index,
                "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}),
            json!({"type": "content_block_stop", "index": index}),
        ]);
    }
    events.extend([
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null},
            "usage": {"output_tokens": 2}}),
        json!({"type": "message_stop"}),
    ]);
    sse(&events)
}

/// The model's final text turn.
pub fn final_round(stream: bool) -> MockResponse {
    if !stream {
        return MockResponse::Json(
            json!({
                "id": "msg_upstream", "type": "message", "role": "assistant", "model": "test-model",
                "content": [{"type": "text", "text": "hello"}], "stop_reason": "end_turn", "stop_sequence": null,
                "usage": {"input_tokens": 3, "output_tokens": 1}
            })
            .to_string(),
        );
    }
    sse(&[
        message_start(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hello"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ])
}

/// How the search backend behind `web_search` answers.
#[derive(Clone)]
pub enum Search {
    Answers,
    Fails,
    /// Signals that a search arrived, then never answers it.
    Hangs(Arc<tokio::sync::Notify>),
}

pub async fn search_server(search: Search) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = axum::Router::new().route(
        "/v1/search",
        axum::routing::get(move || {
            let search = search.clone();
            async move {
                match search {
                    Search::Answers => axum::Json(
                        json!({"results": {"web": [{"url": "https://example.com", "title": "result"}], "news": []}}),
                    )
                    .into_response(),
                    Search::Fails => {
                        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "search backend down").into_response()
                    }
                    Search::Hangs(arrived) => {
                        arrived.notify_one();
                        std::future::pending().await
                    }
                }
            }
        }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, task)
}

/// Bind `web_search` to the gateway against `search_url` and resolve the
/// request's tools, as the Messages handler does.
pub async fn with_web_search(
    exec_ctx: ExecutionContext,
    search_url: &str,
    offer: Offer,
) -> (Arc<ExecutionContext>, Arc<ToolRegistry>) {
    let mut exec_ctx = exec_ctx.with_gateway_executor(Arc::new(WebSearchHandler::with_api_key(
        Arc::new(reqwest::Client::new()),
        "private-search-key".into(),
        search_url,
    )));
    exec_ctx.messages_gateway_tools = match offer {
        Offer::Native => GatewayToolMap::default(),
        Offer::Alias => GatewayToolMap::from_pairs([("WebSearch", "web_search")]),
    };
    let tools: Vec<ToolParam> = serde_json::from_value(offer.tools()).unwrap();
    let mut params = registry_tools(Some(&tools), &exec_ctx.messages_gateway_tools);
    let mut executors = exec_ctx.gateway_executors.clone();
    let registry = ToolRegistry::build_with_handlers(&mut params, &mut executors)
        .await
        .unwrap();
    (Arc::new(exec_ctx), Arc::new(registry))
}
