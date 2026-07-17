//! Streaming Messages-native gateway tool loop.
//!
//! Consumes vLLM's per-round Anthropic SSE and presents the client **one**
//! logical message across all gateway rounds:
//!   * `message_start` emitted once (first round only);
//!   * surfaced `content_block_*` forwarded with client-visible indices rebased
//!     contiguously across rounds;
//!   * gateway-owned `tool_use` blocks suppressed (and their `input_json_delta`
//!     buffered to reconstruct the call for dispatch);
//!   * intermediate `message_delta`/`message_stop` (the per-round terminals)
//!     suppressed; the final round's terminal is forwarded once.
//!
//! Structurally the Anthropic-native analogue of the Responses `GatewayStreamAccumulator`
//! (#119/#132); kept deliberately parallel for a future consolidation. Reuses
//! only the neutral tool layer via [`crate::types::messages::tool_seam`].

use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use futures::StreamExt;
use serde_json::{Value, json};

use crate::executor::inference::{BoxStream, call_inference};
use crate::executor::request::ExecutionContext;
use crate::tool::ToolRegistry;
use crate::types::messages::tool_seam;
use crate::utils::common::{serialize_to_string, serialize_to_vec_or_default};

// Shared with the non-streaming loop so the two Messages loops can't drift.
use crate::executor::messages_loop::{GATEWAY_TOOL_TIMEOUT, MAX_GATEWAY_TOOL_ROUNDS};
/// vLLM streaming chunk timeout (per line). Generous — the loop's own budget is
/// the round cap, not this.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(120);

/// Drive the streaming Messages-native loop, yielding Anthropic SSE lines for
/// the client. Owns the multi-round → single-message accumulation.
#[must_use]
pub fn run_messages_stream(
    mut request: Value,
    registry: Arc<ToolRegistry>,
    exec_ctx: Arc<ExecutionContext>,
    auth: Option<String>,
) -> BoxStream {
    let url = format!("{}/v1/messages", exec_ctx.llm_base_url);
    request["stream"] = Value::Bool(true);

    Box::pin(stream! {
        let mut acc = MessagesStreamAccumulator::new();

        for _round in 0..MAX_GATEWAY_TOOL_ROUNDS {
            let body = match serialize_to_string(&request) {
                Ok(b) => b,
                Err(e) => { yield error_sse(&e.to_string()); return; }
            };
            let mut upstream = Box::pin(call_inference(
                body, url.clone(), Arc::clone(&exec_ctx.client), auth.clone(), CHUNK_TIMEOUT,
            ));

            acc.begin_round();
            while let Some(line) = upstream.next().await {
                let line = match line {
                    Ok(l) => l,
                    Err(e) => { yield error_sse(&e.to_string()); return; }
                };
                for out in acc.push(&line) {
                    yield out;
                }
            }

            // Round finished. Continue only for a pure gateway-tool round; a
            // client-owned tool_use (or any non-tool_use stop) is terminal.
            if !acc.should_continue_loop() {
                for out in acc.finish() {
                    yield out;
                }
                return;
            }
            let calls = acc.take_gateway_calls();
            let resolved = execute_gateway_calls(&calls, &registry).await;
            append_round_to_history(&mut request, &resolved);
        }

        // Round budget exhausted.
        yield error_sse(&format!("gateway tool loop exceeded {MAX_GATEWAY_TOOL_ROUNDS} rounds"));
    })
}

/// A gateway `tool_use` reconstructed from the stream, ready to dispatch.
struct StreamedCall {
    id: String,
    name: String,
    input_json: String,
}

