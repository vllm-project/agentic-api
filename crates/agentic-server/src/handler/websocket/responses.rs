use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::http::HeaderMap;
use axum::response::Response;
use either::Either;
use futures::stream::SplitSink;
use futures::{Sink, SinkExt, Stream, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument as _, debug, warn};

use agentic_core::executor::{BoxStream, ExecuteRequest, ExecutorError, ResponseSession, ResponseSessionGroup};
use agentic_core::types::request_response::RequestPayload;

use super::super::common::extract_bearer;
use super::error::WsError;
use crate::app::AppState;
use crate::auth::AuthenticatedPrincipal;

mod event;
mod local;
use local::complete_without_inference;
mod telemetry;
use event::{StreamId, WsEventLimit, WsOutboundEvent};
#[cfg(test)]
use event::{WS_MAX_STREAM_ID_CHARS, WS_ROUTING_SLACK_BYTES, attach_stream_id, ws_routing_overhead};

type WsSender = SplitSink<WebSocket, Message>;

/// Outbound events queued ahead of the socket writer. Each entry is bounded by
/// the configured `max_stream_event_bytes`, so the queue holds at most
/// `WS_OUTBOUND_BUFFER * max_stream_event_bytes` serialized bytes.
const WS_OUTBOUND_BUFFER: usize = 64;
const WS_MAX_OUTSTANDING_REQUESTS: usize = 64;
const WS_MAX_OUTSTANDING_BYTES: usize = 12 * 1024 * 1024;
// Retained state is bounded separately from queued requests and outbound events.
// Limits cover serialized checkpoints, including pinned parents and replacements.
const WS_MAX_SESSION_LANES: usize = 128;
const WS_MAX_CHECKPOINT_ITEMS: usize = 32_768;
const WS_MAX_CHECKPOINT_BYTES: usize = 16 * 1024 * 1024;
const WS_MAX_RETAINED_BYTES: usize = 32 * 1024 * 1024;

struct WsRequest {
    payload: RequestPayload,
    stream_id: Option<StreamId>,
    generate: Option<bool>,
    execution: Option<telemetry::QueuedExecution>,
}

#[derive(Debug)]
struct WsRequestParseError {
    previous_response_id: Option<String>,
    error: WsError,
    stream_id: Option<StreamId>,
}

enum WsWorkItem {
    Execute {
        request: Box<WsRequest>,
        input_bytes: usize,
    },
    Reject {
        error: WsRequestParseError,
        input_bytes: usize,
    },
}

impl WsWorkItem {
    fn stream_id(&self) -> Option<&StreamId> {
        match self {
            Self::Execute { request, .. } => request.stream_id.as_ref(),
            Self::Reject { error, .. } => error.stream_id.as_ref(),
        }
    }

    fn lane(&self) -> Option<StreamId> {
        self.stream_id().cloned()
    }

    fn input_bytes(&self) -> usize {
        match self {
            Self::Execute { input_bytes, .. } | Self::Reject { input_bytes, .. } => *input_bytes,
        }
    }
}

struct WsAdmissionError {
    error: WsError,
    stream_id: Option<StreamId>,
}

struct RequestCompletion {
    lane: Option<StreamId>,
    input_bytes: usize,
    result: Result<(), WsError>,
}

#[derive(Default)]
struct WsByteBudget {
    used: usize,
}

impl WsByteBudget {
    fn can_reserve(&self, input_bytes: usize) -> bool {
        input_bytes <= WS_MAX_OUTSTANDING_BYTES.saturating_sub(self.used)
    }

    fn reserve(&mut self, input_bytes: usize) {
        debug_assert!(self.can_reserve(input_bytes));
        self.used += input_bytes;
    }

    fn release(&mut self, input_bytes: usize) {
        self.used = self.used.saturating_sub(input_bytes);
    }
}

struct WsMultiplexer {
    state: Arc<AppState>,
    auth: Option<String>,
    principal: Option<Arc<AuthenticatedPrincipal>>,
    outbound_tx: mpsc::Sender<WsOutboundEvent>,
    lanes: HashMap<Option<StreamId>, VecDeque<WsWorkItem>>,
    // Idle lanes retain their latest checkpoint until the connection closes.
    sessions: HashMap<Option<StreamId>, Arc<ResponseSession>>,
    session_group: ResponseSessionGroup,
    request_tasks: JoinSet<RequestCompletion>,
    queued_requests: usize,
    byte_budget: WsByteBudget,
    shutdown_token: CancellationToken,
}

