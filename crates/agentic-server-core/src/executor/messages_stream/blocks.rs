//! Assistant content blocks buffered across a streamed Messages round, the
//! gateway calls reconstructed from them, and their dispatch.

use serde_json::{Value, json};

use crate::executor::messages_context::MessagesRequestContext;
use crate::executor::messages_tools::{self, GatewayToolUse};
use crate::tool::ToolRegistry;
use crate::types::messages::{GatewayToolResult, tool_seam};

/// A gateway `tool_use` reconstructed from the stream, ready to dispatch.
pub(super) struct StreamedCall {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) input_json: String,
}

/// One assistant content block buffered across a round, so the full turn
/// (`thinking`/`text`/`signature`/`tool_use`, in order) can be reconstructed for
/// the next round's history — F3. The client-facing SSE is still forwarded live;
/// this is a parallel record for the fed-back conversation state.
pub(super) struct BufferedBlock {
    /// Whether this block received its upstream `content_block_stop`.
    pub(super) closed: bool,
    /// The `content_block` skeleton from `content_block_start`, mutated by deltas.
    pub(super) block: Value,
    /// Accumulated `input_json_delta` fragments for a `tool_use` block.
    pub(super) input_json: String,
    /// Gateway-owned `tool_use` (drives the loop; suppressed from the client).
    pub(super) is_gateway_tool: bool,
}

impl BufferedBlock {
    pub(super) fn apply_delta(&mut self, delta: &Value) {
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => append_str(&mut self.block, "text", delta.get("text")),
            Some("thinking_delta") => append_str(&mut self.block, "thinking", delta.get("thinking")),
            Some("signature_delta") => append_str(&mut self.block, "signature", delta.get("signature")),
            Some("input_json_delta") => {
                if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                    self.input_json.push_str(partial);
                }
            }
            _ => {}
        }
    }

    /// The finished assistant content block. For `tool_use`, parse the
    /// accumulated arguments (best-effort — a malformed fragment falls back to
    /// `{}`; the paired error `tool_result` records the failure).
    pub(super) fn to_block(&self) -> Value {
        let mut block = self.block.clone();
        if matches!(
            block.get("type").and_then(Value::as_str),
            Some("tool_use" | "server_tool_use")
        ) {
            block["input"] = tool_seam::parse_tool_input(&self.input_json).unwrap_or_else(|_| json!({}));
        }
        block
    }
}

/// Append a streamed string fragment onto a string field of `block`, creating it
/// if absent.
fn append_str(block: &mut Value, field: &str, fragment: Option<&Value>) {
    let Some(fragment) = fragment.and_then(Value::as_str) else {
        return;
    };
    let combined = match block.get(field).and_then(Value::as_str) {
        Some(existing) => format!("{existing}{fragment}"),
        None => fragment.to_owned(),
    };
    block[field] = Value::from(combined);
}

/// Execute reconstructed gateway calls through the dispatcher shared with the
/// JSON loop. Errors become error `tool_result`s (E5).
///
/// Returns one `tool_result` block per call, fed back next round. (The assistant
/// turn — including each call's `tool_use` block — is reconstructed from the
/// accumulator's buffered blocks in `MessagesStreamAccumulator::take_round`.)
pub(super) async fn execute_gateway_calls(
    calls: &[StreamedCall],
    ctx: &mut MessagesRequestContext,
    registry: &ToolRegistry,
    gateway_map: &tool_seam::GatewayToolMap,
) -> Vec<GatewayToolResult> {
    let tool_uses = calls
        .iter()
        .map(|call| GatewayToolUse {
            id: &call.id,
            name: &call.name,
            // F4: a malformed/incomplete reconstructed input is reported, never
            // coerced to {} and dispatched with args the model never sent.
            input: tool_seam::parse_tool_input(&call.input_json),
        })
        .collect();
    messages_tools::execute_gateway_calls(tool_uses, ctx, registry, gateway_map).await
}

impl super::MessagesStreamAccumulator {
    /// Consume this round's buffered blocks, returning (full assistant content in
    /// order, gateway calls to dispatch). The assistant content preserves
    /// `thinking`/`text`/`signature` and the gateway `tool_use` blocks (F3); the
    /// calls are the gateway `tool_use` blocks reconstructed for dispatch.
    pub(super) fn take_round(&mut self) -> (Vec<Value>, Vec<StreamedCall>) {
        self.usage.commit();
        self.take_terminal_round()
    }

    /// Consume terminal content without committing usage before final delivery.
    pub(super) fn take_terminal_round(&mut self) -> (Vec<Value>, Vec<StreamedCall>) {
        let blocks = std::mem::take(&mut self.blocks);
        let mut assistant_content = Vec::with_capacity(blocks.len());
        let mut calls = Vec::new();
        for buffered in blocks.values() {
            assistant_content.push(buffered.to_block());
            if buffered.is_gateway_tool {
                calls.push(StreamedCall {
                    id: buffered.block["id"].as_str().unwrap_or_default().to_owned(),
                    name: buffered.block["name"].as_str().unwrap_or_default().to_owned(),
                    input_json: buffered.input_json.clone(),
                });
            }
        }
        (assistant_content, calls)
    }
}

impl super::MessagesStreamAccumulator {
    /// Number of gateway `tool_use` blocks buffered this round.
    pub(super) fn gateway_call_count(&self) -> usize {
        self.blocks.values().filter(|b| b.is_gateway_tool).count()
    }
}
