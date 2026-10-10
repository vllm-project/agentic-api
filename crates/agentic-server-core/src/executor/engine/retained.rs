//! Streaming ownership for transports that retain delivery after public completion.
use super::run_until_gateway_tools_complete;
use super::streaming::StreamFailureContext;
use crate::executor::{
    error::{ExecutorError, ExecutorResult},
    gateway_accumulator::STREAM_EVENT_BUFFER,
    inference::BoxStream,
    multi_agent::RunControl,
    persist::persist_if_needed,
    pipeline::AgentPipeline,
    relay::{RelayLimits, StreamRelay},
    request::{ExecutionContext, RequestContext},
    response_events::{ResponseCommitState, ResponseEventSink},
    telemetry::ExecutionSpan,
};
use crate::tool::ToolSearchState;
use std::{num::NonZeroUsize, sync::Arc};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

/// A retained response's event stream, control handle, and execution owner.
/// Drain events independently of waiting for execution or control decisions.
#[must_use = "drain the response events and retain its owner until execution is joined"]
pub struct RunningResponse {
    pub response_id: String,
    pub control: Option<RunControl>,
    pub events: BoxStream,
    pub sink: ResponseEventSink,
    pub owner: ResponseRunOwner,
}

/// Drop requests cancellation; adapters explicitly cancel and join on disconnect.
pub struct ResponseRunOwner {
    task: Option<JoinHandle<ExecutorResult<()>>>,
    cancellation: CancellationToken,
    sink: ResponseEventSink,
}
impl ResponseRunOwner {
    /// Connection shutdown may cancel a live response while its create task owns the join.
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Request cancellation without detaching execution; still call [`Self::join`].
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
    /// # Errors
    /// Propagates execution, persistence, and task failure. Repeated calls after
    /// completion return an error. Cancelling this wait leaves the owner joinable.
    pub async fn join(&mut self) -> ExecutorResult<()> {
        let task = self
            .task
            .as_mut()
            .ok_or_else(|| ExecutorError::StreamError("response task has already been joined".into()))?;
        let outcome = task.await;
        self.task = None;
        let result = match outcome {
            Ok(result) => result,
            Err(error) => {
                self.sink.mark_aborted();
                return Err(ExecutorError::StreamError(format!("response task failed: {error}")));
            }
        };
        if result.is_err() && self.sink.commit_state() != ResponseCommitState::Aborted {
            self.sink.mark_failed();
        }
        result
    }
}
impl Drop for ResponseRunOwner {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(super) fn start(
    ctx: RequestContext,
    tools: Option<ToolSearchState>,
    exec: Arc<ExecutionContext>,
    auth: Option<String>,
    max_bytes: usize,
    mut execution: ExecutionSpan,
) -> RunningResponse {
    let response_id = ctx.response_id.clone();
    let failure_context = StreamFailureContext::from(&ctx);
    let (sender, receiver) = mpsc::channel(STREAM_EVENT_BUFFER);
    let sink = ResponseEventSink::new(sender, max_bytes);
    let relay = StreamRelay::response(sink.clone(), RelayLimits::with_event_bytes(max_bytes));
    let mut pipeline = AgentPipeline::new(ctx, tools, relay);
    let control = if pipeline
        .request
        .enriched_request
        .multi_agent
        .as_ref()
        .is_some_and(|config| config.enabled)
        && !pipeline.request.original_request.input.has_compaction_trigger()
    {
        let (control, receiver) = RunControl::channel(
            NonZeroUsize::new(exec.responses_config.max_retained_bytes.max(1)).expect("positive limit"),
        );
        pipeline.control = Some(receiver);
        Some(control)
    } else {
        None
    };
    let cancellation = pipeline.cancellation_token();
    let worker_cancel = cancellation.clone();
    let worker_sink = sink.clone();
    let span = execution.span().clone();
    let task = tokio::spawn(async move {
        let result = Box::pin(async {
            // The multi-agent coordinator handles cancellation by joining its children.
            let multi = pipeline.request.enriched_request.multi_agent.as_ref().is_some_and(|config| config.enabled);
            let run = Box::pin(run_until_gateway_tools_complete(&mut pipeline, &exec, auth.as_deref(), true));
            let (payload, metadata) = if multi { run.await? } else {
                tokio::select! {
                    result = run => result?,
                    () = worker_cancel.cancelled() => return Err(ExecutorError::StreamError("response cancelled".into())),
                }
            };
            worker_sink.validate_terminal(&payload)?;
            let status = payload.status.clone();
            let terminal = payload.clone();
            let (ctx, _) = pipeline.into_parts();
            tokio::select! {
                biased;
                () = worker_cancel.cancelled() => return Err(ExecutorError::StreamError("response cancelled before commit".into())),
                result = persist_if_needed(payload, ctx, metadata, exec.conv_handler.clone(), exec.resp_handler.clone()) => result?,
            }
            tokio::select! {
                () = worker_cancel.cancelled() => return Err(ExecutorError::StreamError("response delivery cancelled".into())),
                result = worker_sink.emit_terminal(&terminal) => result?,
            }
            execution.completed_with_status(&status);
            execution.delivered();
            Ok(())
        }).await;
        if let Err(error) = &result {
            worker_sink.mark_failed();
            if worker_cancel.is_cancelled() {
                execution.cancelled();
                execution.disconnected();
            } else {
                execution.failed(error);
            }
            tokio::select! {
                biased;
                () = worker_cancel.cancelled() => {},
                delivery = async {
                    if error.is_invalid_upstream_tool_search() {
                        worker_sink.emit_terminal(&failure_context.failed_payload(error)).await
                    } else {
                        worker_sink.emit_error(error).await
                    }
                } => {
                    if let Err(error) = delivery {
                        // The execution failure and delivery failure are distinct:
                        // no terminal frame reached the relay, so the adapter must
                        // close the transport rather than leave the client waiting.
                        worker_sink.mark_aborted();
                        tracing::debug!(error = ?error, "failed to deliver response failure");
                    }
                },
            }
        }
        result
    }.instrument(span));
    let events = ResponseEventSink::stream(receiver);
    let owner_sink = sink.clone();
    RunningResponse {
        response_id,
        control,
        events,
        sink,
        owner: ResponseRunOwner {
            task: Some(task),
            cancellation,
            sink: owner_sink,
        },
    }
}
