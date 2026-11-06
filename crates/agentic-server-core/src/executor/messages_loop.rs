//! Messages-native gateway tool loop.
//!
//! Runs the server-side gateway-tool loop for `/v1/messages` **natively**: the
//! client's Anthropic request is forwarded to vLLM `/v1/messages` essentially
//! untouched on the first round, the assistant turn is
//! inspected, any gateway-owned `tool_use` is executed server-side,
//! the loop appends the `tool_result`, relaxes a fulfilled forced tool choice,
//! and re-POSTs until the model stops asking
//! for a gateway tool. The client receives one assistant message with the
//! content of every round and the `usage` of every round: a call to a native
//! server tool appears as its `server_tool_use` and result blocks, and any
//! other gateway call stays hidden.
//!
//! This never touches `RequestPayload`/`ResponsePayload`; it reuses only the
//! protocol-neutral tool bindings and scheduling policy via
//! [`crate::types::messages::tool_seam`]. Non-streaming only; streaming lives in
//! `messages_stream`.

use serde_json::{Value, json};
use tracing::Instrument as _;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::inference::fetch_response_json_with_headers;
use crate::executor::messages_context::MessagesRequestContext;
use crate::executor::messages_round::{MessagesRoundDecision, decide_round};
use crate::executor::messages_server_tools::{NativeServerTools, ServerToolView, client_tool_result};
use crate::executor::messages_tools::{ExecutedCall, execute_gateway_calls, request_gateway_map, resume_pending_mcp};
use crate::executor::messages_usage::MessagesUsageTotals;
use crate::executor::request::ExecutionContext;
use crate::executor::telemetry::metrics::{ExecutorMetrics, Stage};
use crate::executor::telemetry::{Api, ExecutionSpan, FailureCategory, Route};
use crate::tool::ToolRegistry;
use crate::types::messages::GatewayToolResult;
use crate::types::messages::mcp::McpContentBlock;
use crate::types::messages::tool_call::{MessagesToolCall, MessagesToolInputError};
use crate::types::messages::tool_seam;
use crate::utils::common::deserialize_from_str;

/// Max gateway rounds before the loop gives up. Each round is one upstream
/// `/v1/messages` call. Shared with the streaming loop (`messages_stream`).
/// Kept in sync with the Responses loop's `engine::MAX_GATEWAY_TOOL_ROUNDS`
/// (a future Layering-ADR consolidation would unify these).
pub(super) const MAX_GATEWAY_TOOL_ROUNDS: usize = 10;

/// Per-request transport data reused for every upstream Messages round.
#[derive(Clone, Debug)]
pub struct MessagesUpstream {
    url: String,
    headers: reqwest::header::HeaderMap,
}

impl MessagesUpstream {
    #[must_use]
    pub fn new(base_url: &str, query: Option<&str>, headers: reqwest::header::HeaderMap) -> Self {
        let mut url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
        if let Some(query) = query.filter(|query| !query.is_empty()) {
            url.push('?');
            url.push_str(query);
        }
        Self { url, headers }
    }

    pub(super) fn url(&self) -> &str {
        &self.url
    }

    pub(super) fn headers(&self) -> &reqwest::header::HeaderMap {
        &self.headers
    }
}

/// A Messages loop result paired with safe metadata from the relevant upstream response.
pub struct MessagesResponse<T> {
    /// The completed message or client-facing stream.
    pub body: T,
    /// Safe metadata retained from the terminal response, or the initial response for streaming.
    pub headers: http::HeaderMap,
}

