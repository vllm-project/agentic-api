//! The request façade: [`ExecuteRequest`] prepares one stateful turn,
//! opens its `agentic.execute` span, and hands off to the blocking or
//! streaming driver.

use std::sync::Arc;

use either::Either;
use tracing::{Instrument as _, debug};

use super::run_blocking;
use super::streaming::run_stream;
use crate::executor::RunningResponse;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::inference::BoxStream;
use crate::executor::multi_agent::RunControlReceiver;
use crate::executor::prepare::prepare_request_tools;
use crate::executor::rehydrate::rehydrate_with_continuation;
use crate::executor::rehydrate::validate_reasoning_for_vllm;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::session::ResponseContinuation;
use crate::executor::telemetry::{Api, ExecutionSpan, Route};
use crate::tool::ToolSearchState;
use crate::types::request_response::{RequestPayload, ResponsePayload};

/// Builder for a stateful conversation turn.
///
/// ```ignore
/// ExecuteRequest::new(payload, exec_ctx).with_auth(token).run().await
/// ```
pub struct ExecuteRequest {
    payload: RequestPayload,
    exec_ctx: Arc<ExecutionContext>,
    client_auth: Option<String>,
    continuation: Option<crate::executor::session::ResponseContinuation>,
    max_stream_event_bytes: Option<usize>,
    execution: Option<ExecutionSpan>,
    control: Option<RunControlReceiver>,
}

impl ExecuteRequest {
    #[must_use]
    pub fn new(payload: RequestPayload, exec_ctx: Arc<ExecutionContext>) -> Self {
        Self {
            payload,
            exec_ctx,
            client_auth: None,
            continuation: None,
            max_stream_event_bytes: None,
            execution: None,
            control: None,
        }
    }

    /// Bound every serialized client event, including the terminal
    /// `response.completed`, to what the delivering transport can carry after
    /// its own routing metadata. The configured `max_stream_event_bytes` still
    /// applies; a larger transport limit does not raise it.
    #[must_use]
    pub fn with_max_stream_event_bytes(mut self, max_bytes: usize) -> Self {
        self.max_stream_event_bytes = Some(max_bytes);
        self
    }

    fn effective_max_stream_event_bytes(&self) -> usize {
        let configured = self.exec_ctx.responses_config.max_stream_event_bytes;
        self.max_stream_event_bytes
            .map_or(configured, |transport| transport.min(configured))
    }

    /// Override the bearer token for this request only; does not touch the shared [`ExecutionContext`].
    #[must_use]
    pub fn with_auth(mut self, token: Option<String>) -> Self {
        self.client_auth = token;
        self
    }

    /// Continue the execution span opened by a transport when it admitted the request.
    /// The executor takes responsibility for finalizing its outcomes.
    #[must_use]
    pub fn with_execution_span(mut self, execution: ExecutionSpan) -> Self {
        self.execution = Some(execution);
        self
    }

    /// Retain this turn's continuation state in the supplied serial session.
    ///
    /// # Errors
    /// Returns an error when the session is busy or closed.
    pub fn with_session(mut self, session: &crate::executor::session::ResponseSession) -> ExecutorResult<Self> {
        self.continuation = Some(
            session
                .begin(self.payload.previous_response_id.as_deref())
                .inspect_err(|error| {
                    if let Some(execution) = &mut self.execution {
                        execution.failed(error);
                        execution.not_delivered();
                    }
                })?,
        );
        Ok(self)
    }

    /// Keep a streaming multi-agent run alive for client tool outputs. The
    /// coordinator consumes this endpoint; adapters retain the paired handle.
    /// Admission still requires effective multi-agent configuration and storage.
    #[must_use]
    pub fn with_run_control(mut self, control: RunControlReceiver) -> Self {
        self.control = Some(control);
        self
    }

