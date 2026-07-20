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

/// The `tool_result` block for one executed gateway call, fed back next round.
/// (The model's own `tool_use` block is carried forward via the preserved
/// assistant content, not reconstructed here — see `append_round_to_history`.)
struct ResolvedCall {
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
        let gateway_map = &exec_ctx.messages_gateway_tools;
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

        // Terminal when the model didn't ask for a gateway tool, or stopped for
        // another reason. A client-owned tool_use is also terminal (the client
        // must run it) — but the gateway tool_use, if any, must still be hidden
        // (F5): strip gateway blocks from the client-facing content.
        if gateway_calls.is_empty() || stop_reason != Some("tool_use") {
            return Ok(message);
        }
        if has_client_tool_use {
            // Strip the gateway tool_use from the client-facing content (compute
            // before mutating to end the immutable borrow of `message`).
            let stripped = tool_seam::strip_gateway_tool_use(content, gateway_map);
            let mut message = message;
            message["content"] = Value::Array(stripped);
            return Ok(message);
        }

        // Pure gateway-tool round: execute the calls, then feed the model's FULL
        // assistant turn (thinking/text/tool_use, order preserved — F3) plus the
        // tool_results back for the next round. Gateway blocks stay internal.
        let assistant_content = content.clone();
        let resolved = execute_gateway_calls(&gateway_calls, registry, gateway_map).await;
        append_round_to_history(&mut request, &assistant_content, &resolved);
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
async fn execute_gateway_calls(
    gateway_calls: &[Value],
    registry: &ToolRegistry,
    gateway_map: &tool_seam::GatewayToolMap,
) -> Vec<ResolvedCall> {
    let futures = gateway_calls.iter().map(|block| async move {
        let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
        let name = block.get("name").and_then(Value::as_str).unwrap_or_default();

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

        ResolvedCall {
            tool_result_block: tool_seam::tool_result_block(id, &output, is_error),
        }
    });
    join_all(futures).await
}

/// Append the model's assistant turn (preserving its `thinking`/`text`/`tool_use`
/// blocks in order — F3) and a following user turn of `tool_result`s to the
/// request `messages`, so the next upstream round sees the full conversation
/// state. These stay internal — the client never sees them (hide-the-call).
fn append_round_to_history(request: &mut Value, assistant_content: &[Value], resolved: &[ResolvedCall]) {
    let assistant = json!({ "role": "assistant", "content": assistant_content });
    let user = json!({
        "role": "user",
        "content": resolved.iter().map(|r| r.tool_result_block.clone()).collect::<Vec<_>>()
    });
    if let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) {
        messages.push(assistant);
        messages.push(user);
    }
}
