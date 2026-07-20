//! The protocol-neutral seam between Anthropic Messages tool blocks and the
//! internal tool layer (`tool::registry`).
//!
//! The Messages-native loop never touches `RequestPayload`/`ResponsePayload`; it
//! only needs to (1) classify declared tools as gateway-owned vs client-owned so
//! it knows what to execute, (2) turn an assistant `tool_use` block into the
//! `FunctionToolCall` that `ToolRegistry::dispatch` consumes, and (3) turn the
//! resulting `ToolOutput` back into a `tool_result` block to feed the next round.
//!
//! These are pure conversions with no I/O — the loop (`executor::messages_loop`)
//! and the registry supply the behaviour around them.

use serde_json::{Map, Value, json};

use crate::types::event::MessageStatus;
use crate::types::io::output::FunctionToolCall;
use crate::types::tools::{FunctionToolParam, ResponsesTool, WebSearchToolParam};

use super::request::ToolParam;

/// Tool names the gateway executes server-side. A declared tool whose name is in
/// this set is gateway-owned (M6) and drives the loop.
///
/// Only `web_search` today: it is the one default gateway executor and the
/// registry keys it under this exact name (`tool::web_search`). MCP is also
/// gateway-owned but is declared with a distinct wire shape (`server_url`/
/// `server_label`, dynamic tool names) that the Anthropic tool declaration does
/// not express, so it is a separate design step, out of scope here.
///
/// This is a NAME-based predicate because at the Anthropic wire layer a tool is
/// just `{name, input_schema}` — there is no `ToolType` yet. It must stay in
/// sync with the registry's structural classification
/// (`ToolType::is_gateway_owned` / `tool::registry`): adding a new gateway
/// executor there means adding its name here too. When MCP-over-Messages lands,
/// prefer deriving this from the registry rather than extending the match.
#[must_use]
pub fn is_gateway_owned_tool_name(name: &str) -> bool {
    name == "web_search"
}

/// True if the request declares at least one gateway-owned tool — the routing
/// gate that decides loop vs. transparent proxy.
#[must_use]
pub fn has_gateway_tool(tools: Option<&Vec<ToolParam>>) -> bool {
    tools.is_some_and(|tools| tools.iter().any(|t| is_gateway_owned_tool_name(&t.name)))
}

/// Map declared Anthropic tools to the internal `ResponsesTool` list used to
/// build a request-scoped `ToolRegistry`. Gateway-owned tools become the
/// matching gateway variant (registry keys them by name and attaches an
/// executor); everything else becomes a client-owned `Function` (the registry
/// records it so the loop can tell "client-owned" from "unknown", but never
/// executes it).
#[must_use]
pub fn registry_tools(tools: Option<&Vec<ToolParam>>) -> Vec<ResponsesTool> {
    let Some(tools) = tools else {
        return Vec::new();
    };
    tools.iter().filter_map(map_tool).collect()
}

fn map_tool(tool: &ToolParam) -> Option<ResponsesTool> {
    if is_gateway_owned_tool_name(&tool.name) {
        // Defaults are fine: the client's input_schema is the model-facing
        // contract (forwarded to vLLM in the raw request), not the executor's
        // config.
        return Some(ResponsesTool::WebSearch(WebSearchToolParam::default()));
    }
    let name = tool.name.clone().try_into().ok()?;
    Some(ResponsesTool::Function(FunctionToolParam {
        name,
        description: tool.description.clone(),
        parameters: tool.input_schema.clone(),
        strict: None,
        defer_loading: None,
        extra: std::collections::HashMap::new(),
    }))
}

/// Turn an assistant `tool_use` block into the `FunctionToolCall` that
/// `ToolRegistry::dispatch` consumes.
///
/// M1: Anthropic `input` is a JSON object; internal `arguments` is a stringified
/// JSON. M3: the Anthropic `tool_use.id` seeds both `id` and `call_id` so the
/// dispatch result and the fed-back `tool_result` pair by the same id.
#[must_use]
pub fn tool_use_to_call(id: &str, name: &str, input: &Value) -> FunctionToolCall {
    FunctionToolCall {
        id: id.to_owned(),
        call_id: id.to_owned(),
        name: name.to_owned(),
        namespace: None,
        arguments: serde_json::to_string(input).unwrap_or_else(|_| "{}".to_owned()),
        status: MessageStatus::Completed,
    }
}

/// Build the Anthropic `tool_result` content block fed back to the model on the
/// next round, from a dispatched tool's output. `is_error` marks a failed/invalid
/// call so the model knows the tool did not run normally.
#[must_use]
pub fn tool_result_block(tool_use_id: &str, output: &str, is_error: bool) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": output,
        "is_error": is_error,
    })
}

/// Parse a reconstructed `tool_use` input (a JSON string) into the object the
/// tool expects. Returns `Err` for malformed/incomplete JSON or a non-object —
/// the caller must NOT dispatch with fabricated `{}` args the model never sent
/// (F4); it should feed back an error `tool_result` instead.
///
/// # Errors
/// Returns a human-readable reason when the input is not a valid JSON object.
pub fn parse_tool_input(input_json: &str) -> Result<Value, String> {
    let value: Value =
        serde_json::from_str(input_json).map_err(|e| format!("could not parse tool arguments as JSON: {e}"))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err("tool arguments must be a JSON object".to_owned())
    }
}

