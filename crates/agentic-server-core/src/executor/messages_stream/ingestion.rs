//! Synchronous Messages event validation and block ingestion.

use serde_json::Value;

use super::{BufferedBlock, MessagesStreamAccumulator, RoundState, error_sse, sse};
use crate::events::{ClassifiedSseLine, SseLine};
use crate::utils::common::deserialize_from_str;

impl MessagesStreamAccumulator {
    /// Translate one upstream SSE line into zero or more client SSE lines.
    pub(super) fn push(&mut self, line: &str) -> Vec<String> {
        let ClassifiedSseLine::Data(data) = SseLine::parse(line) else {
            return Vec::new();
        };
        let Ok(mut event) = deserialize_from_str::<Value>(data.as_str()) else {
            self.round_state = RoundState::Failed;
            return vec![error_sse("invalid JSON in upstream Messages stream")];
        };
        if !self.round_started
            && matches!(
                event["type"].as_str(),
                Some(
                    "content_block_start"
                        | "content_block_delta"
                        | "content_block_stop"
                        | "message_delta"
                        | "message_stop"
                )
            )
        {
            return self.fail("upstream Messages event before message_start");
        }
        if matches!(
            event["type"].as_str(),
            Some("content_block_start" | "content_block_delta" | "content_block_stop")
        ) && event.get("index").and_then(Value::as_u64).is_none()
        {
            return self.fail("invalid content block index in upstream Messages stream");
        }
        let payload_field = match event["type"].as_str() {
            Some("content_block_start") => Some("content_block"),
            Some("content_block_delta") => Some("delta"),
            _ => None,
        };
        if let Some(field) = payload_field {
            if event[field]["type"].as_str().is_none_or(str::is_empty) {
                return self.fail("invalid content block payload in upstream Messages stream");
            }
        }
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => self.on_message_start(&event),
            Some("content_block_start") => self.on_block_start(&mut event),
            Some("content_block_delta") => self.on_block_delta(&mut event),
            Some("content_block_stop") => self.on_block_stop(&mut event),
            Some("message_delta") => {
                // Buffer as the (possibly) final terminal; suppress mid-loop.
                self.usage.observe(event.get("usage"));
                self.final_message_delta = Some(event);
                Vec::new()
            }
            Some("error") => {
                self.round_state = RoundState::UpstreamError;
                vec![sse("error", &event)]
            }
            Some("message_stop") => {
                if self.blocks.values().any(|block| !block.closed) {
                    self.round_state = RoundState::Failed;
                    return vec![error_sse("upstream Messages stream ended with open content blocks")];
                }
                self.round_state = RoundState::Completed;
                // `finish` emits the single client-visible terminal at loop end.
                Vec::new()
            }
            // Unknown events do not end the round.
            _ => Vec::new(),
        }
    }

    fn fail(&mut self, message: &str) -> Vec<String> {
        self.round_state = RoundState::Failed;
        vec![error_sse(message)]
    }

    fn on_message_start(&mut self, event: &Value) -> Vec<String> {
        if self.round_started {
            return self.fail("duplicate message_start in upstream Messages stream");
        }
        if event["message"]["id"].as_str().is_none_or(str::is_empty) {
            return self.fail("invalid identifier in upstream Messages stream");
        }
        self.round_started = true;
        // Later rounds' message_start is suppressed, but its usage still counts.
        self.usage.observe(event["message"].get("usage"));
        if self.message_started {
            return Vec::new();
        }
        self.message_started = true;
        vec![sse("message_start", event)]
    }

    fn on_block_start(&mut self, event: &mut Value) -> Vec<String> {
        let up_index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
        if self.blocks.contains_key(&up_index) {
            return self.fail("invalid content block transition in upstream Messages stream");
        }
        let block_type = event["content_block"]["type"].as_str().unwrap_or_default();
        let name = event["content_block"]["name"].as_str().unwrap_or_default();

        if block_type == "tool_use"
            && (name.is_empty() || event["content_block"]["id"].as_str().is_none_or(str::is_empty))
        {
            return self.fail("invalid identifier in upstream Messages stream");
        }

        // Buffer every block for history reconstruction (F3), preserving order.
        let is_gateway_tool = block_type == "tool_use" && self.gateway_map.is_gateway_owned(name);
        self.blocks.insert(
            up_index,
            BufferedBlock {
                closed: false,
                block: event["content_block"].clone(),
                input_json: String::new(),
                is_gateway_tool,
            },
        );

        if block_type == "tool_use" {
            if is_gateway_tool {
                // Suppress gateway-owned tool_use from the client; it stays in the
                // buffered history only and drives the loop.
                self.suppressed_indices.insert(up_index);
                return Vec::new();
            }
            // A client-owned tool_use: the client must execute it, so this round
            // is terminal (E7). Forward it (below) and stop the loop.
            self.has_client_tool_use = true;
        }

        // Forward with a rebased contiguous client index.
        let client_index = self.next_index;
        self.next_index += 1;
        self.index_map.insert(up_index, client_index);
        event["index"] = Value::from(client_index);
        vec![sse("content_block_start", event)]
    }

    fn on_block_delta(&mut self, event: &mut Value) -> Vec<String> {
        let up_index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
        if self.blocks.get(&up_index).is_none_or(|block| block.closed) {
            return self.fail("invalid content block transition in upstream Messages stream");
        }
        let fragment_field = match event["delta"]["type"].as_str() {
            Some("text_delta") => Some(("text", "text")),
            Some("thinking_delta") => Some(("thinking", "thinking")),
            Some("signature_delta") => Some(("signature", "thinking")),
            Some("input_json_delta") => Some(("partial_json", "tool_use")),
            _ => None,
        };
        if let Some((field, expected_block)) = fragment_field {
            if !event["delta"][field].is_string() {
                return self.fail("invalid content block delta in upstream Messages stream");
            }
            let block_kind = self
                .blocks
                .get(&up_index)
                .and_then(|block| block.block["type"].as_str());
            if matches!(block_kind, Some("text" | "thinking" | "tool_use")) && block_kind != Some(expected_block) {
                return self.fail("incompatible content block delta in upstream Messages stream");
            }
        }
        // Accumulate the delta into the buffered block (for history — F3),
        // regardless of whether it is forwarded to the client.
        if let Some(buffered) = self.blocks.get_mut(&up_index) {
            buffered.apply_delta(&event["delta"]);
        }
        // A suppressed gateway tool_use is not forwarded to the client (its
        // input_json_delta was just buffered above).
        if self.suppressed_indices.contains(&up_index) {
            return Vec::new();
        }
        let Some(&client_index) = self.index_map.get(&up_index) else {
            return Vec::new();
        };
        event["index"] = Value::from(client_index);
        vec![sse("content_block_delta", event)]
    }

    fn on_block_stop(&mut self, event: &mut Value) -> Vec<String> {
        let up_index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
        if self.blocks.get(&up_index).is_none_or(|block| block.closed) {
            return self.fail("invalid content block transition in upstream Messages stream");
        }
        if let Some(block) = self.blocks.get_mut(&up_index) {
            block.closed = true;
        }
        if self.suppressed_indices.contains(&up_index) {
            return Vec::new();
        }
        let Some(&client_index) = self.index_map.get(&up_index) else {
            return Vec::new();
        };
        event["index"] = Value::from(client_index);
        vec![sse("content_block_stop", event)]
    }
}
