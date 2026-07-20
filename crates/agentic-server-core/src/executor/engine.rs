//! Stateful conversation executor.
//!
//! Exposes each step of the conversation pipeline as a public function so consumers
//! can compose them directly (e.g. as Praxis filters). [`ExecuteRequest`] is the
//! primary entry point; [`execute`] is a convenience shim for callers that don't
//! need per-request configuration.

use std::sync::Arc;

use async_stream::stream;
use either::Either;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::gateway::{
    LoopDecision, append_gateway_calls_to_new_input, append_output_items_to_input, append_tool_outputs, classify_round,
    execute_and_emit_output_calls, has_client_owned_calls, public_output_items,
};
use crate::executor::error::ExecutorResult;
use crate::executor::inference::DONE_MARKER;
use crate::executor::persist::persist_if_needed;
use crate::executor::rehydrate::rehydrate_conversation;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::upstream::{fetch_blocking_payload, fetch_stream_payload};
use crate::tool::ToolRegistry;
use crate::types::io::{OutputItem, ResponseUsage, ToolChoice};
use crate::types::request_response::{IncompleteDetails, RequestPayload, ResponsePayload};
use crate::utils::common::serialize_to_string;

pub use crate::executor::inference::BoxStream;

const MAX_GATEWAY_TOOL_ROUNDS: usize = 10;

fn add_usage(total: ResponseUsage, usage: ResponseUsage) -> ResponseUsage {
    ResponseUsage {
        input_tokens: total.input_tokens.saturating_add(usage.input_tokens),
        output_tokens: total.output_tokens.saturating_add(usage.output_tokens),
        total_tokens: total.total_tokens.saturating_add(usage.total_tokens),
        input_tokens_details: crate::types::io::InputTokenDetails {
            cached_tokens: total
                .input_tokens_details
                .cached_tokens
                .saturating_add(usage.input_tokens_details.cached_tokens),
        },
        output_tokens_details: crate::types::io::OutputTokenDetails {
            reasoning_tokens: total
                .output_tokens_details
                .reasoning_tokens
                .saturating_add(usage.output_tokens_details.reasoning_tokens),
        },
    }
}

fn accumulate_usage(total: &mut Option<ResponseUsage>, usage: Option<ResponseUsage>) {
    if let Some(usage) = usage {
        *total = Some(total.map_or(usage, |current| add_usage(current, usage)));
    }
}

fn error_sse_chunk(message: &str) -> String {
    let event = serde_json::json!({
        "type": "error",
        "error": {
            "message": message,
        },
    });
    let event_json = serialize_to_string(&event).unwrap_or_else(|_| "{\"error\":\"stream error\"}".to_owned());
    format!("data: {event_json}\n\n")
}

struct AbortOnDrop<T> {
    handle: tokio::task::JoinHandle<T>,
}

impl<T> AbortOnDrop<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self { handle }
    }
}

impl<T> std::ops::Deref for AbortOnDrop<T> {
    type Target = tokio::task::JoinHandle<T>;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl<T> std::ops::DerefMut for AbortOnDrop<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handle
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if !self.handle.is_finished() {
            self.handle.abort();
        }
    }
}