impl WsMultiplexer {
    fn new(
        state: Arc<AppState>,
        auth: Option<String>,
        principal: Option<AuthenticatedPrincipal>,
        outbound_tx: mpsc::Sender<WsOutboundEvent>,
        shutdown_token: CancellationToken,
    ) -> Self {
        Self {
            state,
            auth,
            principal: principal.map(Arc::new),
            outbound_tx,
            lanes: HashMap::new(),
            sessions: HashMap::new(),
            session_group: ResponseSessionGroup::new(
                NonZeroUsize::new(WS_MAX_SESSION_LANES).expect("positive session limit"),
                NonZeroUsize::new(WS_MAX_CHECKPOINT_ITEMS).expect("positive item limit"),
                NonZeroUsize::new(WS_MAX_CHECKPOINT_BYTES).expect("positive checkpoint limit"),
                NonZeroUsize::new(WS_MAX_RETAINED_BYTES).expect("positive retention limit"),
            ),
            request_tasks: JoinSet::new(),
            queued_requests: 0,
            byte_budget: WsByteBudget::default(),
            shutdown_token,
        }
    }

    fn event_limit(&self) -> WsEventLimit {
        WsEventLimit::from_state(&self.state)
    }

    fn has_capacity_for(&self, input_bytes: usize) -> bool {
        self.request_tasks.len() + self.queued_requests < WS_MAX_OUTSTANDING_REQUESTS
            && self.byte_budget.can_reserve(input_bytes)
    }

    fn schedule(&mut self, mut work: WsWorkItem) -> Result<(), WsAdmissionError> {
        if !self.has_capacity_for(work.input_bytes()) {
            return Err(WsAdmissionError {
                error: WsError::TooManyRequests,
                stream_id: work.stream_id().cloned(),
            });
        }

        let lane = work.lane();
        if !self.sessions.contains_key(&lane) {
            if self.sessions.len() >= WS_MAX_SESSION_LANES {
                return Err(WsAdmissionError {
                    error: WsError::TooManyRequests,
                    stream_id: lane,
                });
            }
            let session = self.session_group.new_session().map_err(|error| WsAdmissionError {
                error: WsError::from(error),
                stream_id: lane.clone(),
            })?;
            self.sessions.insert(lane.clone(), Arc::new(session));
        }
        if let WsWorkItem::Execute { request, .. } = &mut work {
            request.execution = Some(telemetry::QueuedExecution::new(&self.state));
        }
        self.byte_budget.reserve(work.input_bytes());
        if let Some(queue) = self.lanes.get_mut(&lane) {
            queue.push_back(work);
            self.queued_requests += 1;
            debug!(stream_id = ?lane, queued_requests = queue.len(), "queued websocket request on active lane");
            return Ok(());
        }

        self.lanes.insert(lane.clone(), VecDeque::new());
        self.spawn(lane, work);
        Ok(())
    }

    fn schedule_next(&mut self, lane: Option<StreamId>) {
        let next = self.lanes.get_mut(&lane).and_then(VecDeque::pop_front);
        if let Some(work) = next {
            self.queued_requests -= 1;
            self.spawn(lane, work);
        } else {
            self.lanes.remove(&lane);
        }
    }

    fn discard_queued(&mut self) {
        let discarded_bytes = self
            .lanes
            .values()
            .flat_map(|queue| queue.iter())
            .map(WsWorkItem::input_bytes)
            .sum::<usize>();
        self.byte_budget.release(discarded_bytes);
        self.lanes.clear();
        self.queued_requests = 0;
    }

    fn finish(&mut self, completion: Result<RequestCompletion, tokio::task::JoinError>, draining: bool) -> bool {
        let completion = match completion {
            Ok(completion) => completion,
            Err(error) => {
                warn!(%error, "responses websocket request task failed");
                return false;
            }
        };
        self.byte_budget.release(completion.input_bytes);
        if let Err(error) = completion.result {
            warn!(%error, "responses websocket request failed without a client-visible event");
            return false;
        }
        if !draining && !self.shutdown_token.is_cancelled() {
            self.schedule_next(completion.lane);
        }
        true
    }

