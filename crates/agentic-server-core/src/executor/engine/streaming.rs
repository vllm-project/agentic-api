//! Stream-owned execution lifecycle, terminal validation, and failure delivery.

mod producer;

use super::run_until_gateway_tools_complete;
use crate::executor::error::ExecutorError;
use crate::executor::gateway_accumulator::{GatewayStreamAccumulator, STREAM_EVENT_BUFFER, StreamEvent};
use crate::executor::inference::{BoxStream, DONE_MARKER};
use crate::executor::multi_agent::RunControlReceiver;
use crate::executor::persist::persist_if_needed;
use crate::executor::pipeline::AgentPipeline;
use crate::executor::relay::{RelayLimits, StreamRelay};
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::telemetry::{ExecutionSpan, InstrumentedStream};
use crate::tool::{ToolSearchMetadata, ToolSearchState};
use crate::types::request_response::ResponsePayload;
use crate::utils::common::utcnow_str;
use async_stream::stream;
use futures::StreamExt;
use producer::ProducerEvent;
use std::num::NonZeroU64;
use std::sync::Arc;
use tokio::sync::mpsc;

/// `max_stream_event_bytes` is the effective limit for one serialized client
/// event on the transport that will deliver this stream. The terminal
/// `response.completed` frame is validated against it before the response is
/// persisted or a session checkpoint is published, so a frame the transport
/// cannot deliver never leaves a stored response behind.
///
/// `execution` is the request's `agentic.execute` span guard. It is moved
/// into the stream so the span stays open, and is entered on every poll,
/// until the terminal frame has been yielded or the stream is dropped. The
/// stream owns and polls orchestration, so its stages run inside that span.
/// Dropping the stream drops orchestration. For a multi-agent response that
/// includes its `RunOwner`, which aborts round tasks without joining them.
pub(super) fn run_stream(
    ctx: RequestContext,
    tool_search_state: Option<ToolSearchState>,
    exec_ctx: Arc<ExecutionContext>,
    auth: Option<String>,
    max_stream_event_bytes: usize,
    mut execution: ExecutionSpan,
    control: Option<RunControlReceiver>,
) -> BoxStream {
    let span = execution.span().clone();
    let frames: BoxStream = Box::pin(stream! {
        let failure_context = StreamFailureContext::from(&ctx);
        let (event_tx, event_rx) = mpsc::channel(STREAM_EVENT_BUFFER);
        let exec_ctx_for_run = Arc::clone(&exec_ctx);
        let relay = StreamRelay::client(event_tx, RelayLimits::with_event_bytes(max_stream_event_bytes));
        let mut agent = AgentPipeline::new(ctx, tool_search_state, relay);
        agent.control = control;
        let run = async move {
            let result = run_until_gateway_tools_complete(
                &mut agent,
                exec_ctx_for_run.as_ref(),
                auth.as_deref(),
                true,
            )
            .await;
            let (ctx, stream_accumulator) = agent.into_parts();
            (result.map(|(payload, metadata)| (payload, ctx, metadata)), stream_accumulator)
        };
        let events = producer::drive(run, event_rx);
        futures::pin_mut!(events);

        let mut next_sequence_number = 0;
        while let Some(event) = events.next().await {
            match event {
                ProducerEvent::Event(event) => {
                    if let Some(content) = consume_stream_event(event, &mut next_sequence_number) {
                        yield content;
                    }
                }
                ProducerEvent::Finished(result) => {
                    match result {
                        Err(e) => {
                            // A caught producer panic; no payload is forwarded.
                            execution.failed(&e);
                            yield GatewayStreamAccumulator::executor_error_chunk_at(&e, next_sequence_number);
                            execution.delivered();
                            yield DONE_MARKER.to_string();
                        }
                        Ok((Err(e), mut stream_accumulator)) => {
                            execution.failed(&e);
                            yield failed_stream_chunk(&e, &failure_context, &mut stream_accumulator, next_sequence_number);
                            execution.delivered();
                            yield DONE_MARKER.to_string();
                        }
                        Ok((Ok((payload, ctx, tool_search_metadata)), stream_accumulator)) => {
                            let terminal = Box::pin(completed_stream_chunk(
                                payload,
                                ctx,
                                tool_search_metadata,
                                &stream_accumulator,
                                &exec_ctx,
                                next_sequence_number,
                                &mut execution,
                            ))
                            .await;
                            execution.delivered();
                            yield terminal;
                            yield DONE_MARKER.to_string();
                        }
                    }
                    break;
                }
            }
        }
    });
    Box::pin(InstrumentedStream::new(frames, span))
}

