use std::collections::VecDeque;
use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use either::Either;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use agentic_core::executor::{BoxStream, ExecuteRequest, ExecutorError};
use agentic_core::types::request_response::RequestPayload;

use super::super::common::{MAX_BODY_SIZE, extract_bearer};
use super::error::WsError;
use crate::app::AppState;

type WsSender = SplitSink<WebSocket, Message>;
type WsReceiver = SplitStream<WebSocket>;

pub async fn responses_ws(State(state): State<AppState>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(MAX_BODY_SIZE)
        .max_frame_size(MAX_BODY_SIZE)
        .on_upgrade(move |socket| responses_ws_loop(socket, state, headers))
}

async fn responses_ws_loop(socket: WebSocket, state: AppState, headers: HeaderMap) {
    debug!("responses websocket session opened");
    let shutdown_token = state.shutdown_token.clone();
    let (mut sender, mut receiver) = socket.split();

    // Requests received while a stream is active, processed in order after it completes.
    let mut queue: VecDeque<String> = VecDeque::new();

    loop {
        let text = if let Some(buffered) = queue.pop_front() {
            buffered
        } else {
            let message = tokio::select! {
                () = shutdown_token.cancelled() => break,
                message = receiver.next() => message,
            };

            let Some(message) = message else {
                break;
            };

            match message {
                Ok(Message::Text(text)) => text.to_string(),
                Ok(Message::Binary(_)) => {
                    if !handle_ws_error(&mut sender, WsError::BinaryFrame).await {
                        break;
                    }
                    continue;
                }
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(payload)) => {
                    if sender.send(Message::Pong(payload)).await.is_err() {
                        break;
                    }
                    continue;
                }
                Ok(Message::Pong(_)) => continue,
                Err(e) => {
                    warn!("responses websocket receive error: {e}");
                    break;
                }
            }
        };

        match handle_ws_text(
            &mut sender,
            &mut receiver,
            &state,
            &headers,
            &text,
            &shutdown_token,
            &mut queue,
        )
        .await
        {
            Ok(()) => {}
            Err(err) => {
                if !handle_ws_error(&mut sender, err).await {
                    break;
                }
            }
        }
    }
    debug!("responses websocket session closed");
}

/// Process one `response.create` message.
///
/// Any requests received from the client while the stream is active are
/// pushed onto `queue` and processed by the caller in order after this returns.
async fn handle_ws_text(
    sender: &mut WsSender,
    receiver: &mut WsReceiver,
    state: &AppState,
    headers: &HeaderMap,
    text: &str,
    shutdown_token: &CancellationToken,
    queue: &mut VecDeque<String>,
) -> Result<(), WsError> {
    let value = serde_json::from_str::<Value>(text).map_err(WsError::InvalidJson)?;

    if value.get("type").and_then(Value::as_str) != Some("response.create") {
        return Err(WsError::UnexpectedType);
    }

    let mut payload = serde_json::from_value::<RequestPayload>(value).map_err(ExecutorError::from)?;
    let requested_stream = payload.stream;
    let requested_store = payload.store;
    payload.stream = true;
    payload.store = true;
    debug!(
        requested_stream,
        requested_store,
        forced_stream = payload.stream,
        forced_store = payload.store,
        has_previous_response_id = payload.previous_response_id.is_some(),
        has_conversation_id = payload.conversation_id.is_some(),
        tools = payload.tools.as_ref().map_or(0, Vec::len),
        "accepted websocket response.create"
    );

    let auth = extract_bearer(headers, state.openai_api_key.as_deref());
    let result = ExecuteRequest::new(payload, Arc::clone(&state.exec_ctx))
        .with_auth(auth)
        .run()
        .await?;
    let Either::Right(stream) = result else {
        return Err(WsError::Executor(ExecutorError::InvalidRequest(
            "websocket response.create must produce a stream".to_owned(),
        )));
    };

    stream_ws_response(sender, receiver, stream, shutdown_token, queue).await
}

/// Stream a response from the executor to the client.
///
/// Requests arriving from the client while the stream is active are pushed
/// onto `queue` so the caller can process them in order after this returns.
async fn stream_ws_response(
    sender: &mut WsSender,
    receiver: &mut WsReceiver,
    mut stream: BoxStream,
    shutdown_token: &CancellationToken,
    queue: &mut VecDeque<String>,
) -> Result<(), WsError> {
    'stream: loop {
        let next_line = tokio::select! {
            () = shutdown_token.cancelled() => return Err(WsError::Shutdown),
            message = receiver.next() => {
                match message {
                    None | Some(Ok(Message::Close(_))) => return Err(WsError::ClientDisconnected),
                    Some(Ok(Message::Ping(payload))) => {
                        sender.send(Message::Pong(payload)).await.map_err(|_| WsError::SendFailed)?;
                        continue 'stream;
                    }
                    Some(Ok(Message::Pong(_))) => continue 'stream,
                    Some(Ok(Message::Binary(_))) => return Err(WsError::BinaryFrame),
                    Some(Ok(Message::Text(text))) => {
                        // Client pipelined the next request while we are still streaming.
                        // Enqueue it and keep draining the current stream.
                        queue.push_back(text.to_string());
                        debug!(
                            queued_requests = queue.len(),
                            "queued pipelined websocket response.create while stream is active"
                        );
                        continue 'stream;
                    }
                    Some(Err(e)) => return Err(WsError::Receive(e.to_string())),
                }
            }
            line = stream.next() => line,
        };
        let Some(line) = next_line else {
            break;
        };
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            continue;
        }
        let value = match serde_json::from_str::<Value>(data) {
            Ok(value) => value,
            Err(e) => return Err(WsError::Executor(ExecutorError::from(e))),
        };
        send_ws_json(sender, value).await?;
    }

    Ok(())
}

async fn handle_ws_error(sender: &mut WsSender, err: WsError) -> bool {
    match err {
        WsError::Shutdown | WsError::ClientDisconnected | WsError::SendFailed => false,
        WsError::Receive(message) => {
            warn!("responses websocket receive error: {message}");
            false
        }
        err => send_ws_error(sender, &err).await.is_ok(),
    }
}

async fn send_ws_error(sender: &mut WsSender, err: &WsError) -> Result<(), WsError> {
    let Some(frame) = err.to_ws_frame() else {
        return Err(WsError::SendFailed);
    };
    send_ws_json(sender, frame).await
}

async fn send_ws_json(sender: &mut WsSender, value: Value) -> Result<(), WsError> {
    let text = serde_json::to_string(&value).map_err(WsError::SerializeJson)?;
    sender
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| WsError::SendFailed)
}
