//! Messages-native gateway tool loop.
//!
//! Runs the server-side gateway-tool loop for `/v1/messages` **natively**: the
//! client's Anthropic request is forwarded to vLLM `/v1/messages` essentially
//! untouched on the first round, the assistant turn is
//! inspected, any gateway-owned `tool_use` is executed server-side and hidden,
//! the loop appends the `tool_result`, relaxes a fulfilled forced tool choice,
//! and re-POSTs until the model stops asking
//! for a gateway tool. Only the final assistant message reaches the client,
//! carrying the `usage` of every round.
//!
//! This never touches `RequestPayload`/`ResponsePayload`; it reuses only the
//! protocol-neutral tool layer (`ToolRegistry::dispatch`) via
//! [`crate::types::messages::tool_seam`]. Non-streaming only; streaming lives in
//! `messages_stream`.

use std::time::Duration;

use futures::future::join_all;
use serde_json::{Value, json};
use tracing::Instrument as _;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::inference::fetch_response_json_with_headers;
use crate::executor::messages_context::MessagesRequestContext;
use crate::executor::messages_request::web_search_budget_exhausted_result;
use crate::executor::messages_usage::MessagesUsageTotals;
use crate::executor::request::ExecutionContext;
use crate::executor::telemetry::{Api, ExecutionSpan, FailureCategory, Route};
use crate::tool::ToolRegistry;
use crate::types::messages::{GatewayToolResult, tool_seam};
use crate::utils::common::deserialize_from_str;

/// Max gateway rounds before the loop gives up. Each round is one upstream
/// `/v1/messages` call. Shared with the streaming loop (`messages_stream`).
/// Kept in sync with the Responses loop's `engine::MAX_GATEWAY_TOOL_ROUNDS`
/// (a future Layering-ADR consolidation would unify these).
pub(super) const MAX_GATEWAY_TOOL_ROUNDS: usize = 10;

/// Per gateway-tool-call timeout — a hung tool becomes an error `tool_result`
/// fed back to the model, never a whole-request failure (edge E5). Shared with
/// the streaming loop; matches the Responses loop's `gateway::GATEWAY_TOOL_TIMEOUT`.
pub(super) const GATEWAY_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

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
    let mut execution = ExecutionSpan::start(Api::Messages, Route::Executor, false);
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
    let mut usage = MessagesUsageTotals::default();

    for round in 0..MAX_GATEWAY_TOOL_ROUNDS {
        let body = ctx.upstream_body()?;
        let (resp_text, response_headers) = fetch_response_json_with_headers(
            body,
            &upstream.url,
            &exec_ctx.client,
            &upstream.headers,
            exec_ctx.responses_config.max_upstream_json_bytes,
        )
        .instrument(super::telemetry::stages::inference_round(round))
        .await?;
        let message: Value = deserialize_from_str(&resp_text).map_err(ExecutorError::JsonError)?;

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
        let gateway_map = &exec_ctx.messages_gateway_tools;
        let Some(content) = content else {
            execution.completed_with_stop_reason(stop_reason);
            return Ok(deliver(message, &mut usage, gateway_map, response_headers));
        };
        let mut gateway_calls: Vec<Value> = Vec::new();
        let mut has_client_tool_use = false;
        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                let name = block.get("name").and_then(Value::as_str).unwrap_or_default();
                if gateway_map.is_gateway_owned(name) {
                    gateway_calls.push(block.clone());
                } else {
                    has_client_tool_use = true;
                }
            }
        }

        if has_client_tool_use {
            // Client execution takes precedence even when the provider labels a
            // named call end_turn. `deliver` keeps the hidden gateway calls out.
            execution.completed_with_stop_reason(stop_reason);
            let mut message = message;
            if message["stop_reason"] == "end_turn" {
                message["stop_reason"] = json!("tool_use");
            }
            return Ok(deliver(message, &mut usage, gateway_map, response_headers));
        }

        // The shared context accepts tool_use and vLLM's end_turn for a matching
        // named gateway call. Other stops retain their terminal behavior.
        if gateway_calls.is_empty()
            || !ctx.is_tool_call_stop(
                stop_reason,
                gateway_calls.iter().filter_map(|call| call["name"].as_str()),
            )
        {
            execution.completed_with_stop_reason(stop_reason);
            return Ok(deliver(message, &mut usage, gateway_map, response_headers));
        }
        // Pure gateway-tool round: execute the calls, then feed the model's FULL
        // assistant turn (thinking/text/tool_use, order preserved — F3) plus the
        // tool_results back for the next round. Gateway blocks stay internal.
        usage.record(message.get("usage"));
        let allowed_searches = ctx.reserve_searches(gateway_calls.len());
        let tool_results = execute_gateway_calls(&gateway_calls, registry, gateway_map, allowed_searches).await;
        ctx.append_round(content, tool_results)?;
    }

    // Round budget exhausted — re-run once more is not attempted; return the
    // last message. (Open Q1: a dedicated pause_turn signal could go here.)
    // Reaching here means every round emitted a gateway tool_use; surface a
    // minimal terminal so the client isn't left hanging.
    execution.failed_with(FailureCategory::RoundBudget);
    Ok(MessagesResponse {
        body: json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": format!("gateway tool loop exceeded {MAX_GATEWAY_TOOL_ROUNDS} rounds")
            }
        }),
        headers: http::HeaderMap::new(),
    })
}