/// Split an assistant turn's content blocks into (client-visible, gateway
/// `tool_use` present?). Gateway-owned `tool_use` blocks are removed so the
/// client never sees a call it cannot execute (hide-the-call); every other
/// block — text, thinking, signature, client-owned `tool_use` — is preserved in
/// order. Used for the client-facing response on a mixed round (F5).
#[must_use]
pub fn strip_gateway_tool_use(content: &[Value]) -> Vec<Value> {
    content
        .iter()
        .filter(|b| {
            !(b.get("type").and_then(Value::as_str) == Some("tool_use")
                && is_gateway_owned_tool_name(b.get("name").and_then(Value::as_str).unwrap_or_default()))
        })
        .cloned()
        .collect()
}

/// Build the assistant `tool_use` content block that mirrors a call the gateway
/// executed — appended to the assistant turn in the next-round history so the
/// model sees its own call paired with the `tool_result`.
///
/// M1 reverse: internal `arguments` is a stringified JSON; parse it back to an
/// object, falling back to `{}` rather than failing.
#[must_use]
pub fn call_to_tool_use_block(call: &FunctionToolCall) -> Value {
    let input: Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| Value::Object(Map::new()));
    let id = if call.call_id.is_empty() {
        &call.id
    } else {
        &call.call_id
    };
    json!({
        "type": "tool_use",
        "id": id,
        "name": call.name,
        "input": input,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::messages::request::MessagesRequest;

    fn tools_of(json_req: Value) -> Option<Vec<ToolParam>> {
        serde_json::from_value::<MessagesRequest>(json_req).unwrap().tools
    }

    #[test]
    fn web_search_is_gateway_owned_and_maps_to_gateway_variant() {
        assert!(is_gateway_owned_tool_name("web_search"));
        assert!(!is_gateway_owned_tool_name("get_weather"));

        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"name": "web_search", "input_schema": {"type": "object"}}]
        }));
        assert!(has_gateway_tool(tools.as_ref()));
        let mapped = registry_tools(tools.as_ref());
        assert!(matches!(mapped.as_slice(), [ResponsesTool::WebSearch(_)]));
    }

    #[test]
    fn custom_tool_stays_client_owned_function() {
        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"name": "get_weather", "description": "local", "input_schema": {"type": "object"}}]
        }));
        assert!(!has_gateway_tool(tools.as_ref()));
        let mapped = registry_tools(tools.as_ref());
        assert!(matches!(mapped.as_slice(), [ResponsesTool::Function(_)]));
    }

    #[test]
    fn mixed_tools_classify_independently() {
        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [
                {"name": "web_search", "input_schema": {"type": "object"}},
                {"name": "get_weather", "input_schema": {"type": "object"}}
            ]
        }));
        assert!(has_gateway_tool(tools.as_ref()));
        let mapped = registry_tools(tools.as_ref());
        assert!(matches!(mapped[0], ResponsesTool::WebSearch(_)));
        assert!(matches!(mapped[1], ResponsesTool::Function(_)));
    }

    #[test]
    fn no_tools_is_not_gateway_and_maps_empty() {
        assert!(!has_gateway_tool(None));
        assert!(registry_tools(None).is_empty());
    }

    // M1 + M3: tool_use object args -> stringified; id seeds id + call_id.
    #[test]
    fn tool_use_maps_to_function_call() {
        let call = tool_use_to_call("toolu_1", "web_search", &json!({"query": "rust"}));
        assert_eq!(call.id, "toolu_1");
        assert_eq!(call.call_id, "toolu_1");
        assert_eq!(call.name, "web_search");
        let args: Value = serde_json::from_str(&call.arguments).unwrap();
        assert_eq!(args["query"], "rust");
    }

    #[test]
    fn tool_result_block_pairs_by_id() {
        let block = tool_result_block("toolu_1", "the answer", false);
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "toolu_1");
        assert_eq!(block["content"], "the answer");
        assert_eq!(block["is_error"], false);
        // Error results carry is_error: true (F4).
        assert_eq!(tool_result_block("t", "bad args", true)["is_error"], true);
    }

    #[test]
    fn parse_tool_input_rejects_malformed_and_non_object() {
        assert!(parse_tool_input(r#"{"query":"x"}"#).is_ok());
        assert!(parse_tool_input(r#"{"query":"#).is_err(), "incomplete JSON rejected");
        assert!(parse_tool_input(r#""just a string""#).is_err(), "non-object rejected");
    }

    #[test]
    fn strip_gateway_tool_use_removes_only_gateway_calls() {
        let content = vec![
            json!({"type": "text", "text": "hi"}),
            json!({"type": "tool_use", "name": "web_search", "id": "g"}),
            json!({"type": "tool_use", "name": "get_weather", "id": "c"}),
        ];
        let out = strip_gateway_tool_use(&content);
        let names: Vec<&str> = out
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .filter_map(|b| b["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec!["get_weather"],
            "gateway web_search stripped, client + text kept"
        );
        assert_eq!(out.len(), 2);
    }

    // M1 reverse: stringified args -> object; call_id preferred as the block id.
    #[test]
    fn call_to_tool_use_block_round_trips() {
        let call = tool_use_to_call("toolu_9", "web_search", &json!({"query": "x", "count": 2}));
        let block = call_to_tool_use_block(&call);
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "toolu_9");
        assert_eq!(block["name"], "web_search");
        assert_eq!(block["input"]["query"], "x");
        assert_eq!(block["input"]["count"], 2);
    }

    #[test]
    fn call_to_tool_use_block_falls_back_on_bad_args() {
        let mut call = tool_use_to_call("t", "x", &json!({}));
        call.arguments = "not json".to_owned();
        let block = call_to_tool_use_block(&call);
        assert_eq!(block["input"], json!({}));
    }
}
