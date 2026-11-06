//! Assistant content blocks buffered across a streamed Messages round, the
//! gateway calls reconstructed from them, and their dispatch.

use serde_json::{Map, Value, json};

use crate::types::messages::tool_call::{MessagesToolCall, MessagesToolInputError, parse_input};

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
    /// One parsed argument state shared by client projection and dispatch.
    pub(super) parsed_input: Option<Result<Map<String, Value>, MessagesToolInputError>>,
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
                    self.parsed_input = None;
                }
            }
            _ => {}
        }
    }

    /// The block with its streamed `tool_use` input, as history records it: an
    /// input that does not parse as an object is recorded as `{}`.
    pub(super) fn completed_block(&mut self) -> Value {
        let mut block = self.block.clone();
        if block["type"] == "tool_use" || (block["type"] == "server_tool_use" && !self.input_json.is_empty()) {
            block["input"] = history_input(self.input());
        }
        block
    }

    pub(super) fn input(&mut self) -> &Result<Map<String, Value>, MessagesToolInputError> {
        self.parsed_input.get_or_insert_with(|| parse_input(&self.input_json))
    }

    /// Finish the buffered block once. Dispatch and history share the parsed
    /// input; an invalid input remains an error on the call, never a dispatchable
    /// empty object. The legacy history fallback is preserved separately.
    pub(super) fn into_parts(self) -> (Value, Option<MessagesToolCall>) {
        let mut block = self.block;
        if block["type"] != "tool_use" && (block["type"] != "server_tool_use" || self.input_json.is_empty()) {
            return (block, None);
        }
        let input = self.parsed_input.unwrap_or_else(|| parse_input(&self.input_json));
        block["input"] = history_input(&input);
        let call = self.is_gateway_tool.then(|| MessagesToolCall {
            id: block["id"].as_str().unwrap_or_default().to_owned(),
            name: block["name"].as_str().unwrap_or_default().to_owned(),
            input,
        });
        (block, call)
    }
}

/// The input history records for a streamed call.
fn history_input(input: &Result<Map<String, Value>, MessagesToolInputError>) -> Value {
    input
        .as_ref()
        .map_or_else(|_| json!({}), |object| Value::Object(object.clone()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_fallback_preserves_the_argument_error_for_dispatch() {
        let mut block = BufferedBlock {
            closed: true,
            block: json!({"type":"tool_use", "id":"m1", "name":"mcp_echo"}),
            input_json: "{\"text\":".to_owned(),
            is_gateway_tool: true,
            parsed_input: None,
        };
        assert!(block.input().is_err());
        assert_eq!(block.completed_block()["input"], json!({}));
        let (_, call) = block.into_parts();
        assert!(call.unwrap().input.is_err());
    }
}
