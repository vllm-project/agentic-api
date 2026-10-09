//! The protocol-neutral seam between Anthropic Messages tool blocks and the
//! internal tool layer (`tool::registry`).
//!
//! The Messages-native loop never touches `RequestPayload`/`ResponsePayload`; it
//! only needs to (1) classify declared tools as gateway-owned vs client-owned so
//! it knows what to execute, (2) turn an assistant `tool_use` block into the
//! `FunctionToolCall` that `ToolRegistry::dispatch` consumes, and (3) turn the
//! resulting `ToolOutput` back into a `tool_result` block to feed the next round.
//! Mapping declared tools to the registry's declarations is the tool layer's
//! (`tool::registry_tools`).
//!
//! These are pure conversions with no I/O — the loop (`executor::messages_loop`)
//! and the registry supply the behaviour around them.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::types::event::MessageStatus;
use crate::types::io::output::FunctionToolCall;

use super::request::{GatewayToolResult, ToolParam};

/// The built-in gateway executor every `/v1/messages` request may name. The
/// registry keys it under this exact name (`tool::web_search`).
pub const WEB_SEARCH_EXECUTOR: &str = "web_search";

/// Claude's basic native web-search server tool version, supported by the
/// Messages gateway loop as a gateway-owned tool.
pub const NATIVE_WEB_SEARCH_TYPE: &str = "web_search_20250305";

/// The second built-in gateway executor on `/v1/messages`, reachable only
/// through a native `web_fetch_*` declaration (`tool::web_fetch`).
pub const WEB_FETCH_EXECUTOR: &str = "web_fetch";

/// Claude's basic native web-fetch server tool version, the one the Messages
/// gateway loop executes.
pub const NATIVE_WEB_FETCH_TYPE: &str = "web_fetch_20250910";

/// Whether a declared tool `type` names a native web-fetch server tool, of
/// any version. Version support is decided when the request is normalized.
#[must_use]
pub fn is_native_web_fetch_type(tool_type: Option<&str>) -> bool {
    tool_type.is_some_and(|tool_type| tool_type.starts_with("web_fetch_"))
}

/// Operator-configured map of client-declared tool names to gateway executors.
///
/// Empty by default: a client `function` stays client-owned unless the operator
/// configures it here — this is the "unless configured as gateway-owned" clause
/// of the ownership doctrine (see `docs/design/codex-integration.md`), applied
/// structurally rather than by a hardcoded name heuristic. It lets a client
/// like Claude Code — which declares its web search as a client function named
/// `WebSearch` — have that call executed server-side by the gateway's
/// `web_search` executor, mirroring how Codex's typed `web_search` tool already
/// routes through the Responses loop.
///
/// The canonical executor `web_search` is always recognised; the map only adds
/// operator-approved *aliases* on top. The `web_fetch` executor is recognised
/// per request: the Messages loop marks it owned
/// ([`with_web_fetch_owned`](Self::with_web_fetch_owned)) when the
/// request-scoped registry bound the name to the gateway executor, which only
/// a native `web_fetch_*` declaration does.
#[derive(Clone, Debug, Default)]
pub struct GatewayToolMap {
    /// client tool name (as the model calls it) → canonical executor key.
    aliases: HashMap<String, String>,
    /// Whether `web_fetch` is gateway-owned for this request. Never an
    /// operator setting: a plain function named `web_fetch` is the client's.
    web_fetch: bool,
}

impl GatewayToolMap {
    /// Build from `name=executor` pairs (e.g. `WebSearch=web_search`). Pairs
    /// naming an unknown executor are skipped. Whitespace is trimmed.
    #[must_use]
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let aliases = pairs
            .into_iter()
            .map(|(name, exec)| (name.trim(), exec.trim()))
            .filter(|(name, exec)| !name.is_empty() && *exec == WEB_SEARCH_EXECUTOR)
            .map(|(name, exec)| (name.to_owned(), exec.to_owned()))
            .collect();
        Self {
            aliases,
            web_fetch: false,
        }
    }

    /// Parse the `MESSAGES_GATEWAY_TOOL_ALIASES` env format:
    /// `"WebSearch=web_search,OtherName=web_search"`.
    #[must_use]
    pub fn from_env_str(raw: &str) -> Self {
        let pairs: Vec<(&str, &str)> = raw.split(',').filter_map(|kv| kv.split_once('=')).collect();
        Self::from_pairs(pairs)
    }

    /// The map for one request, with `web_fetch` gateway-owned or not. The
    /// Messages loop derives `owned` from the request-scoped registry, so the
    /// decision lives with the resolved registry entries, not here.
    #[must_use]
    pub fn with_web_fetch_owned(mut self, owned: bool) -> Self {
        self.web_fetch = owned;
        self
    }

    /// The canonical executor key for a declared tool name, if it is
    /// gateway-owned: the built-in `web_search`, or a configured alias.
    #[must_use]
    pub fn canonical_executor(&self, name: &str) -> Option<&str> {
        if name == WEB_SEARCH_EXECUTOR {
            Some(WEB_SEARCH_EXECUTOR)
        } else if self.web_fetch && name == WEB_FETCH_EXECUTOR {
            Some(WEB_FETCH_EXECUTOR)
        } else {
            self.aliases.get(name).map(String::as_str)
        }
    }

    #[must_use]
    pub fn is_gateway_owned(&self, name: &str) -> bool {
        self.canonical_executor(name).is_some()
    }
}