/// Run the Messages-native gateway tool loop and return the final assistant
/// message (Anthropic JSON `Value`).
///
/// `ctx` carries the client's request in both views: its raw body is forwarded
/// upstream with `stream:false` forced and its `messages` extended each round.
///
/// # Errors
/// Returns [`ExecutorError`] on upstream failure or unparseable upstream JSON.
/// Gateway-tool execution failures do **not** error — they become error
/// `tool_result`s fed back to the model.
pub async fn run_messages_loop(
    ctx: MessagesRequestContext,
    registry: &ToolRegistry,
    exec_ctx: &ExecutionContext,
    upstream: &MessagesUpstream,
) -> ExecutorResult<MessagesResponse<Value>> {
    let mut execution = ExecutionSpan::start(Api::Messages, Route::Executor, false, &exec_ctx.metrics);
    let span = execution.span().clone();
    let result = run_messages_loop_traced(ctx, registry, exec_ctx, upstream, &mut execution)
        .instrument(span)
        .await;
    match &result {
        // Every `Ok` path inside states its own execution outcome; the
        // payload is now the handler's to send.
        Ok(_) => execution.delivered(),
        Err(error) => {
            execution.failed(error);
            execution.not_delivered();
        }
    }
    result
}

/// The body of [`run_messages_loop`], inside the `agentic.execute` span.
async fn run_messages_loop_traced(
    mut ctx: MessagesRequestContext,
    registry: &ToolRegistry,
    exec_ctx: &ExecutionContext,
    upstream: &MessagesUpstream,
    execution: &mut ExecutionSpan,
) -> ExecutorResult<MessagesResponse<Value>> {
    // The loop drives turns itself; force non-streaming upstream regardless of
    // what the client asked (the handler routes streaming elsewhere).
    ctx.force_stream(false);
    let key = exec_ctx.messages_result_key()?;
    ctx.replay_server_tool_blocks(key)?;
    let view = ServerToolView {
        native: ctx.native_server_tools(),
        key,
    };
    let mut turn = ClientTurn::default();
    // Gateway ownership is request-scoped: the operator aliases plus the
    // gateway tools this request's registry resolved.
    let gateway_map = request_gateway_map(ctx.gateway_tools_or(&exec_ctx.messages_gateway_tools), registry);
    let resumed = resume_pending_mcp(
        &mut ctx,
        registry,
        &gateway_map,
        view,
        &exec_ctx.gateway_scheduler_policy,
        &exec_ctx.metrics,
    )
    .await?;
    turn.record_resumed(&resumed)?;
    let mut rounds = exec_ctx.metrics.rounds(Api::Messages);

    for round in 0..MAX_GATEWAY_TOOL_ROUNDS {
        let body = ctx.upstream_body()?;
        rounds.begin_round();
        let timer = exec_ctx.metrics.stage(Stage::Inference);
        let fetched = fetch_response_json_with_headers(
            body,
            &upstream.url,
            &exec_ctx.client,
            &upstream.headers,
            exec_ctx.responses_config.max_upstream_json_bytes,
        )
        .instrument(super::telemetry::stages::inference_round(round))
        .await;
        timer.finish_result(&fetched);
        let (resp_text, response_headers) = fetched?;
        // A reply that is not a JSON object is refused like malformed JSON.
        let message = Value::Object(deserialize_from_str(&resp_text).map_err(ExecutorError::JsonError)?);
        record_message_usage(&exec_ctx.metrics, message.get("usage"));

        // Any error body from upstream is surfaced verbatim (handler maps it to
        // the Anthropic error envelope).
        if message.get("type").and_then(Value::as_str) == Some("error") {
            execution.failed_with(FailureCategory::UpstreamError);
            return Ok(MessagesResponse {
                body: message,
                headers: response_headers,
            });
        }

        let content = message.get("content").and_then(Value::as_array);
        let stop_reason = message.get("stop_reason").and_then(Value::as_str);

        // Split the assistant turn into gateway-owned tool_use vs everything the
        // client should see. A client-owned tool_use means we cannot continue
        // the loop server-side — return the turn to the client (edge E7).
        let Some(content) = content else {
            execution.completed_with_stop_reason(stop_reason);
            return deliver(
                message,
                turn,
                &gateway_map,
                MessagesRoundDecision::Finish,
                response_headers,
            );
        };
        let mut gateway_calls: Vec<&Value> = Vec::new();
        let mut has_client_tool_use = false;
        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                let name = block.get("name").and_then(Value::as_str).unwrap_or_default();
                if gateway_map.is_gateway_owned(name) {
                    gateway_calls.push(block);
                } else {
                    has_client_tool_use = true;
                }
            }
        }

        let decision = decide_round(
            &ctx,
            stop_reason,
            has_client_tool_use,
            gateway_calls.iter().filter_map(|call| call["name"].as_str()),
        );
        match decision {
            MessagesRoundDecision::ContinueGateway => {}
            MessagesRoundDecision::ReturnClient | MessagesRoundDecision::Finish => {
                execution.completed_with_stop_reason(stop_reason);
                return deliver(message, turn, &gateway_map, decision, response_headers);
            }
        }
        // Pure gateway-tool round: execute the calls, then feed the model's FULL
        // assistant turn (thinking/text/tool_use, order preserved — F3) plus the
        // tool_results back for the next round. The client is shown the round
        // with its native calls and their results.
        turn.usage.record(message.get("usage"));
        let executed = execute_gateway_calls(
            tool_uses(&gateway_calls),
            &mut ctx,
            registry,
            &gateway_map,
            view,
            &exec_ctx.gateway_scheduler_policy,
            &exec_ctx.metrics,
        )
        .await;
        let tool_results = turn.record_round(view.native, content, &gateway_map, executed)?;
        ctx.append_round(content, tool_results)?;
    }

    // Round budget exhausted — re-run once more is not attempted; return the
    // last message. (Open Q1: a dedicated pause_turn signal could go here.)
    // Reaching here means every round emitted a gateway tool_use; surface a
    // minimal terminal so the client isn't left hanging.
    execution.failed_with(FailureCategory::RoundBudget);
    Ok(round_budget_error())
}

