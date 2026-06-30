use std::collections::VecDeque;
use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use agentic_core::executor::accumulator::ResponseAccumulator;
use agentic_core::executor::{
    ExecutionContext, ExecutorError, RequestContext, call_inference, persist_response, rehydrate_conversation,
};
use agentic_core::types::ResponsePayload;
use agentic_core::types::request_response::RequestPayload;
use agentic_core::utils::common::serialize_to_string;

use super::super::common::{MAX_BODY_SIZE, resolve_exec_ctx_from_headers};
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
    payload.stream = true;
    payload.store = true;

    let exec_ctx = resolve_exec_ctx_from_headers(state, headers);
    let ctx = rehydrate_conversation(payload, &exec_ctx).await?;
    let upstream_json =
        serialize_to_string(&ctx.enriched_request.to_upstream_request(true)).map_err(ExecutorError::from)?;

    stream_ws_response(sender, receiver, exec_ctx, ctx, upstream_json, shutdown_token, queue).await
}

/// Stream a response from the upstream LLM to the client.
///
/// Requests arriving from the client while the stream is active are pushed
/// onto `queue` so the caller can process them in order after this returns.
async fn stream_ws_response(
    sender: &mut WsSender,
    receiver: &mut WsReceiver,
    exec_ctx: Arc<ExecutionContext>,
    ctx: RequestContext,
    upstream_json: String,
    shutdown_token: &CancellationToken,
    queue: &mut VecDeque<String>,
) -> Result<(), WsError> {
    let should_persist = ctx.original_request.store
        || ctx.original_request.previous_response_id.is_some()
        || ctx.conversation_id.is_some();
    let mut lines = Vec::new();
    let mut stream = Box::pin(call_inference(
        upstream_json,
        exec_ctx.responses_url(),
        Arc::clone(&exec_ctx.client),
        exec_ctx.client_auth.clone(),
        exec_ctx.streaming_timeout,
    ));

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
        let line = match line {
            Ok(line) => line,
            Err(e) => return Err(WsError::Executor(e)),
        };
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            continue;
        }
        let mut value = match serde_json::from_str::<Value>(data) {
            Ok(value) => value,
            Err(e) => return Err(WsError::Executor(ExecutorError::from(e))),
        };
        apply_gateway_response_ids(&mut value, &ctx);
        send_ws_json(sender, value).await?;
        if should_persist {
            lines.push(line);
        }
    }

    if should_persist && !lines.is_empty() {
        let acc = ResponseAccumulator::from_sse_lines(lines, ctx.conversation_id.as_deref());
        let mut payload = acc.finalize(
            &ctx.enriched_request.model,
            ctx.original_request.previous_response_id.as_deref(),
            ctx.original_request.instructions.as_deref(),
        );
        apply_gateway_payload_ids(&mut payload, &ctx);
        let ch = exec_ctx.conv_handler.clone();
        let rh = exec_ctx.resp_handler.clone();
        if let Err(e) = persist_response(payload, ctx, ch, rh).await {
            warn!("persist failed: {e}");
        }
    }

    Ok(())
}

fn apply_gateway_response_ids(value: &mut Value, ctx: &RequestContext) {
    let Some(response) = value.get_mut("response").and_then(Value::as_object_mut) else {
        return;
    };
    response.insert("id".to_owned(), Value::String(ctx.response_id.clone()));
    if let Some(previous_response_id) = &ctx.original_request.previous_response_id {
        response.insert(
            "previous_response_id".to_owned(),
            Value::String(previous_response_id.clone()),
        );
    }
    if let Some(conversation_id) = &ctx.conversation_id {
        response.insert("conversation_id".to_owned(), Value::String(conversation_id.clone()));
    }
}

fn apply_gateway_payload_ids(payload: &mut ResponsePayload, ctx: &RequestContext) {
    payload.id.clone_from(&ctx.response_id);
    payload.conversation_id.clone_from(&ctx.conversation_id);
    payload
        .previous_response_id
        .clone_from(&ctx.original_request.previous_response_id);
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
