//! Assistant content blocks buffered across a streamed Messages round, the
//! gateway calls reconstructed from them, and their dispatch.

use serde_json::{Value, json};

use crate::types::messages::tool_call::{MessagesToolCall, parse_input};

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

    /// Finish the buffered block once. Dispatch and history share the parsed
    /// input; an invalid input remains an error on the call, never a dispatchable
    /// empty object. The legacy history fallback is preserved separately.
    pub(super) fn into_parts(self) -> (Value, Option<MessagesToolCall>) {
        let mut block = self.block;
        if block["type"] != "tool_use" {
            return (block, None);
        }
        let input = parse_input(&self.input_json);
        block["input"] = input
            .as_ref()
            .map_or_else(|_| json!({}), |object| Value::Object(object.clone()));
        let call = self.is_gateway_tool.then(|| MessagesToolCall {
            id: block["id"].as_str().unwrap_or_default().to_owned(),
            name: block["name"].as_str().unwrap_or_default().to_owned(),
            input,
        });
        (block, call)
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
