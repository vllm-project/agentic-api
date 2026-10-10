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
//!     suppressed; the final terminal is forwarded once, carrying every round's summed `usage`.
//!
//! Each `message_stop` ends its upstream round without waiting for HTTP EOF.
//!
//! Structurally the Anthropic-native analogue of the Responses `GatewayStreamAccumulator`
//! (#119/#132); kept deliberately parallel for a future consolidation. Reuses
//! only the neutral tool layer via [`crate::types::messages::tool_seam`].

mod blocks;
mod ingestion;
mod wire;
use crate::utils::common::serialize_to_value;
use blocks::{BufferedBlock, execute_gateway_calls};
use wire::{error_sse, executor_error_sse, sse};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use async_stream::stream;
use futures::StreamExt;
use serde_json::{Value, json};
use tracing::Instrument as _;

use crate::executor::error::ExecutorResult;
use crate::executor::inference::{BoxStream, response_lines, send_request};
use crate::executor::messages_context::MessagesRequestContext;
use crate::executor::messages_tools::request_gateway_map;
use crate::executor::messages_usage::MessagesUsageTotals;
use crate::executor::request::ExecutionContext;
use crate::executor::telemetry::{Api, ExecutionSpan, FailureCategory, InstrumentedStream, Route};
use crate::proxy::processed_response_headers;
use crate::tool::ToolRegistry;
use crate::types::messages::tool_seam;

// Shared with the non-streaming loop so the two Messages loops can't drift.
use crate::executor::messages_loop::{MAX_GATEWAY_TOOL_ROUNDS, MessagesResponse, MessagesUpstream};

/// Drive the streaming Messages-native loop, yielding Anthropic SSE lines for
/// the client. Owns the multi-round → single-message accumulation.
///
/// # Errors
///
/// Returns an executor error when the initial request cannot be serialized or
/// when the upstream rejects it before streaming begins.
pub async fn run_messages_stream(
    mut ctx: MessagesRequestContext,
    registry: Arc<ToolRegistry>,
    exec_ctx: Arc<ExecutionContext>,
    upstream: MessagesUpstream,
) -> ExecutorResult<MessagesResponse<BoxStream>> {
    ctx.force_stream(true);
    let mut execution = ExecutionSpan::start(Api::Messages, Route::Executor, true);
    let span = execution.span().clone();
    let first_round = span.in_scope(|| super::telemetry::stages::inference_round(0));
    let gateway_map = request_gateway_map(ctx.gateway_tools_or(&exec_ctx.messages_gateway_tools), &registry);
    let resumed = match super::messages_tools::resume_pending_mcp(&mut ctx, &registry, &gateway_map).await {
        Ok(results) => results,
        Err(error) => {
            execution.failed(&error);
            execution.not_delivered();
            return Err(error);
        }
    };
    let primed = send_messages_round(&ctx, &exec_ctx, &upstream)
        .instrument(first_round.clone())
        .await;
    let first_response = match primed {
        Ok(first_response) => first_response,
        Err(error) => {
            execution.failed(&error);
            execution.not_delivered();
            return Err(error);
        }
    };
    let response_headers = processed_response_headers(first_response.headers());
    let body = messages_stream_body(
        ctx,
        registry,
        exec_ctx,
        upstream,
        (first_response, first_round),
        execution,
        resumed,
    );
    Ok(MessagesResponse {
        body: Box::pin(InstrumentedStream::new(body, span)),
        headers: response_headers,
    })
}

/// Send one upstream round; the first runs before committing HTTP 200 so initial
/// vLLM errors retain their original status and body.
async fn send_messages_round(
    ctx: &MessagesRequestContext,
    exec_ctx: &ExecutionContext,
    upstream: &MessagesUpstream,
) -> ExecutorResult<reqwest::Response> {
    let first_body = ctx.upstream_body()?;
    send_request(
        &exec_ctx.client,
        upstream.url(),
        first_body,
        None,
        Some(upstream.headers()),
        exec_ctx.streaming_timeout,
    )
    .await
}