    fn reap_ready(&mut self, draining: bool) -> bool {
        while let Some(completion) = self.request_tasks.try_join_next() {
            if !self.finish(completion, draining) {
                return false;
            }
        }
        true
    }

    fn spawn(&mut self, lane: Option<StreamId>, work: WsWorkItem) {
        let state = Arc::clone(&self.state);
        let auth = self.auth.clone();
        let principal = self.principal.clone();
        let outbound_tx = self.outbound_tx.clone();
        let shutdown_token = self.shutdown_token.clone();
        let stream_id = work.stream_id().cloned();
        let input_bytes = work.input_bytes();
        let event_limit = self.event_limit();
        // schedule creates a session before admitting work; idle sessions survive schedule_next.
        let session = Arc::clone(self.sessions.get(&lane).expect("admitted lane has a session"));
        self.request_tasks.spawn(async move {
            // Admission may precede dispatch by an entire inference/tool round.
            // Recheck here for both new lanes and work dequeued by schedule_next.
            if let Some(event) = websocket_identity_error_event(principal.as_deref()) {
                return RequestCompletion {
                    lane,
                    input_bytes,
                    result: queue_ws_json(&outbound_tx, event, stream_id.as_ref(), event_limit).await,
                };
            }
            let result = match work {
                WsWorkItem::Execute { request, .. } => {
                    handle_ws_request(
                        *request,
                        &state,
                        auth,
                        &outbound_tx,
                        &shutdown_token,
                        &session,
                        event_limit,
                    )
                    .await
                }
                WsWorkItem::Reject { error, .. } => {
                    if let Some(parent) = error.previous_response_id.as_deref() {
                        session
                            .discard_cached_response(parent)
                            .map_err(WsError::from)
                            .and(Err(error.error))
                    } else {
                        Err(error.error)
                    }
                }
            };
            // Dropping a failed executor stream aborts its worker asynchronously.
            // Do not dispatch the next turn until its lease has been released.
            let result = match session.wait_until_idle().await {
                Ok(()) => result,
                Err(error) => Err(WsError::from(error)),
            };
            let result = match result {
                Ok(()) => Ok(()),
                Err(error) => queue_ws_error(&outbound_tx, error, stream_id.as_ref(), event_limit).await,
            };
            RequestCompletion {
                lane,
                input_bytes,
                result,
            }
        });
    }
}

pub async fn responses_ws(State(state): State<AppState>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    upgrade_responses_ws(state, headers, ws, None)
}

pub(crate) async fn responses_ws_with_auth(
    State(state): State<AppState>,
    principal: Option<Extension<AuthenticatedPrincipal>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    upgrade_responses_ws(state, headers, ws, principal.map(|Extension(principal)| principal))
}

fn upgrade_responses_ws(
    state: AppState,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    principal: Option<AuthenticatedPrincipal>,
) -> Response {
    let websocket_guard = state.websocket_tracker.track();
    // Read before `state` moves into the upgrade closure. Messages and frames
    // above this ceiling are rejected by the transport, which closes the
    // connection before any JSON request is parsed.
    let max_request_body_size = state.max_request_body_size.get();
    ws.max_message_size(max_request_body_size)
        .max_frame_size(max_request_body_size)
        .on_upgrade(move |socket| async move {
            let _websocket_guard = websocket_guard;
            Box::pin(responses_ws_loop(socket, state, headers, principal)).await;
        })
}

fn begin_ws_draining(multiplexer: &mut WsMultiplexer, draining: &mut bool) {
    *draining = true;
    multiplexer.discard_queued();
    debug!(
        active_streams = multiplexer.request_tasks.len(),
        "draining responses websocket session"
    );
}

