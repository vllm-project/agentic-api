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
use tracing::debug;

use super::compaction::maybe_compact_context;
use super::gateway::{
    GatewayCallResult, LoopDecision, append_gateway_calls_to_new_input, append_output_items_to_input,
    append_tool_outputs, classify_round, emit_gateway_completed_events, emit_gateway_start_events,
    execute_and_emit_output_calls, execute_output_calls, gateway_event_plans, has_client_owned_calls,
    is_gateway_owned_call, public_output_items,
};
use super::gateway_accumulator::{GatewayStreamAccumulator, StreamEvent, error_sse_chunk};
use crate::events::EventFrame;
use crate::executor::error::ExecutorResult;
use crate::executor::inference::DONE_MARKER;
use crate::executor::persist::persist_if_needed;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::upstream::{emit_deferred_stream_events, fetch_blocking_payload, fetch_stream_payload};
use crate::tool::ToolRegistry;
use crate::types::io::{OutputItem, ResponseUsage, ToolChoice};
use crate::types::request_response::{IncompleteDetails, RequestPayload, ResponsePayload};

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
    mut stream: Option<(&mut GatewayStreamAccumulator, &mpsc::UnboundedSender<StreamEvent>)>,
) -> ExecutorResult<(ResponsePayload, RequestContext)> {
    let mut executors = exec_ctx.gateway_executors.request_scoped();
    let registry: ToolRegistry = match ctx.enriched_request.tools.as_mut() {
        Some(tools) => ToolRegistry::build_with_handlers(tools, &mut executors).await?,
        None => ToolRegistry::default(),
    };
    let mut combined_output: Vec<crate::OutputItem> = Vec::new();
    let mut combined_usage = None;

    for round in 0..MAX_GATEWAY_TOOL_ROUNDS {
        let compaction_usage = maybe_compact_context(&mut ctx, exec_ctx, auth).await?;
        accumulate_usage(&mut combined_usage, compaction_usage);
        let output_offset = combined_output.len();
        let (mut payload, deferred_stream_events): (ResponsePayload, Vec<_>) = if stream_upstream {
            let stream_payload = fetch_stream_payload(
                &ctx,
                exec_ctx,
                auth,
                &registry,
                stream
                    .as_mut()
                    .map(|(accumulator, sender)| (&mut **accumulator, *sender)),
                output_offset,
            )
            .await?;
            (stream_payload.payload, stream_payload.deferred_events)
        } else {
            (fetch_blocking_payload(&ctx, exec_ctx, auth).await?, Vec::new())
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
        let gateway_results = execute_and_emit_round_output_calls(
            &current_output,
            &registry,
            output_offset,
            deferred_stream_events,
            &ctx,
            stream
                .as_mut()
                .map(|(accumulator, sender)| (&mut **accumulator, *sender)),
        )
        .await?;
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

async fn execute_and_emit_round_output_calls(
    output_items: &[OutputItem],
    registry: &ToolRegistry,
    output_offset: usize,
    deferred_events: Vec<EventFrame>,
    ctx: &RequestContext,
    stream: Option<(&mut GatewayStreamAccumulator, &mpsc::UnboundedSender<StreamEvent>)>,
) -> ExecutorResult<Vec<GatewayCallResult>> {
    match (deferred_events.is_empty(), stream) {
        (true, stream) => execute_and_emit_output_calls(output_items, registry, output_offset, stream).await,
        (false, Some((stream_accumulator, stream_sender))) => {
            execute_and_emit_ordered_output_calls(
                output_items,
                registry,
                output_offset,
                deferred_events,
                ctx,
                stream_accumulator,
                stream_sender,
            )
            .await
        }
        (false, None) => execute_and_emit_output_calls(output_items, registry, output_offset, None).await,
    }
}

async fn execute_and_emit_ordered_output_calls(
    output_items: &[OutputItem],
    registry: &ToolRegistry,
    output_offset: usize,
    deferred_events: Vec<EventFrame>,
    ctx: &RequestContext,
    stream_accumulator: &mut GatewayStreamAccumulator,
    stream_sender: &mpsc::UnboundedSender<StreamEvent>,
) -> ExecutorResult<Vec<GatewayCallResult>> {
    let mut events_by_output = Vec::with_capacity(output_items.len());
    events_by_output.resize_with(output_items.len(), Vec::new);
    let mut remaining_events = Vec::new();
    for frame in deferred_events {
        let Some(output_index) = frame
            .wire
            .output_index
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| *index < events_by_output.len())
        else {
            remaining_events.push(frame);
            continue;
        };
        events_by_output[output_index].push(frame);
    }

    let event_plans = gateway_event_plans(output_items, registry, output_offset);
    let first_gateway_index = output_items
        .iter()
        .position(|item| matches!(item, OutputItem::FunctionCall(call) if is_gateway_owned_call(call, registry)));
    let first_gateway_run_end = first_gateway_index.map_or(0, |start| {
        output_items[start..]
            .iter()
            .take_while(|item| matches!(item, OutputItem::FunctionCall(call) if is_gateway_owned_call(call, registry)))
            .count()
            .saturating_add(start)
    });
    let first_gateway_run_len = first_gateway_run_end.saturating_sub(first_gateway_index.unwrap_or(0));
    emit_gateway_start_events(&event_plans[..first_gateway_run_len], stream_accumulator, stream_sender)?;

    let gateway_results = execute_output_calls(output_items, registry).await?;
    let mut gateway_index = 0;
    for (index, item) in output_items.iter().enumerate() {
        if matches!(item, OutputItem::FunctionCall(call) if is_gateway_owned_call(call, registry)) {
            let plan = &event_plans[gateway_index..=gateway_index];
            let result = &gateway_results[gateway_index..=gateway_index];
            if index >= first_gateway_run_end {
                emit_gateway_start_events(plan, stream_accumulator, stream_sender)?;
            }
            emit_gateway_completed_events(result, plan, stream_accumulator, stream_sender)?;
            emit_deferred_stream_events(
                std::mem::take(&mut events_by_output[index]),
                ctx,
                registry,
                stream_accumulator,
                stream_sender,
                output_offset,
            )?;
            gateway_index += 1;
        } else {
            emit_deferred_stream_events(
                std::mem::take(&mut events_by_output[index]),
                ctx,
                registry,
                stream_accumulator,
                stream_sender,
                output_offset,
            )?;
        }
    }
    emit_deferred_stream_events(
        remaining_events,
        ctx,
        registry,
        stream_accumulator,
        stream_sender,
        output_offset,
    )?;
    Ok(gateway_results)
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
    persist_if_needed(payload.clone(), ctx, ch, rh).await?;

    Ok(payload)
}

fn run_stream(ctx: RequestContext, exec_ctx: Arc<ExecutionContext>, auth: Option<String>) -> BoxStream {
    Box::pin(stream! {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let exec_ctx_for_run = Arc::clone(&exec_ctx);
        let event_tx_for_run = event_tx.clone();
        let stream_accumulator = GatewayStreamAccumulator::new();
        let mut run_handle = AbortOnDrop::new(tokio::spawn(async move {
            let mut stream_accumulator = stream_accumulator;
            let result = run_until_gateway_tools_complete(
                ctx,
                exec_ctx_for_run.as_ref(),
                auth.as_deref(),
                true,
                Some((&mut stream_accumulator, &event_tx_for_run)),
            )
            .await;
            (result, stream_accumulator)
        }));

        let mut next_sequence_number = 0;
        loop {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    yield consume_stream_event(event, &mut next_sequence_number);
                }
                result = &mut run_handle.handle => {
                    match result {
                        Err(e) => {
                            for chunk in panicked_stream_chunks(&e, &mut event_rx, &mut next_sequence_number) {
                                yield chunk;
                            }
                        }
                        Ok((Err(e), mut stream_accumulator)) => {
                            while let Ok(event) = event_rx.try_recv() {
                                yield consume_stream_event(event, &mut next_sequence_number);
                            }
                            yield stream_accumulator.executor_error_chunk(&e);
                            yield DONE_MARKER.to_string();
                        }
                        Ok((Ok((payload, ctx)), mut stream_accumulator)) => {
                            while let Ok(event) = event_rx.try_recv() {
                                yield consume_stream_event(event, &mut next_sequence_number);
                            }
                            // Codex may close its WebSocket as soon as it receives
                            // `response.completed`. Persist before exposing that
                            // event so a custom call/output continuation cannot be
                            // cancelled by the client disconnect.
                            let ch = exec_ctx.conv_handler.clone();
                            let rh = exec_ctx.resp_handler.clone();
                            let mut terminal_accumulator = stream_accumulator.clone();
                            let terminal_chunk = terminal_accumulator.terminal_response_chunk(&payload);
                            match persist_if_needed(payload, ctx, ch, rh).await {
                                Ok(()) => match terminal_chunk {
                                    Ok(chunk) => yield chunk,
                                    Err(e) => yield stream_accumulator.executor_error_chunk(&e),
                                },
                                Err(e) => yield stream_accumulator.executor_error_chunk(&e),
                            }
                            yield DONE_MARKER.to_string();
                        }
                    }
                    break;
                }
            }
        }
    })
}

