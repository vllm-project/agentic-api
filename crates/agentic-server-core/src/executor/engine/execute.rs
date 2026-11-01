//! The request façade: [`ExecuteRequest`] prepares one stateful turn,
//! opens its `agentic.execute` span, and hands off to the blocking or
//! streaming driver.

use crate::executor::inference::UpstreamHeaders;
#[cfg(test)]
use crate::types::io::InOutItem;
use std::sync::Arc;

use either::Either;
use tracing::{Instrument as _, debug};

use super::retained::PreparedRetainedResponse;
use super::run_blocking;
use super::streaming::{StreamOptions, run_stream};
use crate::executor::RunningResponse;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::inference::BoxStream;
use crate::executor::multi_agent::RunControlReceiver;
use crate::executor::prepare::prepare_request_tools;
use crate::executor::rehydrate::rehydrate_with_continuation;
use crate::executor::rehydrate::validate_reasoning_for_vllm;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::session::{ResponseCheckpointPin, ResponseContinuation, ResponseSession, SteeringQueue};
use crate::executor::telemetry::{Api, ExecutionSpan, Route};
use crate::tool::ToolSearchState;
use crate::types::io::ResponsesInput;
use crate::types::request_response::{RequestPayload, ResponsePayload};

/// Builder for a stateful conversation turn.
///
/// ```ignore
/// ExecuteRequest::new(payload, exec_ctx).with_auth(token).run().await
/// ```
pub struct ExecuteRequest {
    payload: RequestPayload,
    exec_ctx: Arc<ExecutionContext>,
    client_auth: Option<UpstreamHeaders>,
    defer_http_status: bool,
    continuation: Option<ResponseContinuation>,
    pinned_continuation: bool,
    steering_ids: Vec<String>,
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
            defer_http_status: false,
            continuation: None,
            pinned_continuation: false,
            steering_ids: Vec::new(),
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

    /// Supply request-scoped upstream headers already filtered by the transport.
    /// Replaces a bearer-only override, including its fallback credentials.
    #[must_use]
    pub fn with_upstream_headers(mut self, headers: http::HeaderMap) -> Self {
        self.client_auth = Some(UpstreamHeaders::Forwarded(headers));
        self
    }