#[tracing::instrument(name = "agentic.websocket.session", skip_all, parent = None)]
async fn responses_ws_loop(
    socket: WebSocket,
    state: AppState,
    headers: HeaderMap,
    principal: Option<AuthenticatedPrincipal>,
) {
    debug!("responses websocket session opened");
    let shutdown_token = state.shutdown_token.clone();
    let state = Arc::new(state);
    let (mut sender, mut receiver) = socket.split();
    let auth = extract_bearer(&headers, state.openai_api_key.as_deref());
    let (outbound_tx, mut outbound_rx) = mpsc::channel(WS_OUTBOUND_BUFFER);
    let mut multiplexer = WsMultiplexer::new(state, auth, principal, outbound_tx, shutdown_token.clone());
    let mut draining = false;
    let mut client_disconnected = false;

    loop {
        if shutdown_token.is_cancelled() && !draining {
            begin_ws_draining(&mut multiplexer, &mut draining);
        }
        if draining && multiplexer.request_tasks.is_empty() && outbound_rx.is_empty() {
            break;
        }

        tokio::select! {
            () = shutdown_token.cancelled(), if !draining => {
                begin_ws_draining(&mut multiplexer, &mut draining);
            }
            outbound = outbound_rx.recv() => {
                let Some(value) = outbound else {
                    continue;
                };
                if send_ws_event(&mut sender, value).await.is_err() {
                    client_disconnected = true;
                    break;
                }
            }
            completion = multiplexer.request_tasks.join_next(), if !multiplexer.request_tasks.is_empty() => {
                let Some(completion) = completion else {
                    continue;
                };
                if !multiplexer.finish(completion, draining) {
                    client_disconnected = true;
                    break;
                }
                if shutdown_token.is_cancelled() && !draining {
                    begin_ws_draining(&mut multiplexer, &mut draining);
                }
            }
            message = receiver.next() => {
                if shutdown_token.is_cancelled() && !draining {
                    begin_ws_draining(&mut multiplexer, &mut draining);
                    debug!("discarded websocket message received during shutdown");
                    continue;
                }
                let Some(message) = message else {
                    client_disconnected = true;
                    break;
                };
                match message {
                    Ok(message) => {
                        if !handle_ws_client_message(
                            message,
                            &mut sender,
                            &mut multiplexer,
                            &mut draining,
                        )
                        .await
                        {
                            client_disconnected = true;
                            break;
                        }
                    }
                    Err(error) => {
                        warn!(%error, "responses websocket receive error");
                        client_disconnected = true;
                        break;
                    }
                }
            }
        }
    }

    if client_disconnected {
        multiplexer.request_tasks.abort_all();
        while multiplexer.request_tasks.join_next().await.is_some() {}
        // Executor stream disposal aborts its nested inference worker. Wait for
        // every lease to release its pinned state before ending this connection.
        for session in multiplexer.sessions.values() {
            if let Err(error) = session.wait_until_idle().await {
                warn!(%error, "failed to await websocket continuation disposal");
            }
        }
    }
    drop(multiplexer);
    close_ws(&mut sender, &mut receiver).await;
    debug!("responses websocket session closed");
}

async fn handle_ws_client_message(
    message: Message,
    sender: &mut WsSender,
    multiplexer: &mut WsMultiplexer,
    draining: &mut bool,
) -> bool {
    match message {
        Message::Text(_) if *draining => {
            debug!("discarded websocket response.create during shutdown");
            true
        }
        Message::Text(text) => {
            if let Some(event) = websocket_identity_error_event(multiplexer.principal.as_deref()) {
                let stream_id = stream_id_from_text(&text);
                let send_succeeded = match WsOutboundEvent::new(event, stream_id.as_ref(), multiplexer.event_limit()) {
                    Ok(event) => send_ws_event(sender, event).await.is_ok(),
                    Err(error) => {
                        warn!(%error, "failed to build websocket identity error event");
                        false
                    }
                };
                *draining = true;
                multiplexer.discard_queued();
                return send_succeeded;
            }

            if !multiplexer.reap_ready(*draining) {
                return false;
            }
            if multiplexer.shutdown_token.is_cancelled() {
                begin_ws_draining(multiplexer, draining);
                debug!("discarded websocket response.create during shutdown");
                return true;
            }
            let input_bytes = text.len();
            if !multiplexer.has_capacity_for(input_bytes) {
                let stream_id = stream_id_from_text(&text);
                let limit = multiplexer.event_limit();
                return handle_ws_error(sender, WsError::TooManyRequests, stream_id.as_ref(), limit).await;
            }
            let work = match parse_ws_request(&text) {
                Ok(request) => WsWorkItem::Execute {
                    request: Box::new(request),
                    input_bytes,
                },
                Err(error) => WsWorkItem::Reject { error, input_bytes },
            };
            if multiplexer.shutdown_token.is_cancelled() {
                begin_ws_draining(multiplexer, draining);
                debug!("discarded websocket response.create during shutdown");
                return true;
            }
            let limit = multiplexer.event_limit();
            match multiplexer.schedule(work) {
                Ok(()) => true,
                Err(rejected) => handle_ws_error(sender, rejected.error, rejected.stream_id.as_ref(), limit).await,
            }
        }
        Message::Binary(_) if *draining => true,
        Message::Binary(_) => handle_ws_error(sender, WsError::BinaryFrame, None, multiplexer.event_limit()).await,
        Message::Close(_) => false,
        Message::Ping(payload) => sender.send(Message::Pong(payload)).await.is_ok(),
        Message::Pong(_) => true,
    }
}

