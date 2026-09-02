//! Step 3 of the conversation pipeline — response persistence.
//!
//! Writes the completed response and output items to storage, routing to the
//! appropriate handler based on whether the turn belongs to a conversation.

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::modes::{ConversationHandler, ResponseHandler};
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::types::event::ResponseStatus;
use crate::types::io::OutputItem;
use crate::types::request_response::ResponsePayload;
use tracing::error;

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
        match persist_response(payload, ctx, conv_handler, resp_handler).await {
            Err(error @ ExecutorError::Conflict(_)) => Err(error),
            Err(source) => {
                error!(error = ?source, "failed to persist response");
                Err(ExecutorError::Persistence(Box::new(source)))
            }
            Ok(()) => Ok(()),
        }
    } else {
        Ok(())
    }
}

/// Step 3 — Persist the completed response to storage.
///
/// Skipped if [`ResponseStatus`] is not `Completed`/`Incomplete` or `payload.id` is empty.
/// Routes explicit `conversation_id` requests to [`ConversationHandler`] and
/// all other requests, including `previous_response_id` continuations, to [`ResponseHandler`].
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

    persist_turn(ctx, payload.output, &conv_handler, &resp_handler).await
}

/// Persists one completed turn with the handler selected by its explicit conversation discriminator.
///
/// # Errors
/// Returns [`ExecutorError`] if the selected storage operation fails.
pub async fn persist_turn(
    ctx: RequestContext,
    output_items: Vec<OutputItem>,
    conv_handler: &ConversationHandler,
    resp_handler: &ResponseHandler,
) -> ExecutorResult<()> {
    if ctx.original_request.conversation_id.is_some() {
        conv_handler.execute_turn(ctx, output_items).await
    } else {
        resp_handler.execute_turn(ctx, output_items).await
    }
}

/// Stores a decoded turn for a caller that ran inference itself. A failed turn is
/// returned unstored, as the in-process flow does.
///
/// # Errors
/// [`ExecutorError::InvalidRequest`] for an unusable id or unfinished response,
/// [`ExecutorError::Conflict`] for an id already stored, or a storage error.
pub async fn commit(
    ctx: RequestContext,
    payload: ResponsePayload,
    exec_ctx: &ExecutionContext,
) -> ExecutorResult<ResponsePayload> {
    if ctx.response_id.is_empty() {
        return Err(ExecutorError::InvalidRequest(
            "context has no reserved response id".to_owned(),
        ));
    }

    // Storing an unfinished turn would return an id that can never be continued.
    if payload.status.parse::<ResponseStatus>().unwrap_or_default() == ResponseStatus::InProgress {
        return Err(ExecutorError::InvalidRequest(format!(
            "upstream response status '{}' is not terminal",
            payload.status
        )));
    }

    persist_if_needed(
        payload.clone(),
        ctx,
        exec_ctx.conv_handler.clone(),
        exec_ctx.resp_handler.clone(),
    )
    .await?;
    Ok(payload)
}