fn consume_stream_event(event: StreamEvent, next_sequence_number: &mut u64) -> String {
    *next_sequence_number = event.sequence_number.saturating_add(1);
    event.content
}

fn stream_task_failure_chunk(error: &tokio::task::JoinError, sequence_number: u64) -> String {
    error_sse_chunk(&format!("stream task failed: {error}"), sequence_number)
}

fn panicked_stream_chunks(
    error: &tokio::task::JoinError,
    event_rx: &mut mpsc::UnboundedReceiver<StreamEvent>,
    next_sequence_number: &mut u64,
) -> Vec<String> {
    let mut chunks = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        chunks.push(consume_stream_event(event, next_sequence_number));
    }
    chunks.push(stream_task_failure_chunk(error, *next_sequence_number));
    chunks.push(DONE_MARKER.to_owned());
    chunks
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
    tenant_id: Option<String>,
}

impl ExecuteRequest {
    #[must_use]
    pub fn new(payload: RequestPayload, exec_ctx: Arc<ExecutionContext>) -> Self {
        Self {
            payload,
            exec_ctx,
            client_auth: None,
            tenant_id: None,
        }
    }

    /// Override the bearer token for this request only; does not touch the shared [`ExecutionContext`].
    #[must_use]
    pub fn with_auth(mut self, token: Option<String>) -> Self {
        self.client_auth = token;
        self
    }

