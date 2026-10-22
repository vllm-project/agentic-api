//! Streaming ownership for transports that retain delivery after public completion.
use super::run_until_gateway_tools_complete;
use super::streaming::StreamFailureContext;
use crate::executor::inference::UpstreamHeaders;
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
    session::SteeringQueue,
    telemetry::{ExecutionSpan, InstrumentedStream},
};
use crate::tool::ToolSearchState;
use std::{num::NonZeroUsize, sync::Arc};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

/// A validated retained response whose execution has not started.
///
/// Owns the existing prepared context and its session lease. Request input and
/// settings cannot be changed after preparation. Dropping this value releases
/// the lease without spawning response work or publishing response events;
/// caller-owned checkpoint pins and steering inputs remain with the caller.
/// Steering provenance identifies the batch incorporated into request input;
/// it does not establish successor commitment or authorize queue consumption.
#[must_use = "start the prepared response or drop it to release its session lease"]
pub struct PreparedRetainedResponse {
    // Taken only by consuming start; Option permits ownership transfer from
    // a Drop implementation, as with ResponseRunOwner's task handle.
    pub(super) ctx: Option<RequestContext>,
    pub(super) tools: Option<ToolSearchState>,
    pub(super) exec_ctx: Arc<ExecutionContext>,
    pub(super) client_auth: Option<UpstreamHeaders>,
    pub(super) max_stream_event_bytes: usize,
    pub(super) execution: Option<ExecutionSpan>,
    pub(super) steering_ids: Vec<String>,
}

impl PreparedRetainedResponse {
    fn context(&self) -> &RequestContext {
        self.ctx
            .as_ref()
            .expect("prepared context is retained until consuming start")
    }

    /// The gateway ID allocated during request preparation.
    #[must_use]
    pub fn response_id(&self) -> &str {
        &self.context().response_id
    }

    #[must_use]
    pub fn previous_response_id(&self) -> Option<&str> {
        self.context().original_request.previous_response_id.as_deref()
    }

    /// Verify that this prepared request incorporates this complete queue in
    /// acceptance order. A later admission invalidates the snapshot. Generated
    /// steering IDs identify immutable queue entries, not caller-supplied IDs.
    /// This check neither seals the queue nor proves `response.created` publication.
    #[must_use]
    pub fn matches_steering(&self, queue: &SteeringQueue) -> bool {
        !self.steering_ids.is_empty()
            && self.steering_ids.len() == queue.len()
            && self.steering_ids.iter().zip(queue.entries()).all(|(id, (steer, _))| {
                id == &steer.id && self.previous_response_id() == Some(steer.previous_response_id.as_str())
            })
    }

    /// Start this exact prepared response once, through the existing retained
    /// execution driver. Drain events independently and join the returned owner
    /// on every exit. Starting is not `response.created` publication.
    /// # Panics
    /// Must be called inside a Tokio runtime to spawn the response task.
    pub fn start(self) -> RunningResponse {
        self.start_with_mode(ResponseStartMode::Immediate)
    }

    /// Allocate the retained route and producer, holding execution behind an
    /// activation gate until the connection owner registers the response.
    /// # Panics
    /// Must be called inside a Tokio runtime.
    pub fn start_gated(self) -> RunningResponse {
        self.start_with_mode(ResponseStartMode::AfterRegistration)
    }

    fn start_with_mode(mut self, mode: ResponseStartMode) -> RunningResponse {
        let ctx = self
            .ctx
            .take()
            .expect("prepared context is retained until consuming start");
        let execution = self
            .execution
            .take()
            .expect("prepared execution span is retained until consuming start");
        start(
            ctx,
            self.tools.take(),
            Arc::clone(&self.exec_ctx),
            self.client_auth.take(),
            self.max_stream_event_bytes,
            execution,
            mode,
        )
    }
}

impl Drop for PreparedRetainedResponse {
    fn drop(&mut self) {
        if let Some(execution) = &mut self.execution {
            // Unlike a dropped live stream, an unstarted response has never
            // handed events to a transport. Reuse the existing outcome guard.
            execution.cancelled();
            execution.not_delivered();
        }
    }
}

/// A retained response's event stream, control handle, and execution owner.
/// Drain events independently of waiting for execution or control decisions.
#[must_use = "drain the response events and retain its owner until execution is joined"]
pub struct RunningResponse {
    pub response_id: String,
    pub control: Option<RunControl>,
    pub events: BoxStream,
    pub sink: ResponseEventSink,
    pub owner: ResponseRunOwner,
    /// Immutable effective settings captured before per-round mutation.
    /// Input is empty; automatic successors append their own accepted batch.
    pub settings: crate::types::request_response::RequestPayload,
}

/// Drop requests cancellation; adapters explicitly cancel and join on disconnect.
pub struct ResponseRunOwner {
    state: ResponseRunState,
    cancellation: CancellationToken,
    sink: ResponseEventSink,
}

/// Startup policy is independent of execution or publication outcomes.
#[derive(Clone, Copy)]
pub(super) enum ResponseStartMode {
    Immediate,
    AfterRegistration,
}