fn stream_id_from_text(text: &str) -> Option<StreamId> {
    #[derive(Deserialize)]
    struct StreamIdEnvelope {
        stream_id: Option<StreamId>,
    }

    serde_json::from_str::<StreamIdEnvelope>(text).ok()?.stream_id
}

fn websocket_identity_error_event(principal: Option<&AuthenticatedPrincipal>) -> Option<Value> {
    principal.is_some_and(AuthenticatedPrincipal::is_expired).then(|| {
        serde_json::json!({
            "type": "error",
            "code": "invalid_token",
            "message": "OIDC bearer token expired",
            "param": null,
            "sequence_number": 0,
        })
    })
}

fn keep_if_running<T>(shutdown_token: &CancellationToken, value: T) -> Option<T> {
    (!shutdown_token.is_cancelled()).then_some(value)
}

async fn close_ws<Sender, Receiver, SendError, ReceiveError>(sender: &mut Sender, receiver: &mut Receiver)
where
    Sender: Sink<Message, Error = SendError> + Unpin,
    Receiver: Stream<Item = Result<Message, ReceiveError>> + Unpin,
    SendError: std::fmt::Display,
    ReceiveError: std::fmt::Display,
{
    if let Err(error) = sender.close().await {
        debug!(%error, "failed to send responses websocket close frame");
        return;
    }

    while let Some(message) = receiver.next().await {
        match message {
            Ok(Message::Close(_)) => break,
            Ok(Message::Text(_) | Message::Binary(_) | Message::Ping(_) | Message::Pong(_)) => {}
            Err(error) => {
                debug!(%error, "responses websocket close handshake receive failed");
                break;
            }
        }
    }
}

fn parse_ws_request(text: &str) -> Result<WsRequest, WsRequestParseError> {
    let value = serde_json::from_str::<Value>(text).map_err(|error| WsRequestParseError {
        error: WsError::InvalidJson(error),
        previous_response_id: None,
        stream_id: None,
    })?;
    let stream_id = value
        .get("stream_id")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| "stream_id must be a string".to_owned())
                .and_then(StreamId::try_from)
        })
        .transpose()
        .map_err(|error| WsRequestParseError {
            error: WsError::from(ExecutorError::InvalidRequest(error)),
            previous_response_id: None,
            stream_id: None,
        })?;

    if value.get("type").and_then(Value::as_str) != Some("response.create") {
        return Err(WsRequestParseError {
            error: WsError::UnexpectedType,
            previous_response_id: None,
            stream_id,
        });
    }

    // Only valid routing plus response.create may identify a checkpoint for eviction.
    // In particular, an explicit null/invalid stream_id must not target the default lane.
    let previous_response_id = value
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let generate = value.get("generate").and_then(Value::as_bool);
    let mut payload = serde_json::from_value::<RequestPayload>(value).map_err(|error| WsRequestParseError {
        error: WsError::from(ExecutorError::from(error)),
        previous_response_id,
        stream_id: stream_id.clone(),
    })?;
    let requested_stream = payload.stream;
    payload.stream = true;
    debug!(
        requested_stream,
        forced_stream = payload.stream,
        store = payload.store,
        has_previous_response_id = payload.previous_response_id.is_some(),
        has_conversation_id = payload.conversation_id.is_some(),
        stream_id = stream_id.as_ref().map(StreamId::as_str),
        ?generate,
        tools = payload.tools.as_ref().map_or(0, Vec::len),
        "accepted websocket response.create"
    );

    Ok(WsRequest {
        execution: None,
        payload,
        stream_id,
        generate,
    })
}

