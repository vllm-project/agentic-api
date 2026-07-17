//! Messages-native gateway tool loop.
//!
//! Runs the server-side gateway-tool loop for `/v1/messages` **natively**: the
//! client's Anthropic request is forwarded to vLLM `/v1/messages` essentially
//! untouched (preserving every Anthropic field), the assistant turn is
//! inspected, any gateway-owned `tool_use` is executed server-side and hidden,
//! the loop appends the `tool_result` and re-POSTs, until the model stops asking
//! for a gateway tool. Only the final assistant message reaches the client.
//!
//! This never touches `RequestPayload`/`ResponsePayload`; it reuses only the
//! protocol-neutral tool layer (`ToolRegistry::dispatch`) via
//! [`crate::types::messages::tool_seam`]. Non-streaming only; streaming lives in
//! `messages_stream`.

use std::time::Duration;

use futures::future::join_all;
use serde_json::{Value, json};

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::inference::fetch_response_json;
use crate::executor::request::ExecutionContext;
use crate::tool::ToolRegistry;
use crate::types::messages::tool_seam;
use crate::utils::common::{deserialize_from_str, serialize_to_string};

/// Max gateway rounds before the loop gives up. Each round is one upstream
/// `/v1/messages` call. Shared with the streaming loop (`messages_stream`).
/// Kept in sync with the Responses loop's `engine::MAX_GATEWAY_TOOL_ROUNDS`
/// (a future Layering-ADR consolidation would unify these).
pub(super) const MAX_GATEWAY_TOOL_ROUNDS: usize = 10;

/// Per gateway-tool-call timeout — a hung tool becomes an error `tool_result`
/// fed back to the model, never a whole-request failure (edge E5). Shared with
/// the streaming loop; matches the Responses loop's `gateway::GATEWAY_TOOL_TIMEOUT`.
pub(super) const GATEWAY_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// A gateway-owned `tool_use` extracted from the assistant turn, plus the
/// executed result to feed back.
struct ResolvedCall {
    /// The original assistant `tool_use` block (echoed back in history).
    tool_use_block: Value,
    /// The `tool_result` block to feed back next round.
    tool_result_block: Value,
}

/// Run the Messages-native gateway tool loop and return the final assistant
/// message (Anthropic JSON `Value`).
///
/// `request` is the client's parsed request body as JSON — forwarded upstream
/// with `stream:false` forced and its `messages` extended each round.
///
/// # Errors
/// Returns [`ExecutorError`] on upstream failure or unparseable upstream JSON.
/// Gateway-tool execution failures do **not** error — they become error
/// `tool_result`s fed back to the model.
pub async fn run_messages_loop(
    mut request: Value,
    registry: &ToolRegistry,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
) -> ExecutorResult<Value> {
    let url = format!("{}/v1/messages", exec_ctx.llm_base_url);
    // The loop drives turns itself; force non-streaming upstream regardless of
    // what the client asked (the handler routes streaming elsewhere).
    request["stream"] = Value::Bool(false);

    for _round in 0..MAX_GATEWAY_TOOL_ROUNDS {
        let body = serialize_to_string(&request).map_err(ExecutorError::JsonError)?;
        let resp_text = fetch_response_json(body, &url, &exec_ctx.client, auth).await?;
        let message: Value = deserialize_from_str(&resp_text).map_err(ExecutorError::JsonError)?;

        // Any error body from upstream is surfaced verbatim (handler maps it to
        // the Anthropic error envelope).
        if message.get("type").and_then(Value::as_str) == Some("error") {
            return Ok(message);
        }

        let content = message.get("content").and_then(Value::as_array);
        let stop_reason = message.get("stop_reason").and_then(Value::as_str);

        // Split the assistant turn into gateway-owned tool_use vs everything the
        // client should see. A client-owned tool_use means we cannot continue
        // the loop server-side — return the turn to the client (edge E7).
        let Some(content) = content else {
            return Ok(message);
        };
        let mut gateway_calls: Vec<Value> = Vec::new();
        let mut has_client_tool_use = false;
        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                let name = block.get("name").and_then(Value::as_str).unwrap_or_default();
                if tool_seam::is_gateway_owned_tool_name(name) {
                    gateway_calls.push(block.clone());
                } else {
                    has_client_tool_use = true;
                }
            }
        }

        // Terminal: the model didn't ask for a gateway tool (or asked for a
        // client tool we must hand back, or stopped for another reason).
        if gateway_calls.is_empty() || has_client_tool_use || stop_reason != Some("tool_use") {
            return Ok(message);
        }

        // Execute all gateway-owned calls (bounded, concurrent) and append the
        // assistant turn + a user turn of tool_results for the next round.
        let resolved = execute_gateway_calls(&gateway_calls, registry).await;
        append_round_to_history(&mut request, &resolved);
    }

    // Round budget exhausted — re-run once more is not attempted; return the
    // last message. (Open Q1: a dedicated pause_turn signal could go here.)
    // Reaching here means every round emitted a gateway tool_use; surface a
    // minimal terminal so the client isn't left hanging.
    Ok(json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": format!("gateway tool loop exceeded {MAX_GATEWAY_TOOL_ROUNDS} rounds")
        }
    }))
}

/// Execute the gateway-owned `tool_use` blocks concurrently, each bounded by the
/// per-call timeout. A failure or timeout becomes an error `tool_result` (E5).
async fn execute_gateway_calls(gateway_calls: &[Value], registry: &ToolRegistry) -> Vec<ResolvedCall> {
    let futures = gateway_calls.iter().map(|block| async move {
        let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
        let name = block.get("name").and_then(Value::as_str).unwrap_or_default();
        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
        let call = tool_seam::tool_use_to_call(id, name, &input);

        let output = match tokio::time::timeout(GATEWAY_TOOL_TIMEOUT, registry.dispatch(&call)).await {
            Ok(Some(result)) => match result.output {
                Ok(tool_output) => tool_output.output,
                Err(e) => format!("tool execution failed: {e}"),
            },
            // Unknown tool (no handler) — shouldn't happen for gateway-owned, but
            // feed it back rather than dropping the call.
            Ok(None) => format!("no handler for tool '{name}'"),
            Err(_) => format!("gateway tool '{name}' timed out after {GATEWAY_TOOL_TIMEOUT:?}"),
        };

        ResolvedCall {
            tool_use_block: tool_seam::call_to_tool_use_block(&call),
            tool_result_block: tool_seam::tool_result_block(id, &output),
        }
    });
    join_all(futures).await
}

/// Append the assistant turn (its gateway `tool_use` blocks) and a following
/// user turn (the matching `tool_result` blocks) to the request `messages` so
/// the next upstream round sees the resolved calls. These stay internal — the
/// client never sees them (hide-the-call).
fn append_round_to_history(request: &mut Value, resolved: &[ResolvedCall]) {
    let assistant = json!({
        "role": "assistant",
        "content": resolved.iter().map(|r| r.tool_use_block.clone()).collect::<Vec<_>>()
    });
    let user = json!({
        "role": "user",
        "content": resolved.iter().map(|r| r.tool_result_block.clone()).collect::<Vec<_>>()
    });
    if let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) {
        messages.push(assistant);
        messages.push(user);
    }
}