fn failed_stream_chunk(
    error: &ExecutorError,
    context: &StreamFailureContext,
    accumulator: &mut GatewayStreamAccumulator,
    next_sequence_number: u64,
) -> String {
    if error.is_invalid_upstream_tool_search() {
        accumulator
            .terminal_response_chunk(&context.failed_payload(error))
            .unwrap_or_else(|error| GatewayStreamAccumulator::executor_error_chunk_at(&error, next_sequence_number))
    } else {
        GatewayStreamAccumulator::executor_error_chunk_at(error, next_sequence_number)
    }
}

/// The terminal frame for an execution that produced a response: the
/// `response.completed` event, or an error frame when it cannot be
/// serialized or persisted.
///
/// Codex may close its WebSocket as soon as it receives `response.completed`,
/// so the response is persisted before that event is exposed and a custom
/// call/output continuation cannot be cancelled by the client disconnect.
async fn completed_stream_chunk(
    payload: ResponsePayload,
    ctx: RequestContext,
    tool_search_metadata: Option<ToolSearchMetadata>,
    stream_accumulator: &GatewayStreamAccumulator,
    exec_ctx: &ExecutionContext,
    next_sequence_number: u64,
    execution: &mut ExecutionSpan,
) -> String {
    let mut terminal_accumulator = stream_accumulator.clone();
    let chunk = match terminal_accumulator.terminal_response_chunk(&payload) {
        Ok(chunk) => chunk,
        Err(e) => {
            execution.failed(&e);
            return GatewayStreamAccumulator::executor_error_chunk_at(&e, next_sequence_number);
        }
    };
    let status = payload.status.clone();
    let ch = exec_ctx.conv_handler.clone();
    let rh = exec_ctx.resp_handler.clone();
    match persist_if_needed(payload, ctx, tool_search_metadata, ch, rh).await {
        Ok(()) => {
            execution.completed_with_status(&status);
            chunk
        }
        Err(e) => {
            execution.failed(&e);
            GatewayStreamAccumulator::executor_error_chunk_at(&e, next_sequence_number)
        }
    }
}

pub(super) struct StreamFailureContext {
    response_id: String,
    conversation_id: Option<String>,
    model: String,
    previous_response_id: Option<String>,
    instructions: Option<String>,
    max_tool_calls: Option<u64>,
}

impl From<&RequestContext> for StreamFailureContext {
    fn from(ctx: &RequestContext) -> Self {
        Self {
            response_id: ctx.response_id.clone(),
            conversation_id: ctx.conversation_id.clone(),
            model: ctx.enriched_request.model.clone(),
            previous_response_id: ctx.original_request.previous_response_id.clone(),
            instructions: ctx.original_request.instructions.clone(),
            max_tool_calls: ctx.max_tool_calls().map(NonZeroU64::get),
        }
    }
}

impl StreamFailureContext {
    pub(super) fn failed_payload(&self, error: &ExecutorError) -> ResponsePayload {
        ResponsePayload {
            id: self.response_id.clone(),
            object: "response".to_owned(),
            created_at: utcnow_str(),
            model: self.model.clone(),
            status: "failed".to_owned(),
            output: Vec::new(),
            usage: None,
            incomplete_details: None,
            error: Some(serde_json::json!({
                "message": error.error_message(),
                "type": error.error_type(),
                "code": error.error_code(),
            })),
            previous_response_id: self.previous_response_id.clone(),
            conversation_id: self.conversation_id.clone(),
            instructions: self.instructions.clone(),
            max_tool_calls: self.max_tool_calls,
            service_tier: None,
            tools: None,
            tool_choice: None,
        }
    }
}

pub(super) fn consume_stream_event(event: StreamEvent, next_sequence_number: &mut u64) -> Option<String> {
    match event {
        StreamEvent::Frame(frame) => {
            *next_sequence_number = frame.sequence_number.saturating_add(1);
            Some(frame.content)
        }
        StreamEvent::Flush(reply) => {
            // Flush markers do not advance the public sequence or emit a frame.
            let _ = reply.send(());
            None
        }
    }
}
