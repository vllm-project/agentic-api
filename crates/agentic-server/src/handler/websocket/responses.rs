use std::sync::Arc;

use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::http::HeaderMap;
use axum::response::Response;
#[cfg(test)]
use futures::SinkExt;
use futures::StreamExt;
#[cfg(test)]
use futures::{Sink, Stream};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument as _, debug, warn};

use agentic_core::executor::response_events::ResponseCommitState;
use agentic_core::executor::{BoxStream, ExecuteRequest, ExecutorError, ResponseSession};
use agentic_core::types::injection::ResponseInjectRequest;

use super::error::WsError;
use crate::app::AppState;
use crate::auth::AuthenticatedPrincipal;

mod connection;
mod control;
mod event;
mod local;
mod multiplexer;
mod request;
mod telemetry;

use connection::{begin_ws_draining, responses_ws_loop};
use control::Registration;
use event::{StreamId, WsEventLimit, WsOutboundEvent};
#[cfg(test)]
use event::{WS_MAX_STREAM_ID_CHARS, WS_ROUTING_SLACK_BYTES, attach_stream_id, ws_routing_overhead};
use local::complete_without_inference;
use multiplexer::{WsMultiplexer, WsWorkItem};
use request::{WsRequest, WsRequestParseError, is_injection, parse_ws_request, stream_id_from_text};

type WsSender = mpsc::Sender<WsOutboundEvent>;

/// Outbound events queued ahead of the socket writer. Each entry is bounded by
/// the configured `max_stream_event_bytes`, so the queue holds at most
/// `WS_OUTBOUND_BUFFER * max_stream_event_bytes` serialized bytes.
const WS_OUTBOUND_BUFFER: usize = 64;

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

fn handle_ws_client_message(
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
                let _ = match WsOutboundEvent::new(event, stream_id.as_ref(), multiplexer.event_limit()) {
                    Ok(event) => send_ws_event(sender, event).is_ok(),
                    Err(error) => {
                        warn!(%error, "failed to build websocket identity error event");
                        false
                    }
                };
                return false;
            }

            if !multiplexer.reap_ready(*draining) {
                return false;
            }
            if multiplexer.shutdown_token.is_cancelled() {
                begin_ws_draining(multiplexer, draining);
                debug!("discarded websocket response.create during shutdown");
                return true;
            }
            if is_injection(&text) {
                match serde_json::from_str::<ResponseInjectRequest>(&text) {
                    Ok(request) => {
                        if let Err(error) =
                            multiplexer
                                .controls
                                .dispatch(request, text.len(), multiplexer.principal.clone())
                        {
                            let _ = handle_ws_error(sender, error, None, multiplexer.event_limit());
                            return false;
                        }
                        return true;
                    }
                    Err(error) => {
                        // Injection schema errors are connection-fatal. Create
                        // validation retains the existing lane-ordered error behavior.
                        let _ = handle_ws_error(sender, WsError::InvalidJson(error), None, multiplexer.event_limit());
                        return false;
                    }
                }
            }
            let input_bytes = text.len();
            if !multiplexer.has_capacity_for(input_bytes) {
                let stream_id = stream_id_from_text(&text);
                let limit = multiplexer.event_limit();
                return handle_ws_error(sender, WsError::TooManyRequests, stream_id.as_ref(), limit);
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
                Err(rejected) => handle_ws_error(sender, rejected.error, rejected.stream_id.as_ref(), limit),
            }
        }
        Message::Binary(_) if *draining => true,
        Message::Binary(_) => handle_ws_error(sender, WsError::BinaryFrame, None, multiplexer.event_limit()),
        Message::Close(_) => false,
        // Axum queues the protocol pong automatically. A second application pong
        // races the independent writer and can duplicate the reply.
        Message::Ping(_) | Message::Pong(_) => true,
    }
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

#[cfg(test)]
fn keep_if_running<T>(shutdown_token: &CancellationToken, value: T) -> Option<T> {
    (!shutdown_token.is_cancelled()).then_some(value)
}

#[cfg(test)]
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

struct RequestExecution<'a> {
    state: &'a AppState,
    auth: Option<String>,
    outbound_tx: &'a mpsc::Sender<WsOutboundEvent>,
    disposal: &'a CancellationToken,
    session: &'a ResponseSession,
    event_limit: WsEventLimit,
    register: &'a mpsc::Sender<Registration>,
}

async fn handle_ws_request(request: WsRequest, context: RequestExecution<'_>) -> Result<(), WsError> {
    let RequestExecution {
        state,
        auth,
        outbound_tx,
        disposal,
        session,
        event_limit,
        register,
    } = context;
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
    let response = ExecuteRequest::new(payload, Arc::clone(&state.exec_ctx))
        .with_execution_span(execution)
        .with_auth(auth)
        .with_session(session)?
        .with_max_stream_event_bytes(event_limit.executor_limit(stream_id.as_ref()))
        .run_retained()
        .await?;
    let sink = response.sink.clone();
    let (reply, receive) = oneshot::channel();
    if let Err(rejected) = register
        .send(Registration {
            response,
            lane: stream_id,
            reply,
        })
        .await
    {
        let mut owner = rejected.0.response.owner;
        owner.cancel();
        let _ = owner.join().await;
        return Err(WsError::SendFailed);
    }
    let mut owner = receive.await.map_err(|_| WsError::SendFailed)?;
    let result = tokio::select! {
        result = owner.join() => result,
        () = disposal.cancelled() => {
            owner.cancel();
            let _ = owner.join().await;
            return Err(WsError::SendFailed);
        }
    };
    if sink.commit_state() == ResponseCommitState::Aborted {
        return Err(WsError::SendFailed);
    }
    sink.flush().await?;
    // Execution failures were delivered by the retained executor. They end this
    // response without closing the session or emitting a duplicate error.
    if let Err(error) = result {
        debug!(%error, "retained websocket response ended with an execution error");
    }
    Ok(())
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

fn handle_ws_error(
    sender: &mut WsSender,
    err: WsError,
    stream_id: Option<&StreamId>,
    event_limit: WsEventLimit,
) -> bool {
    match err {
        WsError::SendFailed => false,
        err => send_ws_error(sender, &err, stream_id, event_limit).is_ok(),
    }
}

fn send_ws_error(
    sender: &mut WsSender,
    err: &WsError,
    stream_id: Option<&StreamId>,
    event_limit: WsEventLimit,
) -> Result<(), WsError> {
    let Some(frame) = err.to_ws_frame() else {
        return Err(WsError::SendFailed);
    };
    send_ws_event(sender, WsOutboundEvent::new(frame, stream_id, event_limit)?)
}

fn send_ws_event(sender: &mut WsSender, event: WsOutboundEvent) -> Result<(), WsError> {
    // Reader-side admission never waits for a slow socket writer.
    sender.try_send(event).map_err(|_| WsError::SendFailed)
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
        StreamId, WS_MAX_STREAM_ID_CHARS, WS_ROUTING_SLACK_BYTES, WsError, WsEventLimit, WsOutboundEvent,
        attach_stream_id, close_ws, forward_ws_stream_chunk, keep_if_running, parse_ws_request, queue_ws_json,
        sse_json_data_lines, websocket_identity_error_event, ws_routing_overhead,
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