async fn run_until_gateway_tools_complete(
    mut ctx: RequestContext,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
    stream_upstream: bool,
    stream_events: Option<&mpsc::UnboundedSender<String>>,
) -> ExecutorResult<(ResponsePayload, RequestContext)> {
    let registry: ToolRegistry = match ctx.enriched_request.tools.as_ref() {
        Some(tools) => ToolRegistry::build_with_handlers(tools, &exec_ctx.gateway_executors).await?,
        None => ToolRegistry::default(),
    };
    let mut combined_output: Vec<crate::OutputItem> = Vec::new();
    let mut combined_usage: Option<ResponseUsage> = None;

    for round in 0..MAX_GATEWAY_TOOL_ROUNDS {
        let mut payload: ResponsePayload = if stream_upstream {
            fetch_stream_payload(&ctx, exec_ctx, auth, &registry, stream_events).await?
        } else {
            fetch_blocking_payload(&ctx, exec_ctx, auth).await?
        };
        registry.restore_final_payload_output(&mut payload.output);
        accumulate_usage(&mut combined_usage, payload.usage.take());
        let current_output = std::mem::take(&mut payload.output);
        for item in &current_output {
            if let OutputItem::CustomToolCall(call) = item {
                debug!(
                    response_id = %ctx.response_id,
                    call_id = %call.call_id,
                    name = %call.name,
                    input_bytes = call.input.len(),
                    "custom tool call requires client execution"
                );
            }
        }
        let has_client_owned = has_client_owned_calls(&current_output, &registry);
        let gateway_results =
            execute_and_emit_output_calls(&current_output, &registry, combined_output.len(), stream_events).await?;
        let public_output = public_output_items(&current_output, &registry, &gateway_results);
        combined_output.extend(public_output);

        match classify_round(has_client_owned, &gateway_results, round, MAX_GATEWAY_TOOL_ROUNDS) {
            // Client-owned calls (plain function or Codex namespace tools) are
            // handed back to the caller. Gateway calls in the same turn are
            // still recorded so the returned conversation is complete.
            LoopDecision::RequiresClientAction => {
                append_gateway_calls_to_new_input(&mut ctx, &current_output, &registry);
                append_tool_outputs(
                    &mut ctx,
                    gateway_results.into_iter().map(|result| result.input_item).collect(),
                );
                finalize_loop(&mut payload, combined_output, combined_usage, &ctx);
                return Ok((payload, ctx));
            }
            // No gateway work remains — this turn is the final response.
            LoopDecision::Done => {
                finalize_loop(&mut payload, combined_output, combined_usage, &ctx);
                return Ok((payload, ctx));
            }
            // Budget exhausted while the model was still requesting gateway
            // tools: surface the accumulated work as a partial
            // `status: "incomplete"` response instead of failing the request.
            // The final round's gateway calls and outputs are recorded so a
            // continuation is not fed a dangling tool call.
            LoopDecision::Incomplete(reason) => {
                append_gateway_calls_to_new_input(&mut ctx, &current_output, &registry);
                append_tool_outputs(
                    &mut ctx,
                    gateway_results.into_iter().map(|result| result.input_item).collect(),
                );
                finalize_loop(&mut payload, combined_output, combined_usage, &ctx);
                "incomplete".clone_into(&mut payload.status);
                payload.incomplete_details = Some(IncompleteDetails { reason: Some(reason) });
                return Ok((payload, ctx));
            }
            // Gateway tools ran and rounds remain; feed outputs back and loop.
            LoopDecision::Continue => {
                ctx.enriched_request.tool_choice = Some(ToolChoice::Auto);
                append_output_items_to_input(&mut ctx.enriched_request.input, &current_output);
                append_gateway_calls_to_new_input(&mut ctx, &current_output, &registry);
                append_tool_outputs(
                    &mut ctx,
                    gateway_results.into_iter().map(|result| result.input_item).collect(),
                );
            }
        }
    }

    unreachable!("the final round returns Done, RequiresClientAction, or Incomplete");
}

/// Move accumulated output/usage onto the terminating round's payload and
/// inject the response/conversation IDs. The payload's `model`/`created_at`/
/// `status` from the latest inference turn are preserved.
fn finalize_loop(
    payload: &mut ResponsePayload,
    combined_output: Vec<crate::types::io::OutputItem>,
    combined_usage: Option<ResponseUsage>,
    ctx: &RequestContext,
) {
    payload.output = combined_output;
    payload.usage = combined_usage;
    ctx.inject_ids(payload);
}

async fn run_blocking(
    ctx: RequestContext,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
) -> ExecutorResult<ResponsePayload> {
    let (payload, ctx) = run_until_gateway_tools_complete(ctx, exec_ctx, auth, false, None).await?;

    let ch = exec_ctx.conv_handler.clone();
    let rh = exec_ctx.resp_handler.clone();
    if let Err(e) = persist_if_needed(payload.clone(), ctx, ch, rh).await {
        warn!("persist failed: {e}");
    }

    Ok(payload)
}