/// The stable terminal response when inference exhausts its gateway round budget.
fn round_budget_error() -> MessagesResponse<Value> {
    MessagesResponse {
        body: json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": format!("gateway tool loop exceeded {MAX_GATEWAY_TOOL_ROUNDS} rounds")
            }
        }),
        headers: http::HeaderMap::new(),
    }
}

/// What the client is shown of the rounds executed so far, and the turn's usage.
#[derive(Default)]
struct ClientTurn {
    shown: Vec<Value>,
    usage: MessagesUsageTotals,
}

impl ClientTurn {
    /// Restored calls already appeared in the preceding response; show only their outputs.
    fn record_resumed(&mut self, resumed: &[GatewayToolResult]) -> ExecutorResult<()> {
        for result in resumed {
            self.shown
                .push(serde_json::to_value(McpContentBlock::result(result)).map_err(ExecutorError::JsonError)?);
        }
        Ok(())
    }

    /// Show one executed round and count its server tool uses; returns the
    /// round's `tool_result`s for the next inference.
    fn record_round(
        &mut self,
        native: NativeServerTools,
        content: &[Value],
        gateway_map: &tool_seam::GatewayToolMap,
        executed: Vec<ExecutedCall>,
    ) -> ExecutorResult<Vec<GatewayToolResult>> {
        let results = executed
            .iter()
            .filter_map(|call| client_tool_result(call, content, gateway_map));
        self.shown.extend(native.executed_round(content, gateway_map, results)?);
        Ok(executed
            .into_iter()
            .map(|call| {
                self.usage.add_server_tool_uses(call.uses);
                call.tool_result
            })
            .collect())
    }
}

/// Record one upstream response's own `usage`, before any turn totals are
/// folded into it.
fn record_message_usage(metrics: &ExecutorMetrics, usage: Option<&Value>) {
    let mut round = MessagesUsageTotals::default();
    round.observe(usage);
    let (input, output) = round.round_tokens();
    metrics.record_token_usage(Api::Messages, input, output);
}