/// A response has one task owner and can be activated and joined only once.
enum ResponseRunState {
    AwaitingRegistration {
        task: JoinHandle<ExecutorResult<()>>,
        activate: oneshot::Sender<()>,
    },
    Running(JoinHandle<ExecutorResult<()>>),
    Joined,
}
impl ResponseRunOwner {
    /// Release gated execution after its route and event relay are registered.
    /// # Errors
    /// Rejects repeated activation or an execution task that already stopped.
    pub fn activate(&mut self) -> ExecutorResult<()> {
        match std::mem::replace(&mut self.state, ResponseRunState::Joined) {
            ResponseRunState::AwaitingRegistration { task, activate } => {
                self.state = ResponseRunState::Running(task);
                activate
                    .send(())
                    .map_err(|()| ExecutorError::StreamError("response execution stopped before activation".into()))
            }
            state @ (ResponseRunState::Running(_) | ResponseRunState::Joined) => {
                self.state = state;
                Err(ExecutorError::InvalidRequest(
                    "response execution is already active or joined".into(),
                ))
            }
        }
    }
    async fn execute(
        mut pipeline: AgentPipeline,
        exec: &ExecutionContext,
        auth: Option<&UpstreamHeaders>,
        sink: &ResponseEventSink,
        execution: &mut ExecutionSpan,
    ) -> ExecutorResult<()> {
        let cancellation = pipeline.cancellation_token();
        // The multi-agent coordinator handles cancellation by joining its children.
        let multi = pipeline
            .request
            .enriched_request
            .multi_agent
            .as_ref()
            .is_some_and(|config| config.enabled);
        let run = Box::pin(run_until_gateway_tools_complete(&mut pipeline, exec, auth, true));
        let (payload, metadata) = if multi {
            run.await?
        } else {
            tokio::select! {
                result = run => result?,
                () = cancellation.cancelled() => return Err(ExecutorError::StreamError("response cancelled".into())),
            }
        };
        sink.validate_terminal(&payload)?;
        let status = payload.status.clone();
        let terminal = payload.clone();
        let (ctx, _) = pipeline.into_parts();
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ExecutorError::StreamError("response cancelled before commit".into())),
            result = persist_if_needed(payload, ctx, metadata, exec.conv_handler.clone(), exec.resp_handler.clone(), &exec.metrics) => result?,
        }
        tokio::select! {
            () = cancellation.cancelled() => return Err(ExecutorError::StreamError("response delivery cancelled".into())),
            result = sink.emit_terminal(&terminal) => result?,
        }
        execution.completed_with_status(&status);
        execution.delivered();
        Ok(())
    }
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
        let task = match &mut self.state {
            ResponseRunState::AwaitingRegistration { task, .. } | ResponseRunState::Running(task) => task,
            ResponseRunState::Joined => {
                return Err(ExecutorError::StreamError(
                    "response task has already been joined".into(),
                ));
            }
        };
        let outcome = task.await;
        self.state = ResponseRunState::Joined;
        self.sink.mark_joined();
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
    mut ctx: RequestContext,
    tools: Option<ToolSearchState>,
    exec: Arc<ExecutionContext>,
    auth: Option<UpstreamHeaders>,
    max_bytes: usize,
    mut execution: ExecutionSpan,
    mode: ResponseStartMode,
) -> RunningResponse {
    let mut settings = ctx.enriched_request.continuation_settings();
    if let Some(tools) = tools.as_ref().filter(|tools| tools.is_active()) {
        // Preparation replaces the model-facing catalog. Successors must inherit
        // public declarations, including definitions still deferred for discovery.
        settings.tools = tools.public_effective_tools().map(<[_]>::to_vec);
    }
    let response_id = ctx.response_id.clone();
    let failure_context = StreamFailureContext::from(&ctx);
    let (sender, receiver) = mpsc::channel(STREAM_EVENT_BUFFER);
    let sink = ResponseEventSink::new(sender, max_bytes);
    let relay = StreamRelay::response(sink.clone(), RelayLimits::with_event_bytes(max_bytes));
    let mut pipeline = AgentPipeline::new(ctx, tools, relay);
    pipeline.steering = Some(sink.clone());
    let (activate, activated) = match mode {
        ResponseStartMode::Immediate => (None, None),
        ResponseStartMode::AfterRegistration => {
            let (send, receive) = oneshot::channel();
            (Some(send), Some(receive))
        }
    };
    let clock = execution.clock().clone();
    pipeline.set_execution_clock(clock.clone());
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
            if worker_cancel.is_cancelled() {
                return Err(ExecutorError::StreamError("response cancelled before activation".into()));
            }
            if let Some(activated) = activated {
                tokio::select! {
                    biased;
                    () = worker_cancel.cancelled() => return Err(ExecutorError::StreamError("response cancelled before activation".into())),
                    activation = activated => activation.map_err(|_| ExecutorError::StreamError("response activation was abandoned".into()))?,
                }
            }
            Box::pin(ResponseRunOwner::execute(pipeline, &exec, auth.as_ref(), &worker_sink, &mut execution)).await
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
    RunningResponse {
        response_id,
        control,
        events: Box::pin(InstrumentedStream::timing_delivery(
            ResponseEventSink::stream(receiver, Some(clock.clone())),
            clock,
        )),
        owner: ResponseRunOwner {
            state: match activate {
                Some(activate) => ResponseRunState::AwaitingRegistration { task, activate },
                None => ResponseRunState::Running(task),
            },
            cancellation,
            sink: sink.clone(),
        },
        sink,
        settings,
    }
}
