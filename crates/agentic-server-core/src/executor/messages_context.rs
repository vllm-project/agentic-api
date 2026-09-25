//! Per-request context for the Anthropic Messages gateway tool loops.
//!
//! The Messages loops are a pass-through, not a transform: the client's request
//! is forwarded to vLLM `/v1/messages` essentially untouched, and only `tools`
//! and `stream`/`messages`/`tool_choice` are read or rewritten. That rules out a typed
//! round-trip through [`MessagesRequest`] as the upstream body — `ContentBlock`
//! carries a `#[serde(other)] Unknown` catch-all, and several block types model
//! only the fields the gateway reads, so re-serializing would silently drop
//! `cache_control`, `is_error`, and every unmodeled block (`image`,
//! `redacted_thinking`, future provider extensions).
//!
//! This context retains the request data needed by the loop:
//!
//! * `raw` — the JSON body actually sent upstream. It
//!   is the source of truth for `messages` and `system`, preserving unmodeled
//!   history blocks and extension fields.
//! * `typed` — only the client's `tools`, `stream`, and `model`, retained for
//!   safe field access in routing and the loops. The parsed message history,
//!   system prompt, and other fields are dropped before the loop begins.
//! * `tool_choice` — the current typed selector. Fulfillment and transitions
//!   use exhaustive enum matches; each transition updates the raw selector.
//!
//! The two are **not** kept byte-identical, and must not be confused: `typed` is
//! what the client sent, `raw` is what the gateway sends upstream. They diverge
//! wherever the gateway rewrites the body for upstream — today
//! `normalize_native_web_search` rewriting a native `web_search_20250305`
//! declaration into the ordinary function-tool shape vLLM accepts. Accordingly,
//! only the fields the loops never mutate are exposed off `typed`
//! ([`tools`](MessagesRequestContext::tools),
//! [`stream`](MessagesRequestContext::stream),
//! [`model`](MessagesRequestContext::model)); `messages` and `system` are
//! deliberately unreachable through it, so a stale typed view can never be read
//! back after a round is appended.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::messages_request::{WebSearchBudget, normalize_native_web_search};
use crate::types::messages::request::MessagesToolDeclarations;
use crate::types::messages::{
    GatewayToolMap, GatewayToolResult, MessagesRequest, MessagesToolChoice, ToolParam, has_gateway_tool,
};
use crate::utils::common::serialize_to_string;

/// A Messages request parsed together with the exact immutable body bytes it
/// came from.
///
/// Private fields make independently pairing typed data and raw bytes
/// impossible through the public API. The handler uses the typed view for
/// routing, then consumes this value to build a [`MessagesRequestContext`] only
/// when the request needs the gateway tool loop.
#[derive(Debug)]
pub struct ParsedMessagesRequest<'a> {
    typed: MessagesRequest,
    body: &'a [u8],
}

impl<'a> ParsedMessagesRequest<'a> {
    /// Parse a Messages request while retaining the exact bytes it came from.
    ///
    /// # Errors
    /// Returns [`ExecutorError::JsonError`] if `body` is not a well-formed
    /// Messages request.
    pub fn parse(body: &'a [u8]) -> ExecutorResult<Self> {
        let typed = serde_json::from_slice(body).map_err(ExecutorError::JsonError)?;
        Ok(Self { typed, body })
    }

    /// Parse requests handled by the gateway; preserve the transparent proxy
    /// contract for requests without gateway-executed tools.
    ///
    /// # Errors
    /// Returns the parse error if a gateway-tool request fails validation.
    pub fn parse_for_gateway(body: &'a [u8], gateway_map: &GatewayToolMap) -> ExecutorResult<Option<Self>> {
        match Self::parse(body) {
            Ok(parsed) => Ok(has_gateway_tool(parsed.tools(), gateway_map).then_some(parsed)),
            Err(error) => {
                let declares_gateway_tool = serde_json::from_slice::<MessagesToolDeclarations>(body)
                    .is_ok_and(|request| has_gateway_tool(request.tools.as_ref(), gateway_map));
                if declares_gateway_tool { Err(error) } else { Ok(None) }
            }
        }
    }

    /// The tools declared by the client, before upstream normalization.
    #[must_use]
    pub fn tools(&self) -> Option<&Vec<ToolParam>> {
        self.typed.tools.as_ref()
    }

    /// Whether the client requested a streaming response.
    #[must_use]
    pub fn stream(&self) -> bool {
        self.typed.stream
    }
}