/// Return the terminal assistant message with the turn's complete `usage` and
/// no gateway-owned `tool_use`.
///
/// Hide-the-call applies to every terminal round, not just a round that also
/// carries a client call. The client declares these tools for the gateway to
/// execute — a native `web_search_20250305` declaration is even rewritten into
/// an ordinary function tool for upstream — so a surfaced call names a tool the
/// client never agreed to run. A round can end while a gateway call is present
/// whenever the stop reason is not a tool-call stop, for example a `max_tokens`
/// truncation mid-call. The streaming loop already suppresses these blocks on
/// every round; this is the non-streaming half of the same contract.
fn deliver(
    mut message: Value,
    usage: &mut MessagesUsageTotals,
    gateway_map: &tool_seam::GatewayToolMap,
    headers: http::HeaderMap,
) -> MessagesResponse<Value> {
    if let Some(content) = message.get("content").and_then(Value::as_array) {
        let visible = tool_seam::strip_gateway_tool_use(content, gateway_map);
        if visible.len() != content.len() {
            message["content"] = Value::Array(visible);
        }
    }
    usage.finish(&mut message);
    MessagesResponse { body: message, headers }
}

/// Execute the gateway-owned `tool_use` blocks concurrently, each bounded by the
/// per-call timeout. A failure or timeout becomes an error `tool_result` (E5).
///
/// Returns one `tool_result` block per call, fed back next round. (The model's
/// own `tool_use` block is carried forward via the preserved assistant content,
/// not reconstructed here — see [`MessagesRequestContext::append_round`].)
async fn execute_gateway_calls(
    gateway_calls: &[Value],
    registry: &ToolRegistry,
    gateway_map: &tool_seam::GatewayToolMap,
    allowed_searches: usize,
) -> Vec<GatewayToolResult> {
    let futures = gateway_calls.iter().enumerate().map(|(index, block)| async move {
        let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
        let name = block.get("name").and_then(Value::as_str).unwrap_or_default();

        if index >= allowed_searches {
            return web_search_budget_exhausted_result(id);
        }

        // F4: reject a malformed/absent input rather than dispatching with args
        // the model never supplied. The block's `input` is already-parsed JSON
        // here (non-streaming), so validate it's an object.
        let input = block.get("input").cloned().unwrap_or(Value::Null);
        let (output, is_error) = if input.is_object() {
            let call = tool_seam::tool_use_to_call(id, name, &input, gateway_map);
            match tokio::time::timeout(GATEWAY_TOOL_TIMEOUT, registry.dispatch(&call)).await {
                Ok(Some(result)) => match result.output {
                    Ok(tool_output) => (tool_output.output, false),
                    Err(e) => (format!("tool execution failed: {e}"), true),
                },
                Ok(None) => (format!("no handler for tool '{name}'"), true),
                Err(_) => (
                    format!("gateway tool '{name}' timed out after {GATEWAY_TOOL_TIMEOUT:?}"),
                    true,
                ),
            }
        } else {
            (
                "invalid tool arguments (not a JSON object); tool was not run".to_owned(),
                true,
            )
        };

        tool_seam::tool_result_block(id, output, is_error)
    });
    join_all(futures).await
}
