//! Public records of gateway-executed calls that a client replays as input.
//!
//! A `store: false` client sends back the `mcp_call` and `web_search_call` items a response returned. The model
//! never saw those public records: it called a function and read its output. The model-facing copy therefore
//! carries each record as that function call and output pair, as stored history does after a live call.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::{FunctionToolResultMessage, InputFunctionToolCall, InputItem, ToolCallOutput};
use crate::tool::mcp::handler::model_tool_name;
use crate::types::event::MessageStatus;
use crate::types::io::AgentAttribution;
use crate::types::io::output::{McpCall, WebSearchAction, WebSearchCall};
use crate::types::tools::ResponsesTool;
use crate::utils::common::{serialize_to_string, serialize_to_value};

/// The function the gateway lowers web search to; see `tool::web_search`.
const WEB_SEARCH_FUNCTION_NAME: &str = "web_search";

/// Whether this item is a replayed public record of a gateway-executed call.
pub(super) const fn is_replayed_gateway_call(item: &InputItem) -> bool {
    matches!(item, InputItem::McpCall(_) | InputItem::WebSearchCall(_))
}

/// The function names MCP discovery assigned this request, by server label and tool name. Discovery gives two tools
/// whose names sanitize alike distinct names, so a replayed call looks its name up here before deriving one.
#[derive(Default)]
pub(crate) struct McpToolNames<'a> {
    names: HashMap<(&'a str, &'a str), &'a str>,
}

impl<'a> McpToolNames<'a> {
    pub(crate) fn from_tools(tools: &'a [ResponsesTool]) -> Self {
        let names = tools
            .iter()
            .filter_map(|tool| match tool {
                ResponsesTool::Mcp(server) => Some(&server.discovered_tools),
                _ => None,
            })
            .flatten()
            .map(|tool| {
                (
                    (tool.server_label.as_str(), tool.tool_name.as_str()),
                    tool.internal_name.as_str(),
                )
            })
            .collect();
        Self { names }
    }

    fn model_name(&self, server_label: &str, tool_name: &str) -> String {
        self.names
            .get(&(server_label, tool_name))
            .map_or_else(|| model_tool_name(server_label, tool_name), |name| (*name).to_owned())
    }
}

/// Gives each replayed gateway call in `items` the function call and output the model saw live, named by
/// `mcp_names`, for a model request that carries no tools of its own (a compaction summary).
pub(crate) fn name_replayed_gateway_calls(items: Vec<InputItem>, mcp_names: &McpToolNames<'_>) -> Vec<InputItem> {
    items
        .into_iter()
        .flat_map(|item| model_items(item, mcp_names))
        .collect()
}

/// The model-facing items for one input item: a replayed gateway call becomes its function call and output.
pub(super) fn model_items(item: InputItem, mcp_names: &McpToolNames<'_>) -> Vec<InputItem> {
    match item {
        InputItem::McpCall(call) => mcp_call_items(&call, mcp_names),
        InputItem::WebSearchCall(call) => web_search_items(&call),
        other => vec![other],
    }
}

fn mcp_call_items(call: &McpCall, mcp_names: &McpToolNames<'_>) -> Vec<InputItem> {
    let output = match (&call.output, &call.error) {
        (Some(output), _) => output.clone(),
        (None, Some(error)) => {
            serialize_to_string(&json!({"error": serialize_to_value(error).unwrap_or(Value::Null)})).unwrap_or_default()
        }
        (None, None) => String::new(),
    };
    call_pair(
        call,
        &call.id,
        mcp_names.model_name(&call.server_label, &call.name),
        call.arguments.clone(),
        output,
    )
}

fn web_search_items(call: &WebSearchCall) -> Vec<InputItem> {
    let (arguments, output) = match &call.action {
        WebSearchAction::Search(search) => {
            let queries = if search.queries.is_empty() {
                vec![search.query.clone()]
            } else {
                search.queries.clone()
            };
            let output = if search.sources.is_empty() {
                json!({"note": "The results of this earlier web search are not available."})
            } else {
                json!({"sources": serialize_to_value(&search.sources).unwrap_or(Value::Null)})
            };
            (json!({"queries": queries}), output)
        }
        action => (
            serialize_to_value(action).unwrap_or(Value::Null),
            json!({"note": "The result of this earlier web action is not available."}),
        ),
    };
    call_pair(
        call,
        &call.id,
        WEB_SEARCH_FUNCTION_NAME.to_owned(),
        serialize_to_string(&arguments).unwrap_or_default(),
        serialize_to_string(&output).unwrap_or_default(),
    )
}

/// The record's id serves as the call id, so a replay pairs its call and output deterministically.
fn call_pair(
    record: &impl Attributed,
    item_id: &str,
    name: String,
    arguments: String,
    output: String,
) -> Vec<InputItem> {
    vec![
        InputItem::FunctionCall(InputFunctionToolCall {
            agent: record.agent(),
            id: Some(item_id.to_owned()),
            call_id: item_id.to_owned(),
            name,
            namespace: None,
            arguments,
            status: Some(MessageStatus::Completed),
            async_execution: false,
        }),
        InputItem::FunctionCallOutput(FunctionToolResultMessage {
            call_id: item_id.to_owned(),
            output: ToolCallOutput::Text(output),
        }),
    ]
}