#[derive(Debug)]
struct MessagesTypedState {
    model: String,
    tools: Option<Vec<ToolParam>>,
    stream: bool,
}

impl From<MessagesRequest> for MessagesTypedState {
    fn from(request: MessagesRequest) -> Self {
        let MessagesRequest {
            model, tools, stream, ..
        } = request;
        Self { model, tools, stream }
    }
}

/// One `/v1/messages` request, in both the typed and raw views the gateway tool
/// loops need. See the module docs for why both exist.
#[derive(Debug)]
pub struct MessagesRequestContext {
    /// The only typed request fields needed after routing.
    typed: MessagesTypedState,
    /// Current upstream selection policy; updated together with `raw`.
    tool_choice: Option<MessagesToolChoice>,
    /// The upstream body. Mutated by the loops; the source of truth for
    /// `messages` and `system`.
    raw: Value,
    /// Request-wide native web-search budget, derived while normalizing `raw`.
    web_search_budget: WebSearchBudget,
}

impl MessagesRequestContext {
    /// Build the context from a validated typed/raw request pair.
    ///
    /// Consuming [`ParsedMessagesRequest`] guarantees both views derive from the
    /// same immutable input. Only the fields required after routing are retained
    /// from the typed view; the owned message history and system prompt are
    /// dropped before this function returns.
    ///
    /// Native web-search declarations are validated and normalized here, before
    /// a streaming handler commits its HTTP status — an invalid declaration must
    /// surface as an error response, not as a mid-stream event.
    ///
    /// # Errors
    /// Returns [`ExecutorError::JsonError`] if `body` is not valid JSON, or
    /// [`ExecutorError::InvalidRequest`] if it carries an unsupported or invalid
    /// native web-search declaration.
    pub fn new(parsed: ParsedMessagesRequest<'_>) -> ExecutorResult<Self> {
        let raw = serde_json::from_slice(parsed.body).map_err(ExecutorError::JsonError)?;
        Self::from_parts(parsed.typed, raw)
    }

    /// Build the context from a raw JSON body alone, deriving the typed view
    /// from it.
    ///
    /// Prefer [`new`](Self::new) when the caller has already parsed the request
    /// for routing; this exists for callers that only hold a [`Value`].
    ///
    /// # Errors
    /// Returns [`ExecutorError::JsonError`] if `raw` is not a well-formed
    /// Messages request, or [`ExecutorError::InvalidRequest`] if it carries an
    /// unsupported or invalid native web-search declaration.
    pub fn from_value(raw: Value) -> ExecutorResult<Self> {
        // Deserializing from the parsed tree avoids re-lexing the body text.
        let typed = MessagesRequest::deserialize(&raw).map_err(ExecutorError::JsonError)?;
        Self::from_parts(typed, raw)
    }

    fn from_parts(mut typed: MessagesRequest, mut raw: Value) -> ExecutorResult<Self> {
        let web_search_budget = normalize_native_web_search(&mut raw)?;
        Ok(Self {
            tool_choice: typed.tool_choice.take(),
            typed: typed.into(),
            raw,
            web_search_budget,
        })
    }

    /// The tools the client declared, for routing and registry construction.
    ///
    /// These are the client's declarations as received — before the upstream
    /// normalization applied to `raw` — which is what the tool seam
    /// needs to recognise a native server-tool declaration.
    #[must_use]
    pub fn tools(&self) -> Option<&Vec<ToolParam>> {
        self.typed.tools.as_ref()
    }

    /// Whether the client asked for a streaming response.
    #[must_use]
    pub fn stream(&self) -> bool {
        self.typed.stream
    }