/// State machine that turns per-round Anthropic SSE into one client-visible
/// message. Fed line-by-line via [`Self::push`].
struct MessagesStreamAccumulator {
    message_started: bool,
    /// Next client-visible block index (contiguous across rounds).
    next_index: u32,
    /// Map upstream (per-round) block index → client index, for the blocks we
    /// forward this round. Cleared each round.
    index_map: std::collections::HashMap<u64, u32>,
    /// Upstream indices belonging to a suppressed gateway `tool_use` this round.
    suppressed_indices: std::collections::HashSet<u64>,
    /// Gateway calls reconstructed this round (id/name known at `block_start`,
    /// input accumulated from `input_json_delta`).
    calls: Vec<StreamedCall>,
    /// Upstream index → position in `calls`, to route `input_json_delta`.
    call_by_index: std::collections::HashMap<u64, usize>,
    /// Did this round end with `stop_reason: tool_use`?
    ended_on_tool_use: bool,
    /// Did this round surface a client-owned `tool_use`? If so the loop cannot
    /// continue server-side (the client must supply that tool's result), so it
    /// is terminal — matching the non-streaming path's E7 handling.
    has_client_tool_use: bool,
    /// Buffered terminal `message_delta` from the final round (emitted by `finish`).
    final_message_delta: Option<Value>,
}

impl MessagesStreamAccumulator {
    fn new() -> Self {
        Self {
            message_started: false,
            next_index: 0,
            index_map: std::collections::HashMap::new(),
            suppressed_indices: std::collections::HashSet::new(),
            calls: Vec::new(),
            call_by_index: std::collections::HashMap::new(),
            ended_on_tool_use: false,
            has_client_tool_use: false,
            final_message_delta: None,
        }
    }

    fn begin_round(&mut self) {
        self.index_map.clear();
        self.suppressed_indices.clear();
        self.call_by_index.clear();
        self.calls.clear();
        self.ended_on_tool_use = false;
        self.has_client_tool_use = false;
    }

    fn take_gateway_calls(&mut self) -> Vec<StreamedCall> {
        std::mem::take(&mut self.calls)
    }

    /// The loop should continue only when the round asked for a gateway tool AND
    /// did not also surface a client-owned tool (which the client must handle,
    /// making the round terminal — E7).
    fn should_continue_loop(&self) -> bool {
        self.ended_on_tool_use && !self.calls.is_empty() && !self.has_client_tool_use
    }

    /// Translate one upstream SSE line into zero or more client SSE lines.
    fn push(&mut self, line: &str) -> Vec<String> {
        let Some(data) = line.strip_prefix("data: ") else {
            return Vec::new();
        };
        let data = data.trim();
        if data == "[DONE]" {
            return Vec::new();
        }
        let Ok(mut event) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => self.on_message_start(&event),
            Some("content_block_start") => self.on_block_start(&mut event),
            Some("content_block_delta") => self.on_block_delta(&mut event),
            Some("content_block_stop") => self.on_block_stop(&mut event),
            Some("message_delta") => {
                // Buffer as the (possibly) final terminal; suppress mid-loop.
                self.ended_on_tool_use = event["delta"]["stop_reason"].as_str() == Some("tool_use");
                self.final_message_delta = Some(event);
                Vec::new()
            }
            // `message_stop` (per-round terminal) is suppressed; `finish` emits
            // the single client-visible terminal. Everything else is dropped.
            _ => Vec::new(),
        }
    }

    fn on_message_start(&mut self, event: &Value) -> Vec<String> {
        if self.message_started {
            return Vec::new();
        }
        self.message_started = true;
        vec![sse("message_start", event)]
    }

    fn on_block_start(&mut self, event: &mut Value) -> Vec<String> {
        let up_index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
        let block_type = event["content_block"]["type"].as_str().unwrap_or_default();
        let name = event["content_block"]["name"].as_str().unwrap_or_default();

        if block_type == "tool_use" {
            if tool_seam::is_gateway_owned_tool_name(name) {
                // Suppress gateway-owned tool_use blocks; buffer them for dispatch.
                self.suppressed_indices.insert(up_index);
                let id = event["content_block"]["id"].as_str().unwrap_or_default().to_owned();
                self.call_by_index.insert(up_index, self.calls.len());
                self.calls.push(StreamedCall {
                    id,
                    name: name.to_owned(),
                    input_json: String::new(),
                });
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
        // Buffer input_json_delta for a suppressed gateway call.
        if let Some(pos) = self.call_by_index.get(&up_index) {
            if let Some(partial) = event["delta"]["partial_json"].as_str() {
                self.calls[*pos].input_json.push_str(partial);
            }
            return Vec::new();
        }
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
        if self.suppressed_indices.contains(&up_index) {
            return Vec::new();
        }
        let Some(&client_index) = self.index_map.get(&up_index) else {
            return Vec::new();
        };
        event["index"] = Value::from(client_index);
        vec![sse("content_block_stop", event)]
    }

    /// Emit the terminal `message_delta` + `message_stop` once, at loop end.
    fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(delta) = self.final_message_delta.take() {
            out.push(sse("message_delta", &delta));
        }
        out.push(sse("message_stop", &json!({"type": "message_stop"})));
        out
    }
}