trait Attributed {
    fn agent(&self) -> Option<AgentAttribution>;
}

impl Attributed for McpCall {
    fn agent(&self) -> Option<AgentAttribution> {
        self.agent.clone()
    }
}

impl Attributed for WebSearchCall {
    fn agent(&self) -> Option<AgentAttribution> {
        self.agent.clone()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::types::io::{InOutItem, InputItem, ResponsesInput};
    use crate::types::request_response::RequestPayload;
    use crate::types::tools::{McpDiscoveredToolParam, ResponsesTool};

    fn model_input(items: serde_json::Value) -> serde_json::Value {
        let input: ResponsesInput = serde_json::from_value(items).expect("input");
        serde_json::to_value(input.model_input()).expect("model input")
    }

    #[test]
    fn replayed_mcp_call_reaches_the_model_as_its_function_call_and_output() {
        let model = model_input(json!([
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "mcp_call", "id": "mcp_1", "server_label": "docs", "name": "search", "arguments": "{\"q\":\"x\"}",
                "status": "completed", "approval_request_id": null, "output": "found", "error": null}
        ]));
        assert_eq!(
            model,
            json!([
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "function_call", "id": "mcp_1", "call_id": "mcp_1", "name": "mcp__docs__search",
                    "arguments": "{\"q\":\"x\"}", "status": "completed"},
                {"type": "function_call_output", "call_id": "mcp_1", "output": "found"}
            ])
        );
    }

    #[test]
    fn replayed_failed_mcp_call_carries_its_error() {
        let model = model_input(json!([
            {"type": "mcp_call", "id": "mcp_2", "server_label": "docs", "name": "search", "arguments": "{}",
                "status": "failed", "approval_request_id": null, "output": null, "error": "boom"}
        ]));
        assert_eq!(model[1]["output"], "{\"error\":\"boom\"}");
    }

    #[test]
    fn replayed_web_search_call_reaches_the_model_as_a_web_search_call() {
        let model = model_input(json!([
            {"type": "web_search_call", "id": "ws_1", "status": "completed",
                "action": {"type": "search", "query": "rust", "queries": ["rust"],
                    "sources": [{"type": "url", "url": "https://www.rust-lang.org"}]}}
        ]));
        assert_eq!(model[0]["type"], "function_call");
        assert_eq!(model[0]["name"], "web_search");
        assert_eq!(model[0]["arguments"], "{\"queries\":[\"rust\"]}");
        assert_eq!(model[1]["call_id"], "ws_1");
        assert_eq!(
            model[1]["output"],
            "{\"sources\":[{\"url\":\"https://www.rust-lang.org\"}]}"
        );
    }

    /// Two tools whose names sanitize alike get distinct names at discovery; a replay keeps that assignment.
    #[test]
    fn replayed_mcp_call_keeps_the_name_discovery_assigned() {
        let discovered = |tool_name: &str, internal_name: &str| McpDiscoveredToolParam {
            server_label: "docs".to_owned(),
            tool_name: tool_name.to_owned(),
            internal_name: internal_name.to_owned(),
            tool: serde_json::from_value(json!({"name": tool_name, "inputSchema": {"type": "object"}})).expect("tool"),
        };
        let mut payload: RequestPayload = serde_json::from_value(json!({
            "model": "test",
            "input": [{"type": "mcp_call", "id": "mcp_2", "server_label": "docs", "name": "read_file",
                "arguments": "{}", "status": "completed", "approval_request_id": null, "output": "second",
                "error": null}],
            "tools": [{"type": "mcp", "server_label": "docs", "server_url": "https://docs.example/mcp",
                "require_approval": "never"}]
        }))
        .expect("request");
        let Some(tools) = payload.tools.as_mut() else {
            panic!("declared tools")
        };
        let ResponsesTool::Mcp(server) = &mut tools[0] else {
            panic!("MCP declaration")
        };
        server.discovered_tools = vec![
            discovered("read.file", "mcp__docs__read_file"),
            discovered("read_file", "mcp__docs__read_file_2"),
        ];

        let upstream = serde_json::to_value(payload.to_upstream_request(false).expect("upstream request").input)
            .expect("upstream input");

        assert_eq!(upstream[0]["name"], "mcp__docs__read_file_2", "{upstream}");
    }

    #[test]
    fn stored_history_keeps_the_public_records() {
        let input: ResponsesInput = serde_json::from_value(json!([
            {"type": "mcp_call", "id": "mcp_3", "server_label": "docs", "name": "search", "arguments": "{}",
                "status": "completed", "approval_request_id": null, "output": "ok", "error": null}
        ]))
        .expect("input");
        let ResponsesInput::Items(items) = &input else {
            panic!("items")
        };
        assert!(matches!(&items[0], InOutItem::Input(InputItem::McpCall(_))));
    }
}