    /// Start a response with delivery retained until all event-sink handles are dropped.
    /// The adapter must drain events independently and join the owner on every exit.
    /// # Errors
    /// Returns request preparation errors before spawning response work.
    pub async fn run_retained(mut self) -> ExecutorResult<RunningResponse> {
        let mut execution = self
            .execution
            .take()
            .unwrap_or_else(|| ExecutionSpan::start(Api::Responses, Route::Executor, true));
        let limit = self.effective_max_stream_event_bytes();
        let prepared = prepare(self.payload, &self.exec_ctx, self.continuation, false).await;
        let (ctx, tools) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                execution.failed(&error);
                execution.not_delivered();
                return Err(error);
            }
        };
        Ok(super::retained::start(
            ctx,
            tools,
            self.exec_ctx,
            self.client_auth,
            limit,
            execution,
        ))
    }

    /// Execute one stateful conversation turn.
    ///
    /// Returns `Either::Left(ResponsePayload)` for non-streaming requests, or
    /// `Either::Right(BoxStream)` for streaming, where each yielded `String` is
    /// a complete SSE frame ready to forward to the client.
    ///
    /// # Errors
    /// Returns [`ExecutorError`] if rehydration or (non-streaming) LLM inference fails.
    pub async fn run(mut self) -> ExecutorResult<Either<ResponsePayload, BoxStream>> {
        let execution = self
            .execution
            .take()
            .unwrap_or_else(|| ExecutionSpan::start(Api::Responses, Route::Executor, self.payload.stream));
        let span = execution.span().clone();
        self.run_traced(execution).instrument(span).await
    }

    /// The body of [`Self::run`], executed inside the `agentic.execute` span.
    ///
    /// Takes the span guard by value so the streaming path can move it into
    /// the stream, where it lives until the last frame is yielded or the
    /// stream is dropped. Every other path finalizes it here.
    async fn run_traced(self, mut execution: ExecutionSpan) -> ExecutorResult<Either<ResponsePayload, BoxStream>> {
        debug!(
            model = %self.payload.model,
            store = self.payload.store,
            stream = self.payload.stream,
            has_previous_response_id = self.payload.previous_response_id.is_some(),
            has_conversation_id = self.payload.conversation_id.is_some(),
            tools = self.payload.tools.as_ref().map_or(0, Vec::len),
            "executor received responses request"
        );
        let max_stream_event_bytes = self.effective_max_stream_event_bytes();
        let prepared = prepare(self.payload, &self.exec_ctx, self.continuation, self.control.is_some()).await;
        let (ctx, tool_search_state) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                execution.failed(&error);
                execution.not_delivered();
                return Err(error);
            }
        };
        if ctx.original_request.stream {
            return Ok(Either::Right(run_stream(
                ctx,
                tool_search_state,
                self.exec_ctx,
                self.client_auth,
                max_stream_event_bytes,
                execution,
                self.control,
            )));
        }
        let result = Box::pin(run_blocking(
            ctx,
            tool_search_state,
            &self.exec_ctx,
            self.client_auth.as_deref(),
            max_stream_event_bytes,
        ))
        .await;
        match result {
            Ok(payload) => {
                execution.completed_with_status(&payload.status);
                execution.delivered();
                Ok(Either::Left(payload))
            }
            Err(error) => {
                execution.failed(&error);
                execution.not_delivered();
                Err(error)
            }
        }
    }
}

/// Execute one stateful conversation turn.
///
/// Thin shim over [`ExecuteRequest`] for callers that don't need per-request auth override.
///
/// # Errors
/// Returns [`crate::executor::error::ExecutorError`] if rehydration or (non-streaming) LLM inference fails.
pub async fn execute(
    request: RequestPayload,
    exec_ctx: Arc<ExecutionContext>,
) -> ExecutorResult<Either<ResponsePayload, BoxStream>> {
    ExecuteRequest::new(request, exec_ctx).run().await
}

async fn prepare(
    payload: RequestPayload,
    exec: &ExecutionContext,
    continuation: Option<ResponseContinuation>,
    controlled: bool,
) -> ExecutorResult<(RequestContext, Option<ToolSearchState>)> {
    let ctx = rehydrate_with_continuation(payload, exec, continuation).await?;
    if controlled
        && (!ctx.original_request.stream
            || !ctx
                .enriched_request
                .multi_agent
                .as_ref()
                .is_some_and(|config| config.enabled)
            || ctx.original_request.input.has_compaction_trigger())
    {
        return Err(ExecutorError::InvalidRequest(
            "run control requires streaming multi-agent inference".into(),
        ));
    }
    if !ctx.enriched_request.input.has_compaction_trigger() {
        validate_reasoning_for_vllm(&ctx.enriched_request.input)?;
    }
    prepare_request_tools(ctx, &exec.conv_handler, &exec.resp_handler).await
}