fn sse(event: &str, value: &Value) -> String {
    let json = String::from_utf8(serialize_to_vec_or_default(value)).unwrap_or_default();
    format!("event: {event}\ndata: {json}\n\n")
}

fn error_sse(message: &str) -> String {
    let event = json!({"type": "error", "error": {"type": "api_error", "message": message}});
    let json = String::from_utf8(serialize_to_vec_or_default(&event)).unwrap_or_default();
    format!("event: error\ndata: {json}\n\n")
}

/// Execute reconstructed gateway calls (concurrent, per-call timeout). Errors
/// become error `tool_result`s (E5).
async fn execute_gateway_calls(calls: &[StreamedCall], registry: &ToolRegistry) -> Vec<ResolvedStreamCall> {
    let futures = calls.iter().map(|c| async move {
        let input: Value = serde_json::from_str(&c.input_json).unwrap_or_else(|_| json!({}));
        let call = tool_seam::tool_use_to_call(&c.id, &c.name, &input);
        let output = match tokio::time::timeout(GATEWAY_TOOL_TIMEOUT, registry.dispatch(&call)).await {
            Ok(Some(result)) => match result.output {
                Ok(o) => o.output,
                Err(e) => format!("tool execution failed: {e}"),
            },
            Ok(None) => format!("no handler for tool '{}'", c.name),
            Err(_) => format!("gateway tool '{}' timed out after {GATEWAY_TOOL_TIMEOUT:?}", c.name),
        };
        ResolvedStreamCall {
            tool_use_block: tool_seam::call_to_tool_use_block(&call),
            tool_result_block: tool_seam::tool_result_block(&c.id, &output),
        }
    });
    futures::future::join_all(futures).await
}

struct ResolvedStreamCall {
    tool_use_block: Value,
    tool_result_block: Value,
}

