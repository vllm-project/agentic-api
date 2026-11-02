//! Native server tool calls in the client's stream.
//!
//! A native call is shown only when its round runs it, and the round's
//! outcome is known only at its terminal. So once a native call starts, it and
//! every later block of the round are held; the round's end releases them in
//! order, each native call as a `server_tool_use` block (start, one
//! `input_json_delta`, stop) when the round runs its calls, or hidden like any
//! unexecuted gateway call when it does not. The result blocks follow the
//! round's last block, each complete in one `content_block_start`. A released
//! block reaches the client through the same `forward` as a live one.

use serde_json::{Value, json};

use super::ingestion::Frame;
use super::{MessagesRoundDecision, MessagesStreamAccumulator, sse};
use crate::executor::messages_tools::ExecutedCall;
use crate::types::messages::{GatewayToolResult, ServerToolUseBlock};

impl MessagesStreamAccumulator {
    /// Release the blocks held since the round's first native call, showing
    /// the native calls when `show_native_calls` (the round runs them).
    pub(super) fn release_held(&mut self, show_native_calls: bool) -> Vec<Frame> {
        let held = std::mem::take(&mut self.held);
        let mut frames = Vec::with_capacity(held.len());
        for event in held {
            let up_index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
            if self.suppressed_indices.contains(&up_index) {
                // Only a native call's start is held, to keep its place.
                let call = self
                    .blocks
                    .get(&up_index)
                    .and_then(|block| self.native.server_tool_use(&block.completed_block()));
                if show_native_calls && let Some(call) = call {
                    let index = self.next_client_index();
                    frames.extend(server_tool_use_frames(index, call));
                }
                continue;
            }
            frames.extend(self.forward(event));
        }
        frames
    }

    /// The frames that end a round whose calls did not run: the held blocks,
    /// with the native calls hidden like any unexecuted gateway call, then the
    /// message's terminal.
    pub(super) fn terminal_frames(&mut self, decision: MessagesRoundDecision) -> Vec<Frame> {
        let mut frames = self.release_held(false);
        frames.extend(self.finish(decision).into_iter().map(|frame| (frame, false)));
        frames
    }

    /// The result blocks of the round's executed native calls, in call order,
    /// and every call's `tool_result` for the next round; each call's server
    /// tool uses join the turn's usage.
    pub(super) fn record_executed(&mut self, executed: Vec<ExecutedCall>) -> (Vec<Frame>, Vec<GatewayToolResult>) {
        let mut frames = Vec::new();
        let mut tool_results = Vec::with_capacity(executed.len());
        for call in executed {
            self.usage.add_server_tool_uses(call.uses);
            if let Some(result) = call.server_result {
                let index = self.next_client_index();
                let start = json!({"type": "content_block_start", "index": index, "content_block": result});
                frames.push((sse("content_block_start", &start), false));
                frames.push((stop_frame(index), false));
            }
            tool_results.push(call.tool_result);
        }
        (frames, tool_results)
    }
}

fn server_tool_use_frames(index: u32, call: ServerToolUseBlock) -> [Frame; 3] {
    let input = serde_json::to_string(&call.input).unwrap_or_else(|_| "{}".to_owned());
    let skeleton = ServerToolUseBlock::new(call.id, call.name, json!({}));
    let start = json!({"type": "content_block_start", "index": index, "content_block": skeleton});
    let delta = json!({
        "type": "content_block_delta", "index": index,
        "delta": {"type": "input_json_delta", "partial_json": input}
    });
    [
        (sse("content_block_start", &start), false),
        (sse("content_block_delta", &delta), false),
        (stop_frame(index), false),
    ]
}

fn stop_frame(index: u32) -> String {
    sse(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": index}),
    )
}