async fn handle_ws_request(
    request: WsRequest,
    state: &AppState,
    auth: Option<String>,
    outbound_tx: &mpsc::Sender<WsOutboundEvent>,
    shutdown_token: &CancellationToken,
    session: &ResponseSession,
    event_limit: WsEventLimit,
) -> Result<(), WsError> {
    let WsRequest {
        payload,
        stream_id,
        generate,
        execution,
    } = request;
    let mut execution = telemetry::dispatch(execution, state);

    if generate == Some(false) {
        debug!("handling non-generating websocket request locally");
        let result = complete_without_inference(outbound_tx, state, payload, stream_id.as_ref(), session, event_limit)
            .instrument(execution.span().clone())
            .await;
        telemetry::finish_local(&mut execution, &result);
        return result;
    }

    // The executor validates every frame, including the terminal
    // `response.completed`, against what this socket can deliver after routing
    // metadata is attached, and does so before persisting the response.
    let result = ExecuteRequest::new(payload, Arc::clone(&state.exec_ctx))
        .with_execution_span(execution)
        .with_auth(auth)
        .with_session(session)?
        .with_max_stream_event_bytes(event_limit.executor_limit(stream_id.as_ref()))
        .run()
        .await?;
    let Some(result) = keep_if_running(shutdown_token, result) else {
        debug!("discarded websocket response initialized during shutdown");
        return Ok(());
    };
    let Either::Right(stream) = result else {
        return Err(WsError::Executor(Box::new(ExecutorError::InvalidRequest(
            "websocket response.create must produce a stream".to_owned(),
        ))));
    };

    stream_ws_response(outbound_tx, stream, stream_id.as_ref(), event_limit).await
}

async fn stream_ws_response(
    outbound_tx: &mpsc::Sender<WsOutboundEvent>,
    mut stream: BoxStream,
    stream_id: Option<&StreamId>,
    event_limit: WsEventLimit,
) -> Result<(), WsError> {
    while let Some(line) = stream.next().await {
        forward_ws_stream_chunk(outbound_tx, &line, stream_id, event_limit).await?;
    }
    Ok(())
}

fn sse_json_data_lines(chunk: &str) -> impl Iterator<Item = &str> {
    chunk
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(str::trim)
        .filter(|data| *data != "[DONE]")
}

async fn forward_ws_stream_chunk(
    outbound_tx: &mpsc::Sender<WsOutboundEvent>,
    chunk: &str,
    stream_id: Option<&StreamId>,
    event_limit: WsEventLimit,
) -> Result<(), WsError> {
    for data in sse_json_data_lines(chunk) {
        let value = serde_json::from_str::<Value>(data)
            .map_err(ExecutorError::from)
            .map_err(WsError::from)?;
        queue_ws_json(outbound_tx, value, stream_id, event_limit).await?;
    }
    Ok(())
}

async fn queue_ws_json(
    outbound_tx: &mpsc::Sender<WsOutboundEvent>,
    value: Value,
    stream_id: Option<&StreamId>,
    event_limit: WsEventLimit,
) -> Result<(), WsError> {
    outbound_tx
        .send(WsOutboundEvent::new(value, stream_id, event_limit)?)
        .await
        .map_err(|_| WsError::SendFailed)
}

async fn queue_ws_error(
    outbound_tx: &mpsc::Sender<WsOutboundEvent>,
    err: WsError,
    stream_id: Option<&StreamId>,
    event_limit: WsEventLimit,
) -> Result<(), WsError> {
    let Some(frame) = err.to_ws_frame() else {
        return Err(err);
    };
    queue_ws_json(outbound_tx, frame, stream_id, event_limit).await
}

async fn handle_ws_error(
    sender: &mut WsSender,
    err: WsError,
    stream_id: Option<&StreamId>,
    event_limit: WsEventLimit,
) -> bool {
    match err {
        WsError::SendFailed => false,
        err => send_ws_error(sender, &err, stream_id, event_limit).await.is_ok(),
    }
}

async fn send_ws_error(
    sender: &mut WsSender,
    err: &WsError,
    stream_id: Option<&StreamId>,
    event_limit: WsEventLimit,
) -> Result<(), WsError> {
    let Some(frame) = err.to_ws_frame() else {
        return Err(WsError::SendFailed);
    };
    send_ws_event(sender, WsOutboundEvent::new(frame, stream_id, event_limit)?).await
}