    /// The model the client requested.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.typed.model
    }

    /// The body to POST upstream for the next round.
    ///
    /// # Errors
    /// Returns [`ExecutorError::JsonError`] if the body cannot be serialized.
    pub(super) fn upstream_body(&self) -> ExecutorResult<String> {
        serialize_to_string(&self.raw).map_err(ExecutorError::JsonError)
    }

    /// Force the upstream streaming mode, regardless of what the client asked.
    ///
    /// Each loop drives its own rounds and so pins `stream` to what it can
    /// consume; the client-facing mode is [`stream`](Self::stream), decided by
    /// the handler before the loop starts.
    pub(super) fn force_stream(&mut self, streaming: bool) {
        self.raw["stream"] = Value::Bool(streaming);
    }

    /// Reserve up to `requested` native web searches, returning how many may run.
    pub(super) fn reserve_searches(&mut self, requested: usize) -> usize {
        self.web_search_budget.reserve(requested)
    }

    /// Whether a finished round permits executing its gateway calls.
    /// vLLM maps named Chat Completions calls to Messages `end_turn`; accept
    /// that terminal only when the explicitly selected tool actually appears.
    /// Other stops (including truncation) retain their normal terminal behavior.
    pub(super) fn is_tool_call_stop<'a>(
        &self,
        stop_reason: Option<&str>,
        mut gateway_names: impl Iterator<Item = &'a str>,
    ) -> bool {
        if stop_reason == Some("tool_use") {
            return true;
        }
        stop_reason == Some("end_turn")
            && match self.tool_choice.as_ref() {
                Some(MessagesToolChoice::Tool { name, .. }) => gateway_names.any(|called| called == name.as_str()),
                Some(MessagesToolChoice::Auto(_) | MessagesToolChoice::Any(_) | MessagesToolChoice::None { .. })
                | None => false,
            }
    }

    /// Append the model's assistant turn (preserving its `thinking`/`text`/
    /// `tool_use` blocks in order — F3) and a following user turn of
    /// `tool_result`s, so the next upstream round sees the full conversation
    /// state. These stay internal — the client never sees them (hide-the-call).
    /// A fulfilled forced `tool_choice` becomes `auto` for subsequent rounds;
    /// parallel-use settings and extension fields remain unchanged.
    ///
    /// # Errors
    /// Returns [`ExecutorError::InvalidRequest`] if the body has no `messages`
    /// array to append to. Unreachable for a context built through either
    /// constructor, since `MessagesRequest::messages` is a required array —
    /// erroring keeps it from silently no-opping into a loop that re-POSTs an
    /// unchanged body until the round cap.
    pub(super) fn append_round(
        &mut self,
        assistant_content: &[Value],
        tool_results: Vec<GatewayToolResult>,
    ) -> ExecutorResult<()> {
        // A forced choice applies to this public turn. Once its gateway call
        // has a result, let the next inference round use that result to answer
        // instead of forcing another call until the round limit is reached.
        let fulfilled_choice = match self.tool_choice.as_ref() {
            Some(MessagesToolChoice::Any(_)) => !tool_results.is_empty(),
            Some(MessagesToolChoice::Tool { name, .. }) => assistant_content.iter().any(|block| {
                block["type"] == "tool_use"
                    && block["name"] == name.as_str()
                    && block
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| tool_results.iter().any(|result| result.tool_use_id == id))
            }),
            Some(MessagesToolChoice::Auto(_) | MessagesToolChoice::None { .. }) | None => false,
        };
        let messages = self
            .raw
            .get_mut("messages")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| ExecutorError::InvalidRequest("request has no messages array".to_owned()))?;
        messages.push(json!({ "role": "assistant", "content": assistant_content }));
        // Built by hand rather than with `json!` so the tool outputs move in
        // instead of being deep-copied — a web-search result runs to kilobytes.
        let mut user = Map::new();
        user.insert("role".to_owned(), Value::String("user".to_owned()));
        user.insert(
            "content".to_owned(),
            serde_json::to_value(tool_results).map_err(ExecutorError::JsonError)?,
        );
        messages.push(Value::Object(user));
        if fulfilled_choice && let Some(choice) = &mut self.tool_choice {
            match choice {
                MessagesToolChoice::Any(options) | MessagesToolChoice::Tool { options, .. } => {
                    *choice = MessagesToolChoice::Auto(std::mem::take(options));
                }
                MessagesToolChoice::Auto(_) | MessagesToolChoice::None { .. } => {}
            }
            self.raw["tool_choice"] = serde_json::to_value(choice).map_err(ExecutorError::JsonError)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Value {
        json!({
            "model": "qwen3", "max_tokens": 1024, "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "web_search", "type": "web_search_20250305", "max_uses": 2}]
        })
    }

    #[test]
    fn end_turn_requires_the_explicitly_selected_gateway_call() {
        for (choice, accepts_end_turn) in [
            (Value::Null, false),
            (json!({"type":"auto", "name":"web_search"}), false),
            (json!({"type":"any", "name":"web_search"}), false),
            (json!({"type":"none"}), false),
            (json!({"type":"tool", "name":"client_echo"}), false),
            (json!({"type":"tool", "name":"web_search"}), true),
        ] {
            let mut body = request();
            body["tool_choice"] = choice.clone();
            let ctx = MessagesRequestContext::from_value(body).unwrap();
            assert!(ctx.is_tool_call_stop(Some("tool_use"), ["web_search"].into_iter()));
            assert_eq!(
                ctx.is_tool_call_stop(Some("end_turn"), ["web_search"].into_iter()),
                accepts_end_turn,
            );
            assert!(!ctx.is_tool_call_stop(Some("end_turn"), std::iter::empty()));
            for reason in [
                None,
                Some("max_tokens"),
                Some("stop_sequence"),
                Some("pause_turn"),
                Some("future"),
            ] {
                assert!(!ctx.is_tool_call_stop(reason, ["web_search"].into_iter()));
            }
        }
    }

    #[test]
    fn typed_view_reads_client_fields_and_raw_carries_upstream_normalization() {
        let ctx = MessagesRequestContext::from_value(request()).unwrap();

        assert_eq!(ctx.model(), "qwen3");
        assert!(ctx.stream());
        // The typed view keeps the client's native declaration, which is what
        // the tool seam classifies on...
        let tools = ctx.tools().expect("tools");
        assert_eq!(tools[0].name, "web_search");
        assert_eq!(tools[0].type_.as_deref(), Some("web_search_20250305"));
        // ...while the raw body carries the function-tool shape vLLM accepts.
        assert_eq!(ctx.raw["tools"][0]["name"], "web_search");
        assert!(ctx.raw["tools"][0].get("type").is_none());
        assert!(ctx.raw["tools"][0].get("input_schema").is_some());
    }

    #[test]
    fn force_stream_overrides_the_client_mode_without_touching_the_typed_view() {
        let mut ctx = MessagesRequestContext::from_value(request()).unwrap();
        ctx.force_stream(false);

        assert_eq!(ctx.raw["stream"], json!(false));
        assert!(ctx.stream(), "the client's requested mode is still readable");
    }

    #[test]
    fn append_round_extends_the_raw_history_only() {
        let mut ctx = MessagesRequestContext::from_value(request()).unwrap();
        let assistant = vec![json!({"type": "tool_use", "id": "t1", "name": "web_search", "input": {}})];
        ctx.append_round(
            &assistant,
            vec![GatewayToolResult::new("t1", "answer".to_owned(), false)],
        )
        .unwrap();

        let messages = ctx.raw["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], json!(assistant));
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "t1");
        assert_eq!(messages[2]["content"][0]["content"], "answer");
        assert_eq!(messages[2]["content"][0]["is_error"], false);
    }

    #[test]
    fn budget_is_shared_across_rounds() {
        let mut ctx = MessagesRequestContext::from_value(request()).unwrap();
        assert_eq!(ctx.reserve_searches(1), 1);
        assert_eq!(ctx.reserve_searches(3), 1, "max_uses caps the request-wide total");
        assert_eq!(ctx.reserve_searches(1), 0);
    }

    #[test]
    fn invalid_native_web_search_declaration_is_rejected_at_construction() {
        let mut body = request();
        body["tools"][0]["max_uses"] = json!(0);
        let error = MessagesRequestContext::from_value(body).unwrap_err();
        assert!(matches!(error, ExecutorError::InvalidRequest(_)), "{error:?}");
    }

    #[test]
    fn unmodeled_blocks_and_cache_control_survive_in_the_raw_body() {
        // The reason the raw view exists: a typed round-trip would drop these.
        let body = json!({
            "model": "m", "max_tokens": 8,
            "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}},
                {"type": "redacted_thinking", "data": "enc"}
            ]}]
        });
        let ctx = MessagesRequestContext::from_value(body.clone()).unwrap();
        assert_eq!(ctx.raw, body);
    }

    #[test]
    fn parsed_request_builds_both_context_views_from_the_same_input() {
        let body = serde_json::to_vec(&request()).unwrap();
        let parsed = ParsedMessagesRequest::parse(&body).unwrap();
        let ctx = MessagesRequestContext::new(parsed).unwrap();

        assert_eq!(ctx.model(), "qwen3");
        assert_eq!(ctx.raw["messages"][0]["content"], "hi");
    }

    #[test]
    fn parsed_request_rejects_non_messages_json() {
        let error = ParsedMessagesRequest::parse(br"[]").unwrap_err();
        assert!(matches!(error, ExecutorError::JsonError(_)), "{error:?}");
    }

    #[test]
    fn malformed_gateway_selectors_fail_routing_including_configured_aliases() {
        let map = GatewayToolMap::from_pairs([("WebSearch", "web_search")]);
        for name in ["web_search", "WebSearch", "client_echo"] {
            let body = serde_json::to_vec(&json!({
                "model":"test", "max_tokens":64, "messages":[],
                "tools":[{"name":name, "input_schema":{"type":"object"}}],
                "tool_choice":{"type":"tool"}
            }))
            .unwrap();
            let parsed = ParsedMessagesRequest::parse_for_gateway(&body, &map);
            if name == "client_echo" {
                assert!(parsed.unwrap().is_none(), "proxy requests retain upstream validation");
            } else {
                assert!(parsed.is_err(), "gateway requests must not fall back to the proxy");
            }
        }
    }

    fn completed_search() -> (Vec<Value>, Vec<GatewayToolResult>) {
        (
            vec![json!({"type":"tool_use", "id":"search_1", "name":"web_search", "input":{}})],
            vec![GatewayToolResult::new("search_1", "answer".to_owned(), false)],
        )
    }

    #[test]
    fn fulfilled_forced_choice_allows_an_answer_and_preserves_other_settings() {
        for kind in ["any", "tool"] {
            for is_error in [false, true] {
                let mut body = request();
                let choice = json!({"type":kind, "name":"web_search", "disable_parallel_tool_use":true, "extension":{"value":1}});
                body["tool_choice"] = choice.clone();
                let mut ctx = MessagesRequestContext::from_value(body).unwrap();
                let initial = ctx.raw.clone();
                assert_eq!(initial["tool_choice"], choice);
                let (content, mut results) = completed_search();
                results[0].is_error = is_error;
                ctx.append_round(&content, results).unwrap();
                let mut expected = initial;
                expected["tool_choice"]["type"] = json!("auto");
                if kind == "tool" {
                    expected["tool_choice"].as_object_mut().unwrap().remove("name");
                }
                expected["messages"].as_array_mut().unwrap().extend([
                    json!({"role":"assistant", "content":content}),
                    json!({"role":"user", "content":[{"type":"tool_result", "tool_use_id":"search_1", "content":"answer", "is_error":is_error}]}),
                ]);
                assert_eq!(ctx.raw, expected);
                let (content, results) = completed_search();
                ctx.append_round(&content, results).unwrap();
                assert_eq!(ctx.raw["tool_choice"], expected["tool_choice"]);
                assert!(matches!(ctx.tool_choice, Some(MessagesToolChoice::Auto(_))));
                assert!(!ctx.is_tool_call_stop(Some("end_turn"), ["web_search"].into_iter()));
            }
        }
    }

    #[test]
    fn unforced_or_unfulfilled_choices_are_preserved() {
        for choice in [
            None,
            Some(Value::Null),
            Some(json!({"type":"auto", "disable_parallel_tool_use":false})),
            Some(json!({"type":"none"})),
            Some(json!({"type":"tool", "name":"client_tool"})),
        ] {
            let mut body = request();
            if let Some(choice) = &choice {
                body["tool_choice"] = choice.clone();
            }
            let mut ctx = MessagesRequestContext::from_value(body).unwrap();
            let (content, results) = completed_search();
            ctx.append_round(&content, results).unwrap();
            assert_eq!(ctx.raw.get("tool_choice"), choice.as_ref());
        }
    }

    #[test]
    fn forced_choice_is_not_fulfilled_without_its_call_result() {
        for kind in ["any", "tool"] {
            let mut body = request();
            body["tool_choice"] = json!({"type":kind});
            if kind == "tool" {
                body["tool_choice"]["name"] = json!("web_search");
            }
            let mut ctx = MessagesRequestContext::from_value(body.clone()).unwrap();
            let (content, _) = completed_search();
            ctx.append_round(&content, vec![]).unwrap();
            assert_eq!(ctx.raw["tool_choice"], body["tool_choice"]);
        }
        for content in [
            vec![],
            vec![json!({"type":"tool_use", "id":"unresolved", "name":"web_search"})],
            vec![json!({"type":"text", "id":"search_1", "name":"web_search"})],
        ] {
            let mut body = request();
            body["tool_choice"] = json!({"type":"tool", "name":"web_search"});
            let mut ctx = MessagesRequestContext::from_value(body.clone()).unwrap();
            let (_, results) = completed_search();
            ctx.append_round(&content, results).unwrap();
            assert_eq!(ctx.raw["tool_choice"], body["tool_choice"]);
        }
    }
}