/// True if the request declares at least one gateway-owned tool — the routing
/// gate that decides loop vs. transparent proxy. Gateway ownership is resolved
/// against the operator-configured [`GatewayToolMap`], plus the native server
/// tools only the gateway can execute.
#[must_use]
pub fn has_gateway_tool(tools: Option<&Vec<ToolParam>>, map: &GatewayToolMap) -> bool {
    tools.is_some_and(|tools| {
        tools
            .iter()
            .any(|t| map.is_gateway_owned(&t.name) || is_native_web_fetch_type(t.type_.as_deref()))
    })
}

/// Turn an assistant `tool_use` block into the `FunctionToolCall` that
/// `ToolRegistry::dispatch` consumes.
///
/// M1: Anthropic `input` is a JSON object; internal `arguments` is a stringified
/// JSON. M3: the Anthropic `tool_use.id` seeds both `id` and `call_id` so the
/// dispatch result and the fed-back `tool_result` pair by the same id. The name
/// is canonicalised to the executor key (so a configured alias like Claude
/// Code's `WebSearch` dispatches to the `web_search` executor), and its
/// arguments are adapted to the executor's schema ([`adapt_web_search_input`]).
#[must_use]
pub fn tool_use_to_call(id: &str, name: &str, input: &Value, map: &GatewayToolMap) -> FunctionToolCall {
    let canonical = map.canonical_executor(name).unwrap_or(name);
    let adapted = if canonical == WEB_SEARCH_EXECUTOR && name != WEB_SEARCH_EXECUTOR {
        adapt_web_search_input(input)
    } else {
        input.clone()
    };
    FunctionToolCall {
        async_execution: false,
        agent: None,
        id: id.to_owned(),
        call_id: id.to_owned(),
        name: canonical.to_owned(),
        namespace: None,
        arguments: serde_json::to_string(&adapted).unwrap_or_else(|_| "{}".to_owned()),
        status: MessageStatus::Completed,
    }
}

/// Adapt a client web-search tool's arguments to the gateway `web_search`
/// executor's schema. Claude Code's `WebSearch` uses `allowed_domains` /
/// `blocked_domains`; the executor uses `include_domains` / `exclude_domains`.
/// `query` and any executor-native fields pass through; fields the executor does
/// not model are dropped (it ignores unknown keys).
///
/// Limitation: the executor rejects `include_domains` combined with
/// `exclude_domains` (see `tool::web_search`). Claude Code's schema allows both
/// `allowed_domains` and `blocked_domains` at once; a call that sets both yields
/// an error `tool_result` (fed back to the model — graceful, not a failure).
/// Not worked around here so the constraint stays in one place (the executor).
#[must_use]
pub fn adapt_web_search_input(input: &Value) -> Value {
    let Some(obj) = input.as_object() else {
        return input.clone();
    };
    let mut out = obj.clone();
    if let Some(v) = out.remove("allowed_domains") {
        out.entry("include_domains").or_insert(v);
    }
    if let Some(v) = out.remove("blocked_domains") {
        out.entry("exclude_domains").or_insert(v);
    }
    Value::Object(out)
}

