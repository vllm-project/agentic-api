//! Step 3 of the conversation pipeline — response persistence.
//!
//! Writes the completed response and output items to storage, routing to the
//! appropriate handler based on whether the turn belongs to a conversation.

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::modes::{ConversationHandler, ResponseHandler};
use crate::executor::prepare::prepare_request_tools;
use crate::executor::request::RequestContext;
use crate::storage::ResponseMetadata;
use crate::tool::ToolRegistry;
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
    registry: ToolRegistry,
    conv_handler: ConversationHandler,
    resp_handler: ResponseHandler,
) -> ExecutorResult<()> {
    if should_persist(&ctx) {
        persist_prepared_response(payload, ctx, registry, conv_handler, resp_handler)
            .await
            .map_err(|source| {
                error!(error = ?source, "failed to persist response");
                ExecutorError::Persistence(Box::new(source))
            })
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

    let (ctx, registry) = prepare_request_tools(ctx, &conv_handler, &resp_handler).await?;
    persist_prepared_turn(ctx, registry, payload.output, &conv_handler, &resp_handler).await
}

async fn persist_prepared_response(
    payload: ResponsePayload,
    ctx: RequestContext,
    registry: ToolRegistry,
    conv_handler: ConversationHandler,
    resp_handler: ResponseHandler,
) -> ExecutorResult<()> {
    if !matches!(
        payload.status.parse::<ResponseStatus>().unwrap_or_default(),
        ResponseStatus::Completed | ResponseStatus::Incomplete
    ) || payload.id.is_empty()
    {
        return Ok(());
    }

    persist_prepared_turn(ctx, registry, payload.output, &conv_handler, &resp_handler).await
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
    let (ctx, registry) = prepare_request_tools(ctx, conv_handler, resp_handler).await?;
    persist_prepared_turn(ctx, registry, output_items, conv_handler, resp_handler).await
}

pub(crate) async fn persist_prepared_turn(
    mut ctx: RequestContext,
    mut registry: ToolRegistry,
    output_items: Vec<OutputItem>,
    conv_handler: &ConversationHandler,
    resp_handler: &ResponseHandler,
) -> ExecutorResult<()> {
    let public_metadata = registry.take_tool_search_metadata();
    let mut metadata = ResponseMetadata {
        model: std::mem::take(&mut ctx.enriched_request.model),
        previous_response_id: ctx.original_request.previous_response_id.take(),
        effective_tools: ctx.enriched_request.tools.take(),
        tool_search_loaded_tools: None,
        effective_tool_choice: ctx.enriched_request.tool_choice.take().unwrap_or_default(),
        effective_instructions: ctx.enriched_request.instructions.take(),
    };
    if let Some((effective_tools, loaded_tools)) = public_metadata {
        metadata.effective_tools = effective_tools;
        metadata.tool_search_loaded_tools = Some(loaded_tools);
    }
    if ctx.original_request.conversation_id.is_some() {
        conv_handler
            .execute_turn_with_metadata(ctx, output_items, metadata)
            .await
    } else {
        resp_handler
            .execute_turn_with_metadata(ctx, output_items, metadata)
            .await
    }
}