/// The client-facing frame stream. `execution` lives inside it: every exit
/// states the outcome before its last frame, and dropping the stream
/// finalizes it as cancelled.
fn messages_stream_body(
    mut ctx: MessagesRequestContext,
    registry: Arc<ToolRegistry>,
    exec_ctx: Arc<ExecutionContext>,
    upstream: MessagesUpstream,
    first_response: (reqwest::Response, tracing::Span),
    mut execution: ExecutionSpan,
    resumed: Vec<crate::types::messages::GatewayToolResult>,
) -> BoxStream {
    Box::pin(stream! {
        let mut acc = MessagesStreamAccumulator {
            gateway_map: request_gateway_map(ctx.gateway_tools_or(&exec_ctx.messages_gateway_tools), &registry),
            pending_mcp_results: resumed,
            ..Default::default()
        };
        let mut prepared_response = Some(first_response);

        for round in 0..MAX_GATEWAY_TOOL_ROUNDS {
            let (response, round_span) = if let Some(response) = prepared_response.take() {
                response
            } else {
                let round_span = super::telemetry::stages::inference_round(round);
                match send_messages_round(&ctx, &exec_ctx, &upstream)
                .instrument(round_span.clone())
                .await
                {
                    Ok(response) => (response, round_span),
                    Err(e) => {
                        execution.failed(&e);
                        execution.delivered();
                        yield executor_error_sse(&e);
                        return;
                    }
                }
            };
            let mut response_stream = InstrumentedStream::new(Box::pin(response_lines(
                response,
                exec_ctx.streaming_timeout,
                exec_ctx.responses_config.max_upstream_sse_line_bytes,
            )), round_span);

            acc.begin_round();
            while let Some(line) = response_stream.next().await {
                let line = match line {
                    Ok(l) => l,
                    Err(e) => {
                        execution.failed(&e);
                        execution.delivered();
                        yield error_sse(&e.to_string());
                        return;
                    }
                };
                for out in acc.push(&line) {
                    yield out;
                }
                if acc.has_error() {
                    execution.failed_with(acc.failure_category());
                    execution.delivered();
                    return;
                }
                if acc.has_completed_round() {
                    break;
                }
            }
            // Release an upstream body that can remain open after its terminal,
            // including before awaiting gateway tool execution for the next round.
            drop(response_stream);

            // Round finished. Continue only for a pure gateway-tool round; a
            // client-executed function tool makes the round terminal.
            let should_continue = acc.should_continue_loop(&ctx);
            if !should_continue {
                execution.completed_with_stop_reason(acc.stop_reason());
                let terminal = acc.finish();
                execution.delivered();
                for out in terminal {
                    yield out;
                }
                return;
            }
            // Reconstruct the FULL assistant turn (thinking/text/signature +
            // gateway tool_use, in order) for the next round's history — not just
            // the gateway tool_use (F3, streaming half). The gateway calls are
            // derived from the same buffered blocks for dispatch.
            let (assistant_content, calls) = acc.take_round();
            let tool_results = execute_gateway_calls(
                &calls,
                &mut ctx,
                &registry,
                &acc.gateway_map,
            ).await;
            match acc.emit_mcp_results(&assistant_content, &tool_results) {
                Ok(frames) => { for frame in frames { yield frame; } }
                Err(error) => {
                    execution.failed(&error);
                    execution.delivered();
                    yield executor_error_sse(&error);
                    return;
                }
            }
            if let Err(e) = ctx.append_round(&assistant_content, tool_results) {
                execution.failed(&e);
                execution.delivered();
                yield executor_error_sse(&e);
                return;
            }
        }

        // Round budget exhausted.
        execution.failed_with(FailureCategory::RoundBudget);
        execution.delivered();
        yield error_sse(&format!("gateway tool loop exceeded {MAX_GATEWAY_TOOL_ROUNDS} rounds"));
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RoundState {
    #[default]
    Active,
    Completed,
    Failed,
    UpstreamError,
}

/// State machine that turns per-round Anthropic SSE into one client-visible
/// message. Fed line-by-line via [`Self::push`].
#[derive(Default)]
struct MessagesStreamAccumulator {
    message_started: bool,
    pending_mcp_results: Vec<crate::types::messages::GatewayToolResult>,
    /// Whether the current upstream round has supplied its `message_start`.
    round_started: bool,
    /// Next client-visible block index (contiguous across rounds).
    next_index: u32,
    /// Map upstream (per-round) block index → client index, for the blocks we
    /// forward this round. Cleared each round.
    index_map: HashMap<u64, u32>,
    /// Upstream indices belonging to a suppressed gateway `tool_use` this round.
    suppressed_indices: HashSet<u64>,
    /// Tool-use IDs admitted in this upstream round, including client-executed tools.
    tool_ids: HashSet<String>,
    /// Every assistant block this round, keyed by upstream index (ordered), so
    /// the full turn — `thinking`/`text`/`signature` + gateway `tool_use` — can
    /// be reconstructed for the next round's history (F3). Cleared each round.
    blocks: BTreeMap<u64, BufferedBlock>,
    /// Did this round surface a client-owned `tool_use`? If so the loop cannot
    /// continue server-side (the client must supply that tool's result), so it
    /// is terminal — matching the non-streaming path's E7 handling.
    has_client_tool_use: bool,
    /// Buffered terminal `message_delta` from the final round (emitted by `finish`).
    final_message_delta: Option<Value>,
    /// Whether this round is active, complete, or terminated with an upstream error.
    round_state: RoundState,
    /// Every consumed round's terminal `usage`, reported once in the final `message_delta`.
    usage: MessagesUsageTotals,
    /// Operator-configured client-tool → gateway-executor aliases, so a client
    /// tool like Claude Code's `WebSearch` is classified gateway-owned (and
    /// suppressed) the same way the built-in `web_search` is.
    gateway_map: tool_seam::GatewayToolMap,
}

impl MessagesStreamAccumulator {
    fn emit_mcp_results(
        &mut self,
        content: &[Value],
        results: &[crate::types::messages::GatewayToolResult],
    ) -> ExecutorResult<Vec<String>> {
        let mut frames = Vec::new();
        for block in content {
            if self.gateway_map.public_mcp_call(block).is_some() {
                if let Some(result) = results.iter().find(|result| block["id"] == result.tool_use_id) {
                    let result = serialize_to_value(&crate::types::messages::mcp::McpContentBlock::result(result))?;
                    frames.extend(self.emit_block(&result));
                }
            }
        }
        Ok(frames)
    }

    fn emit_block(&mut self, block: &Value) -> Vec<String> {
        let index = self.next_index;
        self.next_index += 1;
        vec![
            sse(
                "content_block_start",
                &json!({"type":"content_block_start", "index":index, "content_block":block}),
            ),
            sse(
                "content_block_stop",
                &json!({"type":"content_block_stop", "index":index}),
            ),
        ]
    }

    fn begin_round(&mut self) {
        self.index_map.clear();
        self.suppressed_indices.clear();
        self.tool_ids.clear();
        self.blocks.clear();
        self.has_client_tool_use = false;
        self.round_state = RoundState::Active;
        self.round_started = false;
        // F6: clear the previous round's terminal so a clean-EOF round can't
        // re-emit a stale stop_reason.
        self.final_message_delta = None;
    }

    /// The loop should continue only when the round asked for a gateway tool AND
    /// did not also surface a client-owned tool (which the client must handle,
    /// making the round terminal — E7).
    fn should_continue_loop(&self, ctx: &MessagesRequestContext) -> bool {
        let stop_reason = self
            .final_message_delta
            .as_ref()
            .and_then(|event| event["delta"]["stop_reason"].as_str());
        self.has_completed_round()
            && self.gateway_call_count() > 0
            && !self.has_client_tool_use
            && ctx.is_tool_call_stop(
                stop_reason,
                self.blocks
                    .values()
                    .filter(|block| block.is_gateway_tool)
                    .filter_map(|block| block.block["name"].as_str()),
            )
    }

    fn has_error(&self) -> bool {
        matches!(self.round_state, RoundState::Failed | RoundState::UpstreamError)
    }

    fn failure_category(&self) -> FailureCategory {
        match self.round_state {
            RoundState::UpstreamError => FailureCategory::UpstreamError,
            RoundState::Active | RoundState::Completed | RoundState::Failed => FailureCategory::Stream,
        }
    }

    /// The round's terminal `stop_reason`, while the `message_delta` that
    /// carries it is still buffered (it is taken by [`Self::finish`]).
    fn stop_reason(&self) -> Option<&str> {
        self.final_message_delta
            .as_ref()
            .and_then(|event| event["delta"]["stop_reason"].as_str())
    }

    fn has_completed_round(&self) -> bool {
        self.round_state == RoundState::Completed
    }

    /// Emit completion only for an explicitly completed upstream round.
    fn finish(&mut self) -> Vec<String> {
        if !self.has_completed_round() {
            return vec![error_sse("upstream Messages stream ended before message_stop")];
        }
        let mut out = Vec::new();
        if let Some(mut delta) = self.final_message_delta.take() {
            // A completed client call requires client action even when vLLM labels it end_turn.
            if self.has_client_tool_use && delta["delta"]["stop_reason"] == "end_turn" {
                delta["delta"]["stop_reason"] = json!("tool_use");
            }
            self.usage.finish(&mut delta);
            out.push(sse("message_delta", &delta));
        }
        out.push(sse("message_stop", &json!({"type": "message_stop"})));
        out
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn line(v: &Value) -> String {
        format!("data: {v}")
    }

    /// Accumulator with the default gateway map (built-in `web_search` only).
    fn acc() -> MessagesStreamAccumulator {
        MessagesStreamAccumulator::default()
    }

    fn context() -> MessagesRequestContext {
        MessagesRequestContext::from_value(json!({"model":"test", "max_tokens":64, "messages":[]})).unwrap()
    }

    fn message_start(input_tokens: u64) -> Value {
        json!({"type":"message_start", "message":{"id":"m", "type":"message", "role":"assistant", "content":[],
            "model":"test", "stop_reason":null, "usage":{"input_tokens":input_tokens, "output_tokens":1}}})
    }

    /// A final round whose `message_delta` carries no `usage` still reports the
    /// hidden round's counters plus its own `message_start` snapshot.
    #[test]
    fn final_message_delta_without_usage_still_reports_hidden_rounds() {
        let mut acc = acc();
        acc.begin_round();
        acc.push(&line(&message_start(10)));
        acc.push(&line(
            &json!({"type":"content_block_start", "index":0, "content_block":{
                "type":"tool_use", "id":"search", "name":"web_search", "input":{}
            }}),
        ));
        acc.push(&line(&json!({"type":"content_block_stop", "index":0})));
        acc.push(&line(
            &json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}, "usage":{"output_tokens":4}}),
        ));
        acc.push(&line(&json!({"type":"message_stop"})));
        assert!(acc.should_continue_loop(&context()));
        acc.take_round();

        acc.begin_round();
        acc.push(&line(&message_start(20)));
        acc.push(&line(
            &json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}}),
        ));
        acc.push(&line(&json!({"type":"message_stop"})));
        assert_eq!(
            acc.finish(),
            vec![
                sse(
                    "message_delta",
                    &json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"},
                        "usage":{"input_tokens":30, "output_tokens":5}})
                ),
                sse("message_stop", &json!({"type":"message_stop"}))
            ]
        );
    }

    /// Part of #315: the suppressed gateway round's usage is summed into the final
    /// `message_delta`. Each round is `message_start.usage` overlaid by its deltas,
    /// so an upstream whose deltas carry only `output_tokens` still reports its
    /// input, a repeated cumulative delta counts once, and non-counter fields pass
    /// through from the last round.
    #[test]
    fn final_message_delta_reports_every_rounds_usage() {
        let mut acc = acc();
        acc.begin_round();
        acc.push(&line(&message_start(10)));
        acc.push(&line(
            &json!({"type":"content_block_start", "index":0, "content_block":{
                "type":"tool_use", "id":"search", "name":"web_search", "input":{}
            }}),
        ));
        acc.push(&line(&json!({"type":"content_block_delta", "index":0, "delta":{
            "type":"input_json_delta", "partial_json":"{\"query\":\"rust\"}"
        }})));
        acc.push(&line(&json!({"type":"content_block_stop", "index":0})));
        acc.push(&line(
            &json!({"type":"message_delta", "delta":{"stop_reason":null}, "usage":{"output_tokens":1}}),
        ));
        acc.push(&line(
            &json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"},
                "usage":{"output_tokens":4, "cache_read_input_tokens":128}}),
        ));
        acc.push(&line(&json!({"type":"message_stop"})));
        assert!(acc.should_continue_loop(&context()));
        let (_, calls) = acc.take_round();
        assert_eq!(calls.len(), 1);

        acc.begin_round();
        acc.push(&line(&message_start(20)));
        acc.push(&line(
            &json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}),
        ));
        acc.push(&line(&json!({"type":"content_block_stop", "index":0})));
        acc.push(&line(
            &json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"},
                "usage":{"output_tokens":6, "extension":{"value":1}}}),
        ));
        acc.push(&line(&json!({"type":"message_stop"})));
        assert!(!acc.should_continue_loop(&context()));
        assert_eq!(
            acc.finish(),
            vec![
                sse(
                    "message_delta",
                    &json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{
                        "output_tokens":10, "extension":{"value":1}, "input_tokens":30, "cache_read_input_tokens":128
                    }})
                ),
                sse("message_stop", &json!({"type":"message_stop"}))
            ]
        );
    }

    #[test]
    fn client_terminal_normalization_preserves_metadata_and_requires_completion() {
        for completed in [false, true] {
            for reason in ["end_turn", "tool_use", "max_tokens", "stop_sequence", "future"] {
                let mut acc = acc();
                acc.push(&line(&message_start(0)));
                acc.push(&line(
                    &json!({"type":"content_block_start", "index":0, "content_block":{
                        "type":"tool_use", "id":"client", "name":"client_echo", "input":{}
                    }}),
                ));
                acc.push(&line(&json!({"type":"content_block_stop", "index":0})));
                let mut terminal = json!({"type":"message_delta", "delta":{
                    "stop_reason":reason, "stop_sequence":null, "extension":{"value":1}
                }, "usage":{"output_tokens":7}, "provider_extension":[1,2]});
                acc.push(&line(&terminal));
                if completed {
                    acc.push(&line(&json!({"type":"message_stop"})));
                }
                assert!(!acc.should_continue_loop(&context()));
                if !completed {
                    assert_eq!(
                        acc.finish(),
                        vec![error_sse("upstream Messages stream ended before message_stop")]
                    );
                    continue;
                }
                if reason == "end_turn" {
                    terminal["delta"]["stop_reason"] = json!("tool_use");
                }
                assert_eq!(
                    acc.finish(),
                    vec![
                        sse("message_delta", &terminal),
                        sse("message_stop", &json!({"type":"message_stop"}))
                    ]
                );
            }
        }
    }

    #[test]
    fn named_end_turn_requires_completion_matching_gateway_and_no_client_tool() {
        for (name, stop, completed, client_tool, expected) in [
            ("web_search", "end_turn", true, false, true),
            ("web_search", "end_turn", false, false, false),
            ("client_echo", "end_turn", true, false, false),
            ("web_search", "max_tokens", true, false, false),
            ("web_search", "stop_sequence", true, false, false),
            ("web_search", "end_turn", true, true, false),
        ] {
            let ctx = MessagesRequestContext::from_value(json!({
                "model":"test", "max_tokens":64, "messages":[], "tool_choice":{"type":"tool", "name":name}
            }))
            .unwrap();
            let mut acc = acc();
            acc.push(&line(&message_start(0)));
            acc.push(&line(
                &json!({"type":"content_block_start", "index":0, "content_block":{
                    "type":"tool_use", "id":"search", "name":"web_search", "input":{"query":"proof"}
                }}),
            ));
            acc.push(&line(&json!({"type":"content_block_stop", "index":0})));
            if client_tool {
                acc.push(&line(
                    &json!({"type":"content_block_start", "index":1, "content_block":{
                        "type":"tool_use", "id":"client", "name":"client_echo", "input":{}
                    }}),
                ));
                acc.push(&line(&json!({"type":"content_block_stop", "index":1})));
            }
            acc.push(&line(&json!({"type":"message_delta", "delta":{"stop_reason":stop}})));
            if completed {
                acc.push(&line(&json!({"type":"message_stop"})));
            }
            assert_eq!(
                acc.should_continue_loop(&ctx),
                expected,
                "{name}, {stop}, {completed}, {client_tool}"
            );
        }
    }

    #[tokio::test]
    async fn messages_bridge_accepts_optional_data_space() {
        for prefix in ["data:", "data: "] {
            let mut acc = acc();
            acc.begin_round();
            let events = [
                json!({"type": "message_start", "message": {"id": "m"}}),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hello"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
                json!({"type": "message_stop"}),
            ];
            let mut body = String::new();
            for event in &events {
                write!(body, "{prefix}{event}\n\n").expect("write SSE event");
            }
            write!(body, "{prefix}[DONE]\n\n").expect("write SSE termination marker");
            let response = reqwest::Response::from(http::Response::new(body));
            let mut lines = Box::pin(response_lines(
                response,
                std::time::Duration::ZERO,
                crate::config::DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES,
            ));
            let mut output = Vec::new();
            while let Some(line) = lines.next().await {
                output.extend(acc.push(&line.expect("valid SSE transport")));
            }
            output.extend(acc.finish());

            let output = output.join("");
            assert_eq!(output.matches("event: message_start").count(), 1, "{prefix:?}");
            assert_eq!(output.matches("event: content_block_delta").count(), 1, "{prefix:?}");
            assert_eq!(output.matches("event: message_stop").count(), 1, "{prefix:?}");
            assert!(output.contains(r#""text":"hello""#), "{prefix:?}");
            assert!(output.contains("end_turn"), "{prefix:?}");
        }
    }

    // A single non-tool round: message_start forwarded once, blocks pass through
    // with contiguous indices, terminal emitted by finish().
    #[test]
    fn single_round_text_passes_through() {
        let mut acc = acc();
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
        assert!(!acc.should_continue_loop(&context()), "text-only round is terminal");
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
        let mut acc = acc();
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
        assert!(
            acc.should_continue_loop(&context()),
            "pure gateway-tool round continues the loop"
        );
        assert!(!s.contains("tool_use"), "gateway tool_use must not surface: {s}");
        assert!(!s.contains("message_stop"), "intermediate terminal suppressed");
        assert!(s.contains("thinking"), "thinking forwarded");
        let (_assistant, calls) = acc.take_round();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].input_json, "{\"query\":\"rust\"}");
    }

    // Across two rounds, client-visible block indices stay contiguous (round 1
    // thinking=0, round 2 text=1) — no reset/collision.
    #[test]
    fn indices_are_contiguous_across_rounds() {
        let mut acc = acc();
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
        acc.push(&line(&message_start(0)));
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
        let mut acc = acc();
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
            !acc.should_continue_loop(&context()),
            "mixed round is terminal — loop must not continue"
        );
    }

    // F6 (repro): begin_round() must reset final_message_delta. Round 1 ends on a
    // tool_use terminal; round 2 ends WITHOUT a message_delta (clean EOF). finish()
    // must NOT emit round 1's stale stop_reason: tool_use.
    #[test]
    fn repro_f6_begin_round_resets_stale_terminal() {
        let mut acc = acc();
        // Round 1: a gateway tool round → sets final_message_delta = tool_use terminal.
        acc.begin_round();
        acc.push(&line(&json!({"type": "message_start", "message": {"id": "m"}})));
        acc.push(&line(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "name": "web_search", "id": "t"}})));
        acc.push(&line(&json!({"type": "content_block_stop", "index": 0})));
        acc.push(&line(
            &json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        ));
        // Round 2: text, but upstream ends with NO message_delta (cut short).
        acc.begin_round();
        acc.push(&line(&message_start(0)));
        acc.push(&line(
            &json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ));
        acc.push(&line(
            &json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
        ));
        acc.push(&line(&json!({"type": "content_block_stop", "index": 0})));
        let out = acc.finish().join("");
        assert!(out.contains("event: error"), "incomplete later round must fail: {out}");
        assert!(!out.contains("event: message_stop"), "no synthetic completion: {out}");
        assert!(
            !out.contains(r#""stop_reason":"tool_use""#),
            "must not emit round 1's stale tool_use terminal: {out}"
        );
    }

    // F3 (repro): the assistant turn fed into the next round's history must
    // preserve the model's thinking/text/signature blocks in order, not just the
    // gateway tool_use. (This is the streaming half of Maral's F3 — "also repeated
    // in messages_stream.rs".)
    #[test]
    fn repro_f3_stream_history_preserves_thinking_text_and_signature() {
        let mut acc = acc();
        acc.begin_round();
        acc.push(&line(&json!({"type": "message_start", "message": {"id": "m"}})));
        // thinking idx0 (with a signature delta)
        acc.push(&line(
            &json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        ));
        acc.push(&line(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "let me search"}})));
        acc.push(&line(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "SIG=="}})));
        acc.push(&line(&json!({"type": "content_block_stop", "index": 0})));
        // text idx1
        acc.push(&line(
            &json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        ));
        acc.push(&line(&json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Searching..."}})));
        acc.push(&line(&json!({"type": "content_block_stop", "index": 1})));
        // gateway tool_use idx2 (suppressed from client, but must appear in history)
        acc.push(&line(&json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "tid", "name": "web_search", "input": {}}})));
        acc.push(&line(&json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"query\":\"rust\"}"}})));
        acc.push(&line(&json!({"type": "content_block_stop", "index": 2})));
        acc.push(&line(
            &json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        ));

        let (assistant, _calls) = acc.take_round();
        let types: Vec<&str> = assistant.iter().filter_map(|b| b["type"].as_str()).collect();
        assert_eq!(
            types,
            vec!["thinking", "text", "tool_use"],
            "full assistant turn preserved in order, not just the gateway tool_use: {assistant:?}"
        );
        assert_eq!(assistant[0]["thinking"], "let me search", "thinking text reconstructed");
        assert_eq!(
            assistant[0]["signature"], "SIG==",
            "signature preserved for the next round"
        );
        assert_eq!(assistant[1]["text"], "Searching...", "text reconstructed");
        assert_eq!(
            assistant[2]["input"]["query"], "rust",
            "gateway call input reconstructed"
        );
    }

    // F4 (repro): a malformed/incomplete input_json for a gateway call must NOT
    // silently become `{}` and dispatch the tool with args the model never sent.
    #[tokio::test]
    async fn repro_f4_malformed_partial_json_is_not_dispatched_with_empty_args() {
        let mut acc = acc();
        acc.begin_round();
        acc.push(&line(&json!({"type": "message_start", "message": {"id": "m"}})));
        acc.push(&line(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "name": "web_search", "id": "t"}})));
        // Incomplete partial_json (stream cut mid-arguments).
        acc.push(&line(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"query\":"}})));
        let (_assistant, calls) = acc.take_round();
        assert_eq!(calls.len(), 1);
        // The reconstructed input is invalid JSON.
        assert!(
            serde_json::from_str::<serde_json::Value>(&calls[0].input_json).is_err(),
            "incomplete partial_json is invalid JSON"
        );
        // After the fix, execute_gateway_calls must NOT coerce invalid input to
        // {} and dispatch — it must produce an error tool_result. Assert the
        // reconstructed call is flagged invalid rather than silently dispatchable.
        let resolved = execute_gateway_calls(
            &calls,
            &mut context(),
            &no_op_registry().await,
            &tool_seam::GatewayToolMap::default(),
        )
        .await;
        let content = &resolved[0].content;
        assert!(
            content.contains("invalid") || content.contains("malformed") || content.contains("could not"),
            "malformed args must yield an error tool_result, not an empty-arg dispatch: {content:?}"
        );
    }

    /// Registry with no gateway executors — dispatch of any call returns None, so
    /// the ONLY way `execute_gateway_calls` can produce a non-"no handler" result
    /// for a malformed input is by rejecting the args before dispatch (the fix).
    async fn no_op_registry() -> ToolRegistry {
        let mut tools: [crate::tool::ToolDeclaration; 0] = [];
        let mut executors = crate::tool::GatewayExecutors::from_env(std::sync::Arc::new(reqwest::Client::new()));
        ToolRegistry::build_with_handlers(&mut tools, &mut executors)
            .await
            .unwrap()
    }
}