    /// Override the bearer token for this request only; does not touch the shared [`ExecutionContext`].
    #[must_use]
    pub fn with_auth(mut self, token: Option<String>) -> Self {
        self.client_auth = token.map(UpstreamHeaders::Bearer);
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
    /// With `None`, resolve the parent through the existing session and request
    /// preparation path. With `Some(pin)`, use that exact checkpoint in its
    /// owning session; the request's `previous_response_id` must identify it.
    /// Steering successors must supply their retained pin.
    ///
    /// Borrows the caller's pin only while acquiring the execution lease. The
    /// lease retains the same checkpoint through the existing preparation and
    /// execution path, even if the session cache has discarded it. Keep the pin
    /// to retry after rejected preparation; this method neither accepts steering
    /// nor changes the request's settings or input.
    ///
    /// # Errors
    /// Rejects an already attached session or a busy/closed session. Supplying
    /// a pin also rejects a conversation request or mismatched parent ID/owner.
    /// An error drops this consumed builder and releases any lease it held.
    pub fn with_session(self, session: &ResponseSession, pin: Option<&ResponseCheckpointPin>) -> ExecutorResult<Self> {
        let continuation = self.validate_session_attachment().and_then(|()| match pin {
            Some(pin) => {
                if self.payload.conversation.is_some() {
                    return Err(ExecutorError::InvalidRequest(
                        "pinned response continuation cannot use a conversation".to_owned(),
                    ));
                }
                session.begin_pinned(self.payload.previous_response_id.as_deref(), pin)
            }
            None => session.begin(self.payload.previous_response_id.as_deref()),
        });
        self.with_continuation(continuation).map(|mut request| {
            request.pinned_continuation = pin.is_some();
            request
        })
    }

    fn validate_session_attachment(&self) -> ExecutorResult<()> {
        if self.continuation.is_some() {
            Err(ExecutorError::InvalidRequest(
                "request already has a response session".to_owned(),
            ))
        } else {
            Ok(())
        }
    }

    fn with_continuation(mut self, continuation: ExecutorResult<ResponseContinuation>) -> ExecutorResult<Self> {
        self.continuation = Some(continuation.inspect_err(|error| {
            if let Some(execution) = &mut self.execution {
                execution.failed(error);
                execution.not_delivered();
            }
        })?);
        Ok(self)
    }

    /// Snapshot a qualified queue and prepend its user input to the submitted
    /// input, once and in acceptance order. Attach the owning session with
    /// `Some(pin)` first. The queue and pin remain owned by the caller.
    ///
    /// Use [`Self::prepare_retained`] and check the returned prepared response
    /// against the queue before reserving a successor. Required tool outputs
    /// still pass through ordinary preparation; rejection preserves the queue.
    /// Qualification, sealing and commitment remain the connection owner's work.
    ///
    /// Only admitted inputs and generated IDs are copied. Their source is bounded
    /// by the queue's limits; these execution copies are outside its retention
    /// charge, so the owner must also bound concurrent preparation and active work.
    /// # Errors
    /// Rejects an empty queue, repeated attachment, or a request without an
    /// explicitly pinned continuation for this exact parent. This consumed
    /// builder releases its lease on error without consuming accepted inputs.
    pub fn with_steering(mut self, queue: &SteeringQueue) -> ExecutorResult<Self> {
        self.validate_steering_attachment(queue).inspect_err(|error| {
            if let Some(execution) = &mut self.execution {
                execution.failed(error);
                execution.not_delivered();
            }
        })?;
        let mut items = Vec::new();
        for (_, input) in queue.entries() {
            items.extend(input.as_ref().clone().into_canonical_items());
        }
        items.extend(std::mem::take(&mut self.payload.input).into_canonical_items());
        self.payload.input = ResponsesInput::from(items);
        // Generated IDs are fixed-size and the admitted entry count is bounded.
        // Do not copy descriptors' repeated parent IDs or serialize input again.
        self.steering_ids = queue.entries().map(|(steer, _)| steer.id.clone()).collect();
        Ok(self)
    }

    fn validate_steering_attachment(&self, queue: &SteeringQueue) -> ExecutorResult<()> {
        if !self.steering_ids.is_empty() {
            return Err(ExecutorError::InvalidRequest(
                "request already has steering input".into(),
            ));
        }
        let Some((steer, _)) = queue.entries().next() else {
            return Err(ExecutorError::InvalidRequest("steering input queue is empty".into()));
        };
        let parent = self
            .continuation
            .as_ref()
            .and_then(|continuation| continuation.parent.as_ref());
        if !self.pinned_continuation
            || self.payload.previous_response_id.as_deref() != Some(steer.previous_response_id.as_str())
            || parent.is_none_or(|parent| parent.response_id != steer.previous_response_id)
        {
            return Err(ExecutorError::InvalidRequest(
                "steering input requires an explicitly pinned continuation for its parent".into(),
            ));
        }
        Ok(())
    }

    fn validate_direct_execution(&mut self) -> ExecutorResult<()> {
        if self.steering_ids.is_empty() {
            return Ok(());
        }
        let error = ExecutorError::InvalidRequest("steering input requires explicit retained preparation".into());
        if let Some(execution) = &mut self.execution {
            execution.failed(&error);
            execution.not_delivered();
        }
        Err(error)
    }

    /// Allow a streaming multi-agent run to receive live client tool outputs. The
    /// coordinator consumes this endpoint; adapters retain the paired handle.
    /// Admission requires effective multi-agent configuration and streaming inference.
    /// Use with [`Self::run`]; [`Self::run_retained`] creates its own control pair.
    #[must_use]
    pub fn with_run_control(mut self, control: RunControlReceiver) -> Self {
        self.control = Some(control);
        self
    }

    /// Validate and rehydrate a retained response without starting execution or
    /// creating an event producer. Ordinary preparation may resolve storage and
    /// tools; it uses exactly the same path as [`Self::run_retained`].
    ///
    /// Success retains the acquired session lease until the prepared value is
    /// started or dropped. Errors release the lease; the caller's pin remains
    /// available for retry. Preparation neither consumes steering input nor
    /// establishes successor commitment.
    /// # Errors
    /// Returns request preparation errors or rejects an externally supplied
    /// control endpoint before spawning response work.
    pub async fn prepare_retained(mut self) -> ExecutorResult<PreparedRetainedResponse> {
        let mut execution = self
            .execution
            .take()
            .unwrap_or_else(|| ExecutionSpan::start(Api::Responses, Route::Executor, true, &self.exec_ctx.metrics));
        let limit = self.effective_max_stream_event_bytes();
        let prepared = if self.control.is_some() {
            Err(ExecutorError::InvalidRequest(
                "retained execution creates its own run control; use run for an external control".into(),
            ))
        } else {
            let explicit_steering = !self.steering_ids.is_empty();
            // Omitted explicit tools and tool choice mean this request's own
            // defaults, not the parent's inherited tools. Resolve them before
            // ordinary preparation can discover or load parent tools.
            if explicit_steering {
                self.payload.tools.get_or_insert_with(Vec::new);
                self.payload
                    .tool_choice
                    .get_or_insert(crate::types::io::ToolChoice::Auto);
            }
            prepare(self.payload, &self.exec_ctx, self.continuation, false).await
        };
        let (ctx, tools) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                execution.failed(&error);
                execution.not_delivered();
                return Err(error);
            }
        };
        Ok(PreparedRetainedResponse {
            ctx: Some(ctx),
            tools,
            exec_ctx: self.exec_ctx,
            client_auth: self.client_auth,
            max_stream_event_bytes: limit,
            execution: Some(execution),
            steering_ids: self.steering_ids,
        })
    }

    /// Prepare and start a response with delivery retained until all event-sink
    /// handles are dropped. Drain events independently and join its owner on
    /// every exit. Call [`Self::prepare_retained`] to separate these two steps.
    /// # Errors
    /// Returns the same preparation errors as [`Self::prepare_retained`], or
    /// rejects steering input that requires explicit preparation and handoff.
    pub async fn run_retained(mut self) -> ExecutorResult<RunningResponse> {
        self.validate_direct_execution()?;
        Ok(self.prepare_retained().await?.start())
    }

    /// Execute an HTTP response, retaining initial upstream failures as HTTP errors.
    /// Streaming startup waits for the first client frame before returning success.
    /// Later failures remain SSE events; dropping the request cancels its producer.
    ///
    /// # Errors
    /// Returns preparation errors and upstream failures before the first client frame.
    pub async fn run_http(mut self) -> ExecutorResult<Either<ResponsePayload, BoxStream>> {
        self.defer_http_status = true;
        self.run().await
    }

    /// Execute one stateful conversation turn.
    ///
    /// Returns `Either::Left(ResponsePayload)` for non-streaming requests, or
    /// `Either::Right(BoxStream)` for streaming, where each yielded `String` is
    /// a complete SSE frame ready to forward to the client.
    ///
    /// # Errors
    /// Returns [`ExecutorError`] if rehydration or (non-streaming) LLM inference
    /// fails, or if steering input requires explicit retained preparation.
    pub async fn run(mut self) -> ExecutorResult<Either<ResponsePayload, BoxStream>> {
        self.validate_direct_execution()?;
        let execution = self.execution.take().unwrap_or_else(|| {
            ExecutionSpan::start(
                Api::Responses,
                Route::Executor,
                self.payload.stream,
                &self.exec_ctx.metrics,
            )
        });
        let span = execution.span().clone();
        Box::pin(self.run_traced(execution)).instrument(span).await
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
            has_conversation_id = self.payload.conversation.is_some(),
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
            return Ok(Either::Right(
                run_stream(
                    ctx,
                    tool_search_state,
                    self.exec_ctx,
                    self.client_auth,
                    StreamOptions {
                        max_event_bytes: max_stream_event_bytes,
                        defer_http_status: self.defer_http_status,
                    },
                    execution,
                    self.control,
                )
                .await?,
            ));
        }
        let result = Box::pin(run_blocking(
            ctx,
            tool_search_state,
            &self.exec_ctx,
            self.client_auth.as_ref(),
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

/// Prepare a rehydrated turn for local completion without model generation.
/// Consumes the context so rejected input cannot expose a partially prepared
/// agent tree. The returned context is ready for [`crate::executor::persist_turn`].
///
/// # Errors
/// Rejects invalid multi-agent continuation input or checkpoint retention limits
/// before publishing a checkpoint.
pub fn prepare_non_generating_turn(mut ctx: RequestContext, exec: &ExecutionContext) -> ExecutorResult<RequestContext> {
    super::multi_agent::prepare_without_inference(&mut ctx, exec)?;
    Ok(ctx)
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use futures::FutureExt;
    use serde_json::{Value, json};

    use super::*;
    use crate::executor::{ConversationHandler, ResponseHandler, commit, rehydrate_in_session};
    use crate::storage::{ConversationStore, ResponseStore, StorageError};
    use crate::tool::ToolError;
    use crate::types::io::{InputItem, InputMessageContent, ResponsesInput};
    use crate::utils::common::deserialize_from_value;

    fn execution() -> Arc<ExecutionContext> {
        Arc::new(ExecutionContext::new(
            ConversationHandler::new(ConversationStore::disabled()),
            ResponseHandler::new(ResponseStore::disabled()),
            Arc::new(reqwest::Client::new()),
            "http://127.0.0.1:1".to_owned(),
        ))
    }

    fn session() -> ResponseSession {
        ResponseSession::new(NonZeroUsize::new(100).unwrap(), NonZeroUsize::new(100_000).unwrap())
    }

    fn request(previous: Option<&str>, input: Value) -> RequestPayload {
        let mut payload = json!({
            "model": "test-model", "store": false,
            "previous_response_id": previous
        });
        payload["input"] = input;
        deserialize_from_value(payload).unwrap()
    }

    async fn pin_parent(exec: &ExecutionContext, session: &ResponseSession) -> ResponseCheckpointPin {
        pin_parent_with_output(
            exec,
            session,
            json!([{
                "type": "function_call", "id": "fc_weather", "call_id": "weather",
                "name": "get_weather", "arguments": "{}", "status": "completed"
            }]),
        )
        .await
    }

    async fn pin_parent_with_output(
        exec: &ExecutionContext,
        session: &ResponseSession,
        output: Value,
    ) -> ResponseCheckpointPin {
        let ctx = rehydrate_in_session(request(None, json!("parent prompt")), exec, session)
            .await
            .unwrap();
        let parent_id = ctx.response_id.clone();
        let payload = deserialize_from_value(json!({
            "id": parent_id, "object": "response", "created_at": 0,
            "model": "test-model", "status": "completed", "output": output
        }))
        .unwrap();
        Box::pin(commit(ctx, payload, exec)).await.unwrap();
        session.pin_response(&parent_id).unwrap().unwrap()
    }

    fn output(call_id: &str) -> Value {
        json!([
            {"type": "function_call_output", "call_id": call_id, "output": "sunny"},
            {"role": "user", "content": "successor input"}
        ])
    }

    fn steering_queue(parent: &str) -> SteeringQueue {
        SteeringQueue::new(
            parent.to_owned(),
            NonZeroUsize::new(8).unwrap(),
            NonZeroUsize::new(100_000).unwrap(),
        )
        .unwrap()
    }

    fn steer(text: &str) -> crate::types::websocket::SteeringInput {
        ResponsesInput::Text(text.to_owned()).try_into().unwrap()
    }

    #[tokio::test]
    async fn steering_preparation_preserves_order_media_and_canonical_custom_outputs() {
        let exec = execution();
        let session = session();
        let pin = pin_parent_with_output(
            &exec,
            &session,
            json!([
                {"type":"function_call", "id":"fc_weather", "call_id":"weather",
                    "name":"get_weather", "arguments":"{}", "status":"completed"},
                {"type":"custom_tool_call", "id":"ctc_notes", "call_id":"notes",
                    "name":"record_notes", "input":"weather notes"}
            ]),
        )
        .await;
        let mut queue = steering_queue(pin.response_id());
        queue.try_accept(steer("")).unwrap();
        queue
            .try_accept(
                deserialize_from_value(json!([{
                    "role":"user", "content":[
                        {"type":"input_text", "text":"FIRST_UPDATE"},
                        {"type":"input_image", "image_url":"data:image/png;base64,test", "detail":"high"}
                    ]
                }]))
                .unwrap(),
            )
            .unwrap();
        queue.try_accept(steer("SECOND_UPDATE")).unwrap();
        let ids = queue.entries().map(|(steer, _)| steer.id.clone()).collect::<Vec<_>>();
        let submitted = json!([
            {"type":"function_call_output", "call_id":"weather", "output":"sunny"},
            {"type":"custom_tool_call_output", "call_id":"notes", "output":"recorded"},
            {"role":"user", "content":"EXPLICIT_INPUT"}
        ]);
        let prepared = ExecuteRequest::new(request(Some(pin.response_id()), submitted), Arc::clone(&exec))
            .with_session(&session, Some(&pin))
            .unwrap()
            .with_steering(&queue)
            .unwrap()
            .prepare_retained()
            .await
            .unwrap();
        assert!(prepared.matches_steering(&queue));
        assert_eq!(
            queue.entries().map(|(steer, _)| steer.id.clone()).collect::<Vec<_>>(),
            ids
        );
        let ResponsesInput::Items(items) = &prepared.ctx.as_ref().unwrap().original_request.input else {
            panic!("items")
        };
        assert_eq!(items.len(), 6);
        assert!(matches!(&items[0], InOutItem::Input(InputItem::Message(message))
            if matches!(&message.content, InputMessageContent::Text(text) if text.is_empty())));
        assert!(matches!(&items[1], InOutItem::Input(InputItem::Message(message))
            if matches!(&message.content, InputMessageContent::Parts(parts) if parts.len() == 2)));
        assert!(matches!(&items[2], InOutItem::Input(InputItem::Message(message))
            if matches!(&message.content, InputMessageContent::Text(text) if text == "SECOND_UPDATE")));
        assert!(
            matches!(&items[3], InOutItem::Input(InputItem::FunctionCallOutput(output)) if output.call_id == "weather")
        );
        assert!(
            matches!(&items[4], InOutItem::Input(InputItem::CustomToolCallOutput(output)) if output.call_id == "notes")
        );
        assert!(matches!(&items[5], InOutItem::Input(InputItem::Message(message))
            if matches!(&message.content, InputMessageContent::Text(text) if text == "EXPLICIT_INPUT")));
        assert!(
            matches!(&prepared.ctx.as_ref().unwrap().new_input_items[4], InOutItem::Input(InputItem::CustomToolCallOutput(output))
            if output.call_id == "notes")
        );
        assert!(!prepared.matches_steering(&steering_queue(pin.response_id())));
        let mut unrelated = steering_queue(pin.response_id());
        for (_, input) in queue.entries() {
            unrelated
                .try_accept(input.as_ref().clone().try_into().unwrap())
                .unwrap();
        }
        // Same parent, count and content still do not identify the accepted IDs.
        assert!(!prepared.matches_steering(&unrelated));
        drop(prepared);
        assert_eq!(queue.len(), 3);
    }

    #[tokio::test]
    async fn rejected_wrong_or_partial_outputs_preserve_steering_for_corrected_preparation() {
        let exec = execution();
        let session = session();
        let pin = pin_parent_with_output(
            &exec,
            &session,
            json!([
                {"type":"function_call", "id":"fc_weather", "call_id":"weather",
                    "name":"get_weather", "arguments":"{}", "status":"completed"},
                {"type":"function_call", "id":"fc_humidity", "call_id":"humidity",
                    "name":"get_humidity", "arguments":"{}", "status":"completed"}
            ]),
        )
        .await;
        let mut queue = steering_queue(pin.response_id());
        queue.try_accept(steer("KEEP_FIRST")).unwrap();
        queue.try_accept(steer("KEEP_SECOND")).unwrap();
        let ids = queue.entries().map(|(steer, _)| steer.id.clone()).collect::<Vec<_>>();
        for (submitted, wrong) in [(output("unknown"), true), (output("weather"), false)] {
            let result = ExecuteRequest::new(request(Some(pin.response_id()), submitted), Arc::clone(&exec))
                .with_session(&session, Some(&pin))
                .unwrap()
                .with_steering(&queue)
                .unwrap()
                .prepare_retained()
                .await;
            if wrong {
                assert!(
                    matches!(result, Err(ExecutorError::Tool(ToolError::UnknownCallOutput { call_id }))
                    if call_id == "unknown")
                );
            } else {
                assert!(
                    matches!(result, Err(ExecutorError::Tool(ToolError::MissingOutput { call_id }))
                    if call_id == "humidity")
                );
            }
            assert_eq!(
                queue.entries().map(|(steer, _)| steer.id.clone()).collect::<Vec<_>>(),
                ids
            );
            assert!(session.wait_until_idle().now_or_never().unwrap().is_ok());
        }
        let prepared = ExecuteRequest::new(
            request(
                Some(pin.response_id()),
                json!([
                    {"type":"function_call_output", "call_id":"weather", "output":"sunny"},
                    {"type":"function_call_output", "call_id":"humidity", "output":"dry"}
                ]),
            ),
            Arc::clone(&exec),
        )
        .with_session(&session, Some(&pin))
        .unwrap()
        .with_steering(&queue)
        .unwrap()
        .prepare_retained()
        .await
        .unwrap();
        assert!(prepared.matches_steering(&queue));
        assert_eq!(queue.len(), 2);
    }

    #[tokio::test]
    async fn steering_attachment_requires_an_explicit_matching_pin_and_is_single_use() {
        let exec = execution();
        let session = session();
        let pin = pin_parent(&exec, &session).await;
        let mut queue = steering_queue(pin.response_id());
        queue.try_accept(steer("update")).unwrap();
        // A cached parent is present, but None is not an explicit retained pin.
        let ordinary = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
            .with_session(&session, None)
            .unwrap();
        assert!(ordinary.continuation.as_ref().unwrap().parent.is_some());
        assert!(matches!(
            ordinary.with_steering(&queue),
            Err(ExecutorError::InvalidRequest(_))
        ));
        assert!(matches!(
            ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
                .with_steering(&queue),
            Err(ExecutorError::InvalidRequest(_))
        ));
        let mut wrong_parent = steering_queue("resp_other");
        wrong_parent.try_accept(steer("other")).unwrap();
        for rejected in [&wrong_parent, &steering_queue(pin.response_id())] {
            let builder = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
                .with_session(&session, Some(&pin))
                .unwrap();
            assert!(matches!(
                builder.with_steering(rejected),
                Err(ExecutorError::InvalidRequest(_))
            ));
        }
        let builder = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
            .with_session(&session, Some(&pin))
            .unwrap()
            .with_steering(&queue)
            .unwrap();
        assert!(
            matches!(builder.with_steering(&queue), Err(ExecutorError::InvalidRequest(message))
            if message == "request already has steering input")
        );
        assert_eq!(queue.len(), 1);
        assert!(session.wait_until_idle().now_or_never().unwrap().is_ok());
    }

    #[tokio::test]
    async fn later_admission_invalidates_the_prepared_steering_snapshot() {
        let exec = execution();
        let session = session();
        let pin = pin_parent(&exec, &session).await;
        let mut queue = steering_queue(pin.response_id());
        queue.try_accept(steer("first")).unwrap();
        let builder = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
            .with_session(&session, Some(&pin))
            .unwrap()
            .with_steering(&queue)
            .unwrap();
        // No queue borrow escapes the setter. Admission can continue while
        // preparation is outstanding, but the resulting snapshot is now stale.
        queue.try_accept(steer("second")).unwrap();
        let stale = builder.prepare_retained().await.unwrap();
        assert!(!stale.matches_steering(&queue));
        drop(stale);
        let fresh = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
            .with_session(&session, Some(&pin))
            .unwrap()
            .with_steering(&queue)
            .unwrap()
            .prepare_retained()
            .await
            .unwrap();
        assert!(fresh.matches_steering(&queue));
        drop(fresh);
        let unbound = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
            .with_session(&session, Some(&pin))
            .unwrap()
            .prepare_retained()
            .await
            .unwrap();
        assert!(!unbound.matches_steering(&queue));
        drop(unbound);
        let cancelled = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
            .with_session(&session, Some(&pin))
            .unwrap()
            .with_steering(&queue)
            .unwrap()
            .prepare_retained();
        drop(cancelled);
        assert_eq!(queue.len(), 2);
        assert!(session.wait_until_idle().now_or_never().unwrap().is_ok());
    }

    #[tokio::test]
    async fn steering_bound_requests_require_prepared_handoff_instead_of_convenience_drivers() {
        let exec = execution();
        let session = session();
        let pin = pin_parent(&exec, &session).await;
        let mut queue = steering_queue(pin.response_id());
        queue.try_accept(steer("update")).unwrap();
        for retained in [false, true] {
            let builder = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
                .with_session(&session, Some(&pin))
                .unwrap()
                .with_steering(&queue)
                .unwrap();
            let error = if retained {
                builder.run_retained().await.err().unwrap()
            } else {
                builder.run().await.err().unwrap()
            };
            assert!(matches!(error, ExecutorError::InvalidRequest(message)
                if message == "steering input requires explicit retained preparation"));
            assert_eq!(queue.len(), 1);
            assert!(session.wait_until_idle().now_or_never().unwrap().is_ok());
        }
    }

    #[tokio::test]
    async fn prepared_retained_response_holds_the_lease_and_retries_after_disposal() {
        let exec = execution();
        let session = session();
        let pin = pin_parent(&exec, &session).await;
        let mut payload = request(Some(pin.response_id()), output("weather"));
        payload.stream = true;
        payload.instructions = Some("explicit successor instructions".to_owned());
        payload.max_output_tokens = Some(64);
        let prepared = ExecuteRequest::new(payload, Arc::clone(&exec))
            .with_session(&session, Some(&pin))
            .unwrap()
            .with_auth(Some("test-token".to_owned()))
            .with_max_stream_event_bytes(512)
            .prepare_retained()
            .await
            .unwrap();
        let first_id = prepared.response_id().to_owned();
        assert_ne!(first_id, pin.response_id());
        assert_eq!(prepared.previous_response_id(), Some(pin.response_id()));
        assert!(matches!(prepared.client_auth.as_ref(), Some(UpstreamHeaders::Bearer(token)) if token == "test-token"));
        assert_eq!(
            prepared.max_stream_event_bytes,
            512.min(exec.responses_config.max_stream_event_bytes)
        );
        assert_eq!(
            prepared.ctx.as_ref().unwrap().enriched_request.instructions.as_deref(),
            Some("explicit successor instructions")
        );
        assert_eq!(
            prepared.ctx.as_ref().unwrap().enriched_request.max_output_tokens,
            Some(64)
        );
        assert!(!prepared.ctx.as_ref().unwrap().original_request.store);
        // Preparation retains the canonical parent and new input through the
        // same request context, without handing either to an execution task.
        assert_eq!(prepared.ctx.as_ref().unwrap().new_input_items.len(), 2);
        assert!(matches!(session.begin(None), Err(ExecutorError::InvalidRequest(_))));
        tokio::task::yield_now().await;
        assert!(matches!(session.begin(None), Err(ExecutorError::InvalidRequest(_))));
        drop(prepared);
        assert!(session.wait_until_idle().now_or_never().unwrap().is_ok());
        assert!(session.pin_response(pin.response_id()).unwrap().is_none());

        // Dropping the lease evicts ordinary cached state. The caller-owned
        // exact pin still permits a second preparation without storage fallback.
        let retry = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
            .with_session(&session, Some(&pin))
            .unwrap()
            .prepare_retained()
            .await
            .unwrap();
        assert_eq!(retry.previous_response_id(), Some(pin.response_id()));
        assert_ne!(retry.response_id(), first_id);
        drop(retry);
        drop(session.begin(None).unwrap());
    }

    #[tokio::test]
    async fn starting_a_prepared_response_preserves_its_identity_and_cleanup_owner() {
        let exec = execution();
        let session = session();
        let pin = pin_parent(&exec, &session).await;
        let prepared = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
            .with_session(&session, Some(&pin))
            .unwrap()
            .prepare_retained()
            .await
            .unwrap();
        let response_id = prepared.response_id().to_owned();
        let mut running = prepared.start();
        assert_eq!(running.response_id, response_id);
        assert!(running.control.is_none());
        // This current-thread test has not yielded to the new producer yet.
        // start is distinct from creation publication and cannot provide proof.
        assert!(running.sink.created_receipt().is_none());
        running.owner.cancel();
        assert!(running.owner.join().await.is_err());
        drop(running);
        assert!(session.wait_until_idle().now_or_never().unwrap().is_ok());
        drop(session.begin(None).unwrap());
    }

    #[tokio::test]
    async fn retained_preparation_rejects_external_control_and_preserves_the_pin() {
        let exec = execution();
        let session = session();
        let pin = pin_parent(&exec, &session).await;
        let (_control, receiver) = crate::executor::multi_agent::RunControl::channel(NonZeroUsize::new(1024).unwrap());
        let result = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
            .with_session(&session, Some(&pin))
            .unwrap()
            .with_run_control(receiver)
            .prepare_retained()
            .await;
        assert!(matches!(result, Err(ExecutorError::InvalidRequest(message))
            if message.contains("retained execution creates its own run control")));
        assert!(session.wait_until_idle().now_or_never().unwrap().is_ok());
        let retry = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
            .with_session(&session, Some(&pin))
            .unwrap()
            .prepare_retained()
            .await
            .unwrap();
        drop(retry);
    }

    #[tokio::test]
    async fn pinned_builder_retries_rejected_output_through_existing_preparation() {
        let exec = execution();
        let session = session();
        let pin = pin_parent(&exec, &session).await;

        // Both public drivers reject before inference. Neither failure consumes
        // the caller's exact parent, even though the ordinary cache is evicted.
        for retained in [false, true] {
            let mut payload = request(Some(pin.response_id()), output("unknown"));
            payload.stream = retained;
            let builder = ExecuteRequest::new(payload, Arc::clone(&exec))
                .with_session(&session, Some(&pin))
                .unwrap();
            let error = if retained {
                builder.run_retained().await.err().unwrap()
            } else {
                builder.run().await.err().unwrap()
            };
            assert!(
                matches!(error, ExecutorError::Tool(ToolError::UnknownCallOutput { call_id })
                if call_id == "unknown")
            );
            assert!(session.pin_response(pin.response_id()).unwrap().is_none());
        }

        let ordinary = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
            .with_session(&session, None)
            .unwrap();
        assert!(matches!(
            ordinary.run().await,
            Err(ExecutorError::Storage(StorageError::PreviousResponseNotFound { .. }))
        ));

        for stream in [false, true] {
            let mut payload = request(Some(pin.response_id()), output("weather"));
            payload.stream = stream;
            payload.instructions = Some("explicit successor instructions".to_owned());
            payload.max_output_tokens = Some(64);
            let builder = ExecuteRequest::new(payload, Arc::clone(&exec))
                .with_session(&session, Some(&pin))
                .unwrap();
            let (ctx, _) = prepare(builder.payload, &builder.exec_ctx, builder.continuation, false)
                .await
                .unwrap();
            assert!(!ctx.original_request.store);
            assert_eq!(ctx.original_request.stream, stream);
            assert_eq!(
                ctx.original_request.previous_response_id.as_deref(),
                Some(pin.response_id())
            );
            assert_eq!(
                ctx.enriched_request.instructions.as_deref(),
                Some("explicit successor instructions")
            );
            assert_eq!(ctx.enriched_request.max_output_tokens, Some(64));
            assert!(ctx.enriched_request.previous_response_id.is_none());
            let ResponsesInput::Items(items) = &ctx.enriched_request.input else {
                panic!("preparation must combine parent history and successor input");
            };
            assert_eq!(items.len(), 4);
            assert!(matches!(&items[0], InOutItem::Input(InputItem::Message(message))
                if matches!(&message.content, InputMessageContent::Text(text) if text == "parent prompt")));
            assert!(matches!(&items[1], InOutItem::Input(InputItem::FunctionCall(call)) if call.call_id == "weather"));
            assert!(
                matches!(&items[2], InOutItem::Input(InputItem::FunctionCallOutput(output)) if output.call_id == "weather")
            );
            assert!(matches!(&items[3], InOutItem::Input(InputItem::Message(message))
                if matches!(&message.content, InputMessageContent::Text(text) if text == "successor input")));
            drop(ctx);
        }
    }

    #[tokio::test]
    async fn pinned_builder_rejects_retargeting_before_acquiring_a_lease() {
        let exec = execution();
        let owner = session();
        let other = session();
        let pin = pin_parent(&exec, &owner).await;
        for previous in [None, Some("different-parent")] {
            assert!(matches!(
                ExecuteRequest::new(request(previous, output("weather")), Arc::clone(&exec))
                    .with_session(&owner, Some(&pin)),
                Err(ExecutorError::InvalidRequest(_))
            ));
        }
        let mut conversation = request(Some(pin.response_id()), output("weather"));
        conversation.conversation = Some("conversation".to_owned());
        assert!(matches!(
            ExecuteRequest::new(conversation, Arc::clone(&exec)).with_session(&owner, Some(&pin)),
            Err(ExecutorError::InvalidRequest(_))
        ));
        assert!(matches!(
            ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
                .with_session(&other, Some(&pin)),
            Err(ExecutorError::InvalidRequest(_))
        ));
        drop(other.begin(None).unwrap());

        let builder = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
            .with_session(&owner, Some(&pin))
            .unwrap();
        // The acquired lease owns the parent; no pin borrow escapes the setter.
        drop(pin);
        let (ctx, _) = prepare(builder.payload, &builder.exec_ctx, builder.continuation, false)
            .await
            .unwrap();
        drop(ctx);
        drop(owner.begin(None).unwrap());
    }

    #[tokio::test]
    async fn duplicate_session_attachment_releases_consumed_builder_without_leasing_another_session() {
        let exec = execution();
        let first = session();
        let second = session();
        let pin = pin_parent(&exec, &first).await;
        for pinned in [false, true] {
            let builder = ExecuteRequest::new(request(Some(pin.response_id()), output("weather")), Arc::clone(&exec))
                .with_session(&first, Some(&pin))
                .unwrap();
            let result = builder.with_session(&second, pinned.then_some(&pin));
            assert!(matches!(result, Err(ExecutorError::InvalidRequest(message))
                if message == "request already has a response session"));
            drop(first.begin(None).unwrap());
            drop(second.begin(None).unwrap());
        }
    }
}