/// Assemble the terminal client message and its accumulated usage.
/// Unexecuted native calls stay hidden. MCP calls with complete object arguments
/// remain public without results, including truncated rounds. A normal mixed
/// round must not publish invalid pending arguments for a later execution.
/// A final bare string remains a text block after earlier shown rounds.
fn deliver(
    mut message: Value,
    turn: ClientTurn,
    gateway_map: &tool_seam::GatewayToolMap,
    decision: MessagesRoundDecision,
    headers: http::HeaderMap,
) -> ExecutorResult<MessagesResponse<Value>> {
    let stop_reason = message.get("stop_reason").and_then(Value::as_str);
    let require_valid_pending =
        decision == MessagesRoundDecision::ReturnClient && matches!(stop_reason, Some("tool_use" | "end_turn"));
    let ClientTurn { mut shown, mut usage } = turn;
    let visible = match message.get("content") {
        Some(Value::Array(content)) => {
            if require_valid_pending
                && content.iter().any(|block| {
                    block["type"] == "tool_use"
                        && gateway_map
                            .mcp_identity(block["name"].as_str().unwrap_or_default())
                            .is_some()
                        && !block["input"].is_object()
                })
            {
                return Err(ExecutorError::InvalidRequest(format!(
                    "invalid arguments in pending MCP call: {}",
                    MessagesToolInputError::NonObject
                )));
            }
            Some(tool_seam::project_pending_mcp(content, gateway_map)?)
        }
        Some(Value::String(text)) => (!shown.is_empty()).then(|| vec![json!({"type": "text", "text": text})]),
        _ => (!shown.is_empty()).then(Vec::new),
    };
    let visible_stop = decision.visible_stop_reason(stop_reason);
    if visible_stop != stop_reason {
        message["stop_reason"] = json!(visible_stop);
    }
    if let (Some(visible), Some(fields)) = (visible, message.as_object_mut()) {
        shown.extend(visible);
        fields.insert("content".to_owned(), Value::Array(shown));
    }
    usage.finish(&mut message);
    Ok(MessagesResponse { body: message, headers })
}

/// The round's gateway-owned `tool_use` blocks as dispatchable calls.
///
/// F4: a malformed/absent input is reported rather than dispatched with args the
/// model never supplied. The block's `input` is already-parsed JSON here
/// (non-streaming), so it only has to be an object.
fn tool_uses(gateway_calls: &[&Value]) -> Vec<MessagesToolCall> {
    gateway_calls
        .iter()
        .map(|block| MessagesToolCall {
            id: block.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
            name: block.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
            input: block
                .get("input")
                .and_then(Value::as_object)
                .cloned()
                .ok_or(MessagesToolInputError::MissingObject),
        })
        .collect()
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    #[test]
    fn mixed_terminal_rejects_invalid_mcp_arguments_but_preserves_truncation() {
        let mut map = tool_seam::GatewayToolMap::default();
        map.insert_mcp("mcp_echo".to_owned(), "server".to_owned(), "echo".to_owned());
        for reason in ["tool_use", "end_turn", "max_tokens"] {
            for input in [json!({"text":"hello"}), json!(["invalid"]), Value::Null] {
                let valid = input.is_object();
                let message = json!({"type":"message", "stop_reason":reason, "content":[
                    {"type":"tool_use", "id":"m", "name":"mcp_echo", "input":input},
                    {"type":"tool_use", "id":"c", "name":"client_echo", "input":{}}
                ]});
                let result = deliver(
                    message,
                    ClientTurn::default(),
                    &map,
                    MessagesRoundDecision::ReturnClient,
                    http::HeaderMap::new(),
                );
                if !valid && reason != "max_tokens" {
                    assert!(matches!(result, Err(ExecutorError::InvalidRequest(_))));
                    continue;
                }
                let result = result.unwrap().body;
                assert_eq!(
                    result["stop_reason"],
                    if reason == "end_turn" { "tool_use" } else { reason }
                );
                let content = result["content"].as_array().unwrap();
                assert_eq!(content.iter().any(|block| block["type"] == "mcp_tool_use"), valid);
                assert!(!content.iter().any(|block| block["type"] == "mcp_tool_result"));
                assert!(content.iter().any(|block| block["id"] == "c"));
            }
        }
    }
}