async fn send_ws_event(sender: &mut WsSender, event: WsOutboundEvent) -> Result<(), WsError> {
    sender
        .send(Message::Text(event.0.into()))
        .await
        .map_err(|_| WsError::SendFailed)
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use axum::extract::ws::Message;
    use futures::{Sink, StreamExt, sink, stream};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::{
        StreamId, WS_MAX_OUTSTANDING_BYTES, WS_MAX_OUTSTANDING_REQUESTS, WS_MAX_STREAM_ID_CHARS,
        WS_ROUTING_SLACK_BYTES, WsByteBudget, WsError, WsEventLimit, WsOutboundEvent, attach_stream_id, close_ws,
        forward_ws_stream_chunk, keep_if_running, parse_ws_request, queue_ws_json, sse_json_data_lines,
        websocket_identity_error_event, ws_routing_overhead,
    };
    use crate::auth::AuthenticatedPrincipal;

    struct CloseErrorSink;

    #[tokio::test]
    async fn outbound_event_limit_counts_routing_metadata_and_json_escaping() {
        let limit = WsEventLimit(1024 * 1024);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        // The JSON envelope {"text":"","type":"test"} occupies 25 bytes.
        let event = json!({"text": "x".repeat(1024 * 1024 - 25), "type": "test"});
        queue_ws_json(&sender, event.clone(), None, limit)
            .await
            .expect("exact limit is accepted");
        assert_eq!(receiver.recv().await.unwrap().0.len(), 1024 * 1024);

        let stream_id = StreamId::try_from("🦀").unwrap();
        let chunk = format!("data: {event}\n\n");
        assert!(
            forward_ws_stream_chunk(&sender, &chunk, Some(&stream_id), limit)
                .await
                .is_err()
        );
        assert!(
            receiver.try_recv().is_err(),
            "routing metadata must count toward the limit"
        );

        let escaped = json!({"type": "test", "text": "\n".repeat(512 * 1024)});
        assert!(queue_ws_json(&sender, escaped, None, limit).await.is_err());
        assert!(
            receiver.try_recv().is_err(),
            "escaped JSON bytes must count toward the limit"
        );
    }

    #[test]
    fn executor_limit_reserves_the_exact_routing_member_plus_slack() {
        let limit = WsEventLimit(1024 * 1024);
        assert_eq!(limit.executor_limit(None), 1024 * 1024 - WS_ROUTING_SLACK_BYTES);

        // Any frame the executor admits must still fit once `stream_id` is attached,
        // for an ASCII id and for one whose JSON encoding is longer than its chars.
        for raw_id in ["lane-a", "quote\"d", "🦀"] {
            let stream_id = StreamId::try_from(raw_id).unwrap();
            let overhead = ws_routing_overhead(Some(&stream_id));
            let event = json!({"type": "test", "text": "x".repeat(limit.executor_limit(Some(&stream_id)) - 25)});
            let bare = serde_json::to_string(&event).unwrap().len();
            assert!(bare <= limit.executor_limit(Some(&stream_id)));
            let routed = WsOutboundEvent::new(event, Some(&stream_id), limit).expect("routed event fits");
            assert!(routed.0.len() <= limit.bytes());
            assert_eq!(
                routed.0.len() - bare,
                overhead - WS_ROUTING_SLACK_BYTES,
                "the member overhead is exact for {raw_id:?}"
            );
        }
    }

    #[test]
    fn sse_json_data_lines_accept_named_and_data_only_frames() {
        let chunk = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\"}\n\n",
            "data: [DONE]\n\n",
        );

        assert_eq!(
            sse_json_data_lines(chunk).collect::<Vec<_>>(),
            [r#"{"type":"response.completed"}"#]
        );
    }

    #[test]
    fn stream_id_must_contain_between_one_and_256_characters() {
        for invalid in [String::new(), "a".repeat(WS_MAX_STREAM_ID_CHARS + 1)] {
            let request = json!({
                "type": "response.create",
                "stream_id": invalid,
                "model": "test-model",
                "input": "hello"
            });
            assert!(parse_ws_request(&request.to_string()).is_err());
        }
        let null_request = json!({
            "type": "response.create",
            "stream_id": null,
            "model": "test-model",
            "input": "hello"
        });
        assert!(parse_ws_request(&null_request.to_string()).is_err());

        let maximum = "🦀".repeat(WS_MAX_STREAM_ID_CHARS);
        let request = json!({
            "type": "response.create",
            "stream_id": maximum,
            "model": "test-model",
            "input": "hello"
        });
        let parsed = parse_ws_request(&request.to_string()).expect("256-character stream_id should be valid");
        assert_eq!(parsed.stream_id.expect("stream_id").as_str(), maximum);
    }

    #[test]
    fn websocket_capacity_is_bounded_by_count_and_input_bytes() {
        assert_eq!(WS_MAX_OUTSTANDING_REQUESTS, 64);
        let mut budget = WsByteBudget::default();
        budget.reserve(WS_MAX_OUTSTANDING_BYTES - 1);
        assert!(budget.can_reserve(1));
        budget.reserve(1);
        assert!(!budget.can_reserve(1));
        budget.release(WS_MAX_OUTSTANDING_BYTES);
        assert!(budget.can_reserve(WS_MAX_OUTSTANDING_BYTES));
    }

    #[test]
    fn stream_id_attachment_normalizes_routing_metadata() {
        let requested = StreamId::try_from("requested".to_owned()).expect("valid stream ID");
        let tagged = attach_stream_id(
            json!({"type": "response.created", "stream_id": "spoofed"}),
            Some(&requested),
        )
        .expect("object event");
        assert_eq!(tagged["stream_id"], "requested");

        let untagged =
            attach_stream_id(json!({"type": "response.created", "stream_id": "spoofed"}), None).expect("object event");
        assert!(untagged.get("stream_id").is_none());
        assert!(attach_stream_id(json!(["response.created"]), Some(&requested)).is_err());
    }

    impl Sink<Message> for CloseErrorSink {
        type Error = &'static str;

        fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Err("close failed"))
        }
    }

    #[test]
    fn cancellation_after_request_setup_discards_unpolled_stream() {
        let shutdown_token = CancellationToken::new();
        shutdown_token.cancel();

        assert_eq!(keep_if_running(&shutdown_token, "unpolled stream"), None);
    }

    #[test]
    fn websocket_identity_expiry_uses_responses_error_event() {
        assert!(websocket_identity_error_event(None).is_none());
        let frame = websocket_identity_error_event(Some(&AuthenticatedPrincipal::expired_for_test()))
            .expect("expired-token error event");

        assert_eq!(
            frame,
            json!({
                "type": "error",
                "code": "invalid_token",
                "message": "OIDC bearer token expired",
                "param": null,
                "sequence_number": 0,
            })
        );

        let generic_frame = WsError::UnexpectedType
            .to_ws_frame()
            .expect("generic client-visible error frame");
        assert_eq!(generic_frame["status"], 400);
        assert_eq!(generic_frame["error"]["code"], "invalid_request_error");
    }

    #[tokio::test]
    async fn close_ws_ignores_late_frames_until_peer_close() {
        let mut sender = sink::drain();
        let mut receiver = stream::iter([
            Ok::<_, &'static str>(Message::Text("late request".into())),
            Ok(Message::Binary(vec![1].into())),
            Ok(Message::Close(None)),
            Err("must remain unread"),
        ]);

        close_ws(&mut sender, &mut receiver).await;

        assert!(matches!(receiver.next().await, Some(Err("must remain unread"))));
    }

    #[tokio::test]
    async fn close_ws_returns_without_reading_when_close_send_fails() {
        let mut sender = CloseErrorSink;
        let mut receiver = stream::iter([Ok::<_, &'static str>(Message::Close(None))]);

        close_ws(&mut sender, &mut receiver).await;

        assert!(matches!(receiver.next().await, Some(Ok(Message::Close(None)))));
    }

    #[tokio::test]
    async fn close_ws_stops_reading_after_receive_error() {
        let mut sender = sink::drain();
        let mut receiver = stream::iter([Err::<Message, _>("receive failed"), Ok(Message::Close(None))]);

        close_ws(&mut sender, &mut receiver).await;

        assert!(matches!(receiver.next().await, Some(Ok(Message::Close(None)))));
    }
}