fn append_round_to_history(request: &mut Value, resolved: &[ResolvedStreamCall]) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: &Value) -> String {
        format!("data: {v}")
    }

    // A single non-tool round: message_start forwarded once, blocks pass through
    // with contiguous indices, terminal emitted by finish().
    #[test]
    fn single_round_text_passes_through() {
        let mut acc = MessagesStreamAccumulator::new();
        acc.begin_round();
        let mut out = Vec::new();
        out.extend(acc.push(&line(&json!({"type": "message_start", "message": {"id": "m"}}))));
        out.extend(acc.push(&line(
            &json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        )));
        out.extend(acc.push(&line(
            &json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
        )));
        out.extend(acc.push(&line(&json!({"type": "content_block_stop", "index": 0}))));
        out.extend(acc.push(&line(
            &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        )));
        out.extend(acc.push(&line(&json!({"type": "message_stop"}))));
        assert!(!acc.should_continue_loop(), "text-only round is terminal");
        out.extend(acc.finish());
        let s = out.join("");
        assert_eq!(s.matches("event: message_start").count(), 1);
        assert_eq!(s.matches("event: message_stop").count(), 1);
        assert!(s.contains("text_delta"));
        assert!(s.contains("end_turn"));
    }

    // A gateway tool round: the tool_use block (start/delta/stop) is suppressed,
    // its input reconstructed, thinking/text forwarded, and no terminal leaks.
    #[test]
    fn gateway_tool_round_suppresses_tool_use_and_reconstructs_call() {
        let mut acc = MessagesStreamAccumulator::new();
        acc.begin_round();
        let mut out = Vec::new();
        out.extend(acc.push(&line(&json!({"type": "message_start", "message": {"id": "m"}}))));
        // thinking idx0 (forward)
        out.extend(acc.push(&line(
            &json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        )));
        out.extend(acc.push(&line(&json!({"type": "content_block_stop", "index": 0}))));
        // gateway tool_use idx1 (suppress + reconstruct)
        out.extend(acc.push(&line(&json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "tid", "name": "web_search", "input": {}}}))));
        out.extend(acc.push(&line(&json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"query\":"}}))));
        out.extend(acc.push(&line(&json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"rust\"}"}}))));
        out.extend(acc.push(&line(&json!({"type": "content_block_stop", "index": 1}))));
        out.extend(acc.push(&line(
            &json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        )));
        out.extend(acc.push(&line(&json!({"type": "message_stop"}))));

        let s = out.join("");
        assert!(acc.should_continue_loop(), "pure gateway-tool round continues the loop");
        assert!(!s.contains("tool_use"), "gateway tool_use must not surface: {s}");
        assert!(!s.contains("message_stop"), "intermediate terminal suppressed");
        assert!(s.contains("thinking"), "thinking forwarded");
        let calls = acc.take_gateway_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].input_json, "{\"query\":\"rust\"}");
    }

    // Across two rounds, client-visible block indices stay contiguous (round 1
    // thinking=0, round 2 text=1) — no reset/collision.
    #[test]
    fn indices_are_contiguous_across_rounds() {
        let mut acc = MessagesStreamAccumulator::new();
        // round 1: thinking (idx0) + suppressed tool_use (idx1)
        acc.begin_round();
        acc.push(&line(&json!({"type": "message_start", "message": {"id": "m"}})));
        acc.push(&line(
            &json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking"}}),
        ));
        acc.push(&line(&json!({"type": "content_block_stop", "index": 0})));
        acc.push(&line(&json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "name": "web_search", "id": "t"}})));
        acc.push(&line(&json!({"type": "content_block_stop", "index": 1})));
        // round 2: text (upstream idx0) must map to client idx1
        acc.begin_round();
        let out = acc.push(&line(
            &json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ));
        let started: Value =
            serde_json::from_str(out[0].lines().nth(1).unwrap().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(started["index"], 1, "round-2 text rebased to contiguous client index 1");
    }

    // E7 (streaming): a round with a gateway tool_use AND a client-owned tool_use
    // is terminal — the loop must NOT continue (the client owns the second tool).
    // The client-owned tool_use is forwarded; the gateway one is suppressed.
    #[test]
    fn mixed_client_and_gateway_tool_use_stops_the_loop() {
        let mut acc = MessagesStreamAccumulator::new();
        acc.begin_round();
        acc.push(&line(&json!({"type": "message_start", "message": {"id": "m"}})));
        // gateway tool_use (idx0) — suppressed
        acc.push(&line(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "name": "web_search", "id": "g"}})));
        acc.push(&line(&json!({"type": "content_block_stop", "index": 0})));
        // client tool_use (idx1) — forwarded
        let out = acc.push(&line(&json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "name": "get_weather", "id": "c"}})));
        acc.push(&line(&json!({"type": "content_block_stop", "index": 1})));
        acc.push(&line(
            &json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        ));

        // Client tool_use surfaces; gateway one does not.
        let started: Value =
            serde_json::from_str(out[0].lines().nth(1).unwrap().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(
            started["content_block"]["name"], "get_weather",
            "client tool_use forwarded"
        );
        // The loop must terminate despite a gateway call being present.
        assert!(
            !acc.should_continue_loop(),
            "mixed round is terminal — loop must not continue"
        );
    }
}