    #[must_use]
    pub fn with_tenant_id(mut self, tenant_id: Option<String>) -> Self {
        self.tenant_id = tenant_id;
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
        let ctx =
            super::rehydrate::rehydrate_conversation_for_tenant(self.payload, &self.exec_ctx, self.tenant_id).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stream_task_panic_after_event_uses_next_sequence_number_for_error() {
        let accumulator = GatewayStreamAccumulator::new();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut accumulator = accumulator;
            let event = accumulator
                .process_sse_line(r#"data: {"type":"response.created"}"#, 0)
                .expect("event should be emitted");
            event_tx
                .send(StreamEvent {
                    content: "event".to_owned(),
                    sequence_number: event.sequence_number().expect("event should be numbered"),
                })
                .expect("test receiver should remain open");
            panic!("test task panic");
        });

        let error = task.await.expect_err("task should panic");
        let mut next_sequence_number = 0;
        let chunks = panicked_stream_chunks(&error, &mut event_rx, &mut next_sequence_number);
        let error_event: serde_json::Value = serde_json::from_str(
            chunks[1]
                .trim_end_matches('\n')
                .strip_prefix("data: ")
                .expect("SSE data prefix"),
        )
        .expect("error chunk should be valid JSON");

        assert_eq!(chunks[0], "event");
        assert_eq!(error_event["sequence_number"], 1);
        assert_eq!(chunks[2], DONE_MARKER);
    }
}