fn run_stream(ctx: RequestContext, exec_ctx: Arc<ExecutionContext>, auth: Option<String>) -> BoxStream {
    Box::pin(stream! {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let exec_ctx_for_run = Arc::clone(&exec_ctx);
        let mut run_handle = AbortOnDrop::new(tokio::spawn(async move {
            run_until_gateway_tools_complete(
                ctx,
                exec_ctx_for_run.as_ref(),
                auth.as_deref(),
                true,
                Some(&event_tx),
            )
            .await
        }));

        loop {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    yield event;
                }
                result = &mut run_handle.handle => {
                    while let Ok(event) = event_rx.try_recv() {
                        yield event;
                    }
                    match result {
                        Err(e) => {
                            yield error_sse_chunk(&format!("stream task failed: {e}"));
                            yield DONE_MARKER.to_string();
                        }
                        Ok(Err(e)) => {
                            yield error_sse_chunk(&e.to_string());
                            yield DONE_MARKER.to_string();
                        }
                        Ok(Ok((payload, ctx))) => {
                            // Codex may close its WebSocket as soon as it receives
                            // `response.completed`. Persist before exposing that
                            // event so a custom call/output continuation cannot be
                            // cancelled by the client disconnect.
                            let terminal_event = payload.as_terminal_response_chunk();
                            let ch = exec_ctx.conv_handler.clone();
                            let rh = exec_ctx.resp_handler.clone();
                            if let Err(e) = persist_if_needed(payload, ctx, ch, rh).await {
                                warn!("persist failed: {e}");
                            }

                            yield terminal_event;
                            yield DONE_MARKER.to_string();
                        }
                    }
                    break;
                }
            }
        }
    })
}

/// Create a new conversation and return its data.
///
/// Exposes the conversation-creation step as a standalone function so callers
/// (e.g. `agentic-server`, Praxis filters, or tests) can pre-create a
/// conversation before submitting response turns.
///
/// # Errors
/// Returns [`ExecutorError`] if the conversation store is unavailable.
pub async fn create_conversation(exec_ctx: &ExecutionContext) -> ExecutorResult<crate::ConversationData> {
    exec_ctx.conv_handler.create().await
}

/// Builder for a stateful conversation turn.
///
/// ```ignore
/// ExecuteRequest::new(payload, exec_ctx).with_auth(token).run().await
/// ```
pub struct ExecuteRequest {
    payload: RequestPayload,
    exec_ctx: Arc<ExecutionContext>,
    client_auth: Option<String>,
}

impl ExecuteRequest {
    #[must_use]
    pub fn new(payload: RequestPayload, exec_ctx: Arc<ExecutionContext>) -> Self {
        Self {
            payload,
            exec_ctx,
            client_auth: None,
        }
    }

    /// Override the bearer token for this request only; does not touch the shared [`ExecutionContext`].
    #[must_use]
    pub fn with_auth(mut self, token: Option<String>) -> Self {
        self.client_auth = token;
        self
    }

    /// Execute one stateful conversation turn.
    ///
    /// Returns `Either::Left(ResponsePayload)` for non-streaming requests, or
    /// `Either::Right(BoxStream)` for streaming, each yielded `String` is an SSE
    /// line ready to forward to the client.
    ///
    /// # Errors
    /// Returns [`ExecutorError`] if rehydration or (non-streaming) LLM inference fails.
    pub async fn run(self) -> ExecutorResult<Either<ResponsePayload, BoxStream>> {
        debug!(
            model = %self.payload.model,
            store = self.payload.store,
            stream = self.payload.stream,
            has_previous_response_id = self.payload.previous_response_id.is_some(),
            has_conversation_id = self.payload.conversation_id.is_some(),
            tools = self.payload.tools.as_ref().map_or(0, Vec::len),
            "executor received responses request"
        );
        let ctx = rehydrate_conversation(self.payload, &self.exec_ctx).await?;
        if ctx.original_request.stream {
            Ok(Either::Right(run_stream(ctx, self.exec_ctx, self.client_auth)))
        } else {
            Ok(Either::Left(
                run_blocking(ctx, &self.exec_ctx, self.client_auth.as_deref()).await?,
            ))
        }
    }
}

/// Execute one stateful conversation turn.
///
/// Thin shim over [`ExecuteRequest`] for callers that don't need per-request auth override.
///
/// # Errors
/// Returns [`ExecutorError`] if rehydration or (non-streaming) LLM inference fails.
pub async fn execute(
    request: RequestPayload,
    exec_ctx: Arc<ExecutionContext>,
) -> ExecutorResult<Either<ResponsePayload, BoxStream>> {
    ExecuteRequest::new(request, exec_ctx).run().await
}
