//! A Messages request whose model turn calls the gateway's `web_search`
//! executor twice, followed by a final text turn. Shared by the trace and
//! metrics tests.

// Each test binary uses only the offers it needs.
#![allow(dead_code)]

use std::sync::Arc;

use agentic_core::executor::{ExecutionContext, MessagesRequestContext};
use agentic_core::tool::{ToolRegistry, WebSearchHandler, registry_tools};
use agentic_core::types::messages::{GatewayToolMap, ToolParam};
use serde_json::{Value, json};

use crate::support::{MockResponse, messages_json_response, messages_sse, messages_sse_response, messages_start_event};

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
    let mut events = vec![messages_start_event()];
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
    messages_sse(&events)
}

/// The model's final text turn.
pub fn final_round(stream: bool) -> MockResponse {
    if stream {
        messages_sse_response("end_turn")
    } else {
        messages_json_response("end_turn")
    }
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
