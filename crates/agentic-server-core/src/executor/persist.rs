//! Step 3 of the conversation pipeline — response persistence.
//!
//! Writes the completed response and output items to storage, routing to the
//! appropriate handler based on whether the turn belongs to a conversation.

use crate::executor::error::ExecutorResult;
use crate::executor::modes::{ConversationHandler, ResponseHandler};
use crate::executor::request::RequestContext;
use crate::types::event::ResponseStatus;
use crate::types::request_response::ResponsePayload;

#[must_use]
pub(crate) fn should_persist(ctx: &RequestContext) -> bool {
    ctx.original_request.store
        || ctx.original_request.previous_response_id.is_some()
        || ctx.original_request.conversation_id.is_some()
}

pub(crate) async fn persist_if_needed(
    payload: ResponsePayload,
    ctx: RequestContext,
    conv_handler: ConversationHandler,
    resp_handler: ResponseHandler,
) -> ExecutorResult<()> {
    if should_persist(&ctx) {
        persist_response(payload, ctx, conv_handler, resp_handler).await
    } else {
        Ok(())
    }
}

/// Step 3 — Persist the completed response to storage.
///
/// Skipped if [`ResponseStatus`] is not `Completed`/`Incomplete` or `payload.id` is empty.
/// Routes to [`ConversationHandler`] when `ctx.conversation_id` is set,
/// otherwise [`ResponseHandler`].
///
/// # Errors
/// Returns [`ExecutorError`] if the storage operation fails.
pub async fn persist_response(
    payload: ResponsePayload,
    ctx: RequestContext,
    conv_handler: ConversationHandler,
    resp_handler: ResponseHandler,
) -> ExecutorResult<()> {
    // Use typed enum — no hardcoded status strings.
    if !matches!(
        payload.status.parse::<ResponseStatus>().unwrap_or_default(),
        ResponseStatus::Completed | ResponseStatus::Incomplete
    ) || payload.id.is_empty()
    {
        return Ok(());
    }

    // Move output items from payload; handlers build ResponseMetadata from ctx internally.
    let output_items = payload.output;

    if ctx.conversation_id.is_some() {
        conv_handler.execute_turn(ctx, output_items).await
    } else {
        resp_handler.execute_turn(ctx, output_items).await
    }
}
