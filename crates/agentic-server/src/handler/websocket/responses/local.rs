//! Local completion of a non-generating WebSocket request.

use agentic_core::ResponseUsage;
use agentic_core::executor::{RequestContext, ResponseSession, persist_turn, rehydrate_in_session};
use agentic_core::types::request_response::RequestPayload;
use agentic_core::utils::common::utcnow_str;
use serde_json::Value;
use tokio::sync::mpsc;

use super::{StreamId, WsError, WsEventLimit, WsOutboundEvent};
use crate::app::AppState;

pub(super) async fn complete_without_inference(
    outbound_tx: &mpsc::Sender<WsOutboundEvent>,
    state: &AppState,
    payload: RequestPayload,
    stream_id: Option<&StreamId>,
    session: &ResponseSession,
    event_limit: WsEventLimit,
) -> Result<(), WsError> {
    let ctx = rehydrate_in_session(payload, &state.exec_ctx, session).await?;
    let created_at = utcnow_str();
    let created_event = empty_response_event(&ctx, created_at, "response.created", "in_progress", 0, None);
    let completed_event = empty_response_event(
        &ctx,
        created_at,
        "response.completed",
        "completed",
        1,
        Some(ResponseUsage::default()),
    );

    // Validate both lifecycle events, including routing metadata, before any
    // persistence or delivery. Completion metadata can exceed the limit even
    // when the created event fits.
    let created_event = WsOutboundEvent::new(created_event, stream_id, event_limit)?;
    let completed_event = WsOutboundEvent::new(completed_event, stream_id, event_limit)?;

    #[cfg(debug_assertions)]
    state.websocket_tracker.pause_local_completion_after_rehydration().await;
    persist_turn(
        ctx,
        Vec::new(),
        &state.exec_ctx.conv_handler,
        &state.exec_ctx.resp_handler,
    )
    .await?;

    outbound_tx.send(created_event).await.map_err(|_| WsError::SendFailed)?;
    outbound_tx.send(completed_event).await.map_err(|_| WsError::SendFailed)
}

fn empty_response_event(
    ctx: &RequestContext,
    created_at: i64,
    event_type: &str,
    status: &str,
    sequence_number: u32,
    usage: Option<ResponseUsage>,
) -> Value {
    serde_json::json!({
        "type": event_type,
        "sequence_number": sequence_number,
        "response": {
            "id": &ctx.response_id,
            "object": "response",
            "created_at": created_at,
            "model": &ctx.enriched_request.model,
            "status": status,
            "output": [],
            "usage": usage,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": &ctx.original_request.previous_response_id,
            "conversation_id": &ctx.conversation_id,
            "instructions": &ctx.enriched_request.instructions,
        },
    })
}