/// Build the Anthropic `tool_result` content block fed back to the model on the
/// next round, from a dispatched tool's output. `is_error` marks a failed/invalid
/// call so the model knows the tool did not run normally.
#[must_use]
pub fn tool_result_block(tool_use_id: &str, output: String, is_error: bool) -> GatewayToolResult {
    GatewayToolResult::new(tool_use_id, output, is_error)
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
pub fn strip_gateway_tool_use(content: &[Value], map: &GatewayToolMap) -> Vec<Value> {
    content
        .iter()
        .filter(|b| {
            !(b.get("type").and_then(Value::as_str) == Some("tool_use")
                && map.is_gateway_owned(b.get("name").and_then(Value::as_str).unwrap_or_default()))
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

    /// Default map: only the built-in `web_search` executor, no aliases.
    fn default_map() -> GatewayToolMap {
        GatewayToolMap::default()
    }

    /// Map with Claude Code's `WebSearch` aliased to the `web_search` executor.
    fn alias_map() -> GatewayToolMap {
        GatewayToolMap::from_pairs([("WebSearch", "web_search")])
    }

    #[test]
    fn web_search_is_gateway_owned() {
        let map = default_map();
        assert!(map.is_gateway_owned("web_search"));
        assert!(!map.is_gateway_owned("get_weather"));

        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"name": "web_search", "input_schema": {"type": "object"}}]
        }));
        assert!(has_gateway_tool(tools.as_ref(), &map));
    }

    #[test]
    fn custom_tool_stays_client_owned() {
        let map = default_map();
        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"name": "get_weather", "description": "local", "input_schema": {"type": "object"}}]
        }));
        assert!(!has_gateway_tool(tools.as_ref(), &map));
    }

    #[test]
    fn mixed_tools_take_the_loop() {
        let map = default_map();
        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [
                {"name": "web_search", "input_schema": {"type": "object"}},
                {"name": "get_weather", "input_schema": {"type": "object"}}
            ]
        }));
        assert!(has_gateway_tool(tools.as_ref(), &map));
    }

    #[test]
    fn no_tools_is_not_gateway() {
        assert!(!has_gateway_tool(None, &default_map()));
    }

    // Claude Code declares its web search as a client function `WebSearch`. With
    // no alias configured it stays client-owned; with the alias it becomes
    // gateway-owned and maps to the WebSearch gateway variant.
    #[test]
    fn claude_code_websearch_is_client_owned_by_default_gateway_when_aliased() {
        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"name": "WebSearch", "description": "Search the web.",
                       "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}}]
        }));
        // Default: not gateway-owned (doctrine default — unless configured).
        assert!(!has_gateway_tool(tools.as_ref(), &default_map()));
        // Aliased: gateway-owned, dispatching to the web_search executor.
        let map = alias_map();
        assert!(map.is_gateway_owned("WebSearch"));
        assert_eq!(map.canonical_executor("WebSearch"), Some("web_search"));
        assert!(has_gateway_tool(tools.as_ref(), &map));
    }

    #[test]
    fn gateway_tool_map_from_env_str_and_rejects_unknown_executor() {
        let map = GatewayToolMap::from_env_str("WebSearch=web_search, Foo=web_search");
        assert!(map.is_gateway_owned("WebSearch"));
        assert!(map.is_gateway_owned("Foo"));
        // An alias pointing at an unknown executor is dropped.
        let bad = GatewayToolMap::from_env_str("X=not_a_real_executor");
        assert!(!bad.is_gateway_owned("X"));
    }

    // A configured alias canonicalises the dispatch name AND adapts args
    // (allowed_domains -> include_domains) so Claude Code's WebSearch schema
    // reaches the web_search executor correctly.
    #[test]
    fn aliased_tool_use_canonicalises_name_and_adapts_args() {
        let call = tool_use_to_call(
            "toolu_cc",
            "WebSearch",
            &json!({"query": "rust", "allowed_domains": ["rust-lang.org"], "blocked_domains": ["spam.com"]}),
            &alias_map(),
        );
        assert_eq!(call.name, "web_search", "alias canonicalised to executor key");
        let args: Value = serde_json::from_str(&call.arguments).unwrap();
        assert_eq!(args["query"], "rust");
        assert_eq!(
            args["include_domains"],
            json!(["rust-lang.org"]),
            "allowed_domains adapted"
        );
        assert_eq!(args["exclude_domains"], json!(["spam.com"]), "blocked_domains adapted");
        assert!(args.get("allowed_domains").is_none(), "CC field renamed away");
    }

    #[test]
    fn adapt_web_search_input_is_noop_for_native_fields() {
        let out = adapt_web_search_input(&json!({"query": "x", "include_domains": ["a.com"]}));
        assert_eq!(out["query"], "x");
        assert_eq!(out["include_domains"], json!(["a.com"]));
    }

    // M1 + M3: tool_use object args -> stringified; id seeds id + call_id.
    #[test]
    fn tool_use_maps_to_function_call() {
        let call = tool_use_to_call("toolu_1", "web_search", &json!({"query": "rust"}), &default_map());
        assert_eq!(call.id, "toolu_1");
        assert_eq!(call.call_id, "toolu_1");
        assert_eq!(call.name, "web_search");
        let args: Value = serde_json::from_str(&call.arguments).unwrap();
        assert_eq!(args["query"], "rust");
    }

    #[test]
    fn tool_result_block_pairs_by_id() {
        let block = serde_json::to_value(tool_result_block("toolu_1", "the answer".to_owned(), false)).unwrap();
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "toolu_1");
        assert_eq!(block["content"], "the answer");
        assert_eq!(block["is_error"], false);
        // Error results carry is_error: true (F4).
        let error = serde_json::to_value(tool_result_block("t", "bad args".to_owned(), true)).unwrap();
        assert_eq!(error["is_error"], true);
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
        let out = strip_gateway_tool_use(&content, &default_map());
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
        let call = tool_use_to_call(
            "toolu_9",
            "web_search",
            &json!({"query": "x", "count": 2}),
            &default_map(),
        );
        let block = call_to_tool_use_block(&call);
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "toolu_9");
        assert_eq!(block["name"], "web_search");
        assert_eq!(block["input"]["query"], "x");
        assert_eq!(block["input"]["count"], 2);
    }

    #[test]
    fn call_to_tool_use_block_falls_back_on_bad_args() {
        let mut call = tool_use_to_call("t", "x", &json!({}), &default_map());
        call.arguments = "not json".to_owned();
        let block = call_to_tool_use_block(&call);
        assert_eq!(block["input"], json!({}));
    }

    #[test]
    fn a_native_web_fetch_declaration_routes_to_the_loop() {
        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"type": "web_fetch_20250910", "name": "web_fetch", "max_uses": 2,
                       "allowed_domains": ["example.com"], "max_content_tokens": 5000}]
        }));
        let map = default_map();
        assert!(
            !map.is_gateway_owned("web_fetch"),
            "the operator map alone never owns web_fetch"
        );
        assert!(
            has_gateway_tool(tools.as_ref(), &map),
            "a native declaration takes the loop"
        );

        // Ownership for the loop comes from the registry, through the adapter.
        let owned = map.clone().with_web_fetch_owned(true);
        assert_eq!(owned.canonical_executor("web_fetch"), Some("web_fetch"));
        assert!(owned.is_gateway_owned("web_search"), "web_search stays recognised");
        assert!(!map.clone().with_web_fetch_owned(false).is_gateway_owned("web_fetch"));

        // Any web_fetch_* version routes to the loop; the version itself is
        // judged when the request is normalized.
        let later = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"type": "web_fetch_20260318", "name": "web_fetch"}]
        }));
        assert!(has_gateway_tool(later.as_ref(), &map));
        // Operator aliases cannot point at web_fetch.
        assert!(!GatewayToolMap::from_env_str("WebFetch=web_fetch").is_gateway_owned("WebFetch"));
    }

    #[test]
    fn a_plain_function_named_web_fetch_stays_client_owned() {
        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"name": "web_fetch", "input_schema": {"type": "object"}}]
        }));
        assert!(!has_gateway_tool(tools.as_ref(), &default_map()));
        let map = default_map();
        assert!(!map.is_gateway_owned("web_fetch"));
        let content = vec![json!({"type": "tool_use", "name": "web_fetch", "id": "c"})];
        assert_eq!(
            strip_gateway_tool_use(&content, &map).len(),
            1,
            "the client's call stays visible"
        );
    }

    #[test]
    fn web_fetch_calls_dispatch_under_their_own_name_without_adaptation() {
        let map = default_map().with_web_fetch_owned(true);
        let call = tool_use_to_call("toolu_f", "web_fetch", &json!({"url": "https://example.com/a"}), &map);
        assert_eq!(call.name, "web_fetch");
        assert_eq!(call.call_id, "toolu_f");
        let args: Value = serde_json::from_str(&call.arguments).unwrap();
        assert_eq!(args["url"], "https://example.com/a");
        let content = vec![
            json!({"type": "text", "text": "t"}),
            json!({"type": "tool_use", "name": "web_fetch", "id": "g"}),
        ];
        assert_eq!(
            strip_gateway_tool_use(&content, &map).len(),
            1,
            "the gateway fetch is hidden"
        );
    }
}
