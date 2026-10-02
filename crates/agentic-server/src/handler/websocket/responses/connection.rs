//! Socket reading, ordered writing, and connection shutdown.
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::http::HeaderMap;
use futures::{SinkExt, StreamExt, stream::SplitSink};
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinHandle},
};
use tracing::{debug, warn};

use super::{WS_OUTBOUND_BUFFER, WsError, WsMultiplexer, WsOutboundEvent, WsSender, handle_ws_client_message};
use crate::app::AppState;
use crate::auth::AuthenticatedPrincipal;
use crate::handler::common::extract_bearer;

pub(super) fn begin_ws_draining(multiplexer: &mut WsMultiplexer, draining: &mut bool) {
    *draining = true;
    multiplexer.discard_queued();
    multiplexer.controls.cancel_live_controls();
    debug!(
        active_streams = multiplexer.request_tasks.len(),
        "draining responses websocket session"
    );
}

#[tracing::instrument(name = "agentic.websocket.session", skip_all, parent = None)]
pub(super) async fn responses_ws_loop(
    socket: WebSocket,
    state: AppState,
    headers: HeaderMap,
    principal: Option<AuthenticatedPrincipal>,
) {
    let shutdown_token = state.shutdown_token.clone();
    let state = Arc::new(state);
    let (socket_sender, mut receiver) = socket.split();
    let auth = extract_bearer(&headers, state.openai_api_key.as_deref());
    let (outbound_tx, outbound_rx) = mpsc::channel(WS_OUTBOUND_BUFFER);
    let mut sender = outbound_tx.clone();
    let mut writer = spawn_writer(socket_sender, outbound_rx);
    let mut writer_finished = false;
    let mut expiry = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut multiplexer = WsMultiplexer::new(state, auth, principal, outbound_tx, shutdown_token.clone());
    let mut draining = false;
    let mut client_disconnected = false;

    loop {
        if shutdown_token.is_cancelled() && !draining {
            begin_ws_draining(&mut multiplexer, &mut draining);
        }
        if draining && multiplexer.request_tasks.is_empty() && multiplexer.controls.tasks.is_empty() {
            break;
        }

        tokio::select! {
            () = shutdown_token.cancelled(), if !draining => {
                begin_ws_draining(&mut multiplexer, &mut draining);
            }
            result = &mut writer => {
                log_writer_completion(result);
                writer_finished = true;
                client_disconnected = true;
                break;
            }
            _ = expiry.tick() => multiplexer.controls.expire(),
            registration = multiplexer.controls.registrations.recv() => {
                if let Some(registration) = registration {
                    if multiplexer.controls.register(registration).is_err() { client_disconnected = true; break; }
                    if draining { multiplexer.controls.cancel_live_controls(); }
                }
            }
            completion = multiplexer.controls.tasks.join_next(), if !multiplexer.controls.tasks.is_empty() => {
                if let Some(completion) = completion {
                    if !multiplexer.controls.finish(completion) { client_disconnected = true; break; }
                }
            }
            Some(relay) = multiplexer.controls.relays.join_next(), if !multiplexer.controls.relays.is_empty() => {
                if !relay_completed(relay) { client_disconnected = true; break; }
            }
            completion = multiplexer.request_tasks.join_next(), if !multiplexer.request_tasks.is_empty() => {
                let Some(completion) = completion else {
                    continue;
                };
                if !multiplexer.finish(completion, draining) {
                    client_disconnected = true;
                    break;
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

    finish_ws_connection(multiplexer, sender, writer, writer_finished, client_disconnected).await;
}

async fn finish_ws_connection(
    mut multiplexer: WsMultiplexer,
    sender: WsSender,
    mut writer: tokio::task::JoinHandle<Result<(), WsError>>,
    writer_finished: bool,
    client_disconnected: bool,
) {
    multiplexer.disposal.cancel();
    multiplexer.controls.shutdown().await;
    if client_disconnected {
        while let Some(completion) = multiplexer.request_tasks.join_next().await {
            if let Err(error) = completion {
                warn!(%error, "responses websocket request task failed during disposal");
            }
        }
        // Executor stream disposal aborts its nested inference worker. Wait for
        // every lease to release its pinned state before ending this connection.
        for session in multiplexer.sessions.values() {
            if let Err(error) = session.wait_until_idle().await {
                warn!(%error, "failed to await websocket continuation disposal");
            }
        }
    }
    drop(multiplexer);
    drop(sender);
    if !writer_finished {
        if let Ok(result) = tokio::time::timeout(std::time::Duration::from_secs(15), &mut writer).await {
            log_writer_completion(result);
        } else {
            writer.abort();
            log_writer_completion(writer.await);
        }
    }
    debug!("responses websocket session closed");
}

fn log_writer_completion(result: Result<Result<(), WsError>, tokio::task::JoinError>) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => debug!(%error, "responses websocket writer failed"),
        Err(error) if error.is_cancelled() => debug!("responses websocket writer cancelled"),
        Err(error) => warn!(%error, "responses websocket writer task failed"),
    }
}

fn spawn_writer(
    mut socket_sender: SplitSink<WebSocket, Message>,
    mut outbound_rx: mpsc::Receiver<WsOutboundEvent>,
) -> JoinHandle<Result<(), WsError>> {
    tokio::spawn(async move {
        while let Some(event) = outbound_rx.recv().await {
            let event: WsOutboundEvent = event;
            let message = Message::Text(event.0.into());
            tokio::time::timeout(std::time::Duration::from_secs(10), socket_sender.send(message))
                .await
                .map_err(|_| WsError::SendFailed)?
                .map_err(|_| WsError::SendFailed)?;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), socket_sender.close())
            .await
            .map_err(|_| WsError::SendFailed)?
            .map_err(|_| WsError::SendFailed)?;
        Ok::<_, WsError>(())
    })
}

fn relay_completed(result: Result<Result<(), WsError>, JoinError>) -> bool {
    match result {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            debug!(%error, "responses websocket relay failed");
            false
        }
        Err(error) => {
            warn!(%error, "responses websocket relay task failed");
            false
        }
    }
}
