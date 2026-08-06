//! Step 1 of the conversation pipeline — history rehydration.
//!
//! Builds a [`RequestContext`] by loading prior turns from storage and
//! injecting them into the enriched request before it is forwarded to the LLM.

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::storage::InOutItem;
use crate::types::io::{InputItem, ResponsesInput, resolve_tool_choice, resolve_tools};
use crate::types::request_response::RequestPayload;
use crate::utils::uuid7_str;

/// Step 1 — Build [`RequestContext`] by rehydrating conversation history.
///
/// `request` is moved into the context as `enriched_request`; one clone is taken
/// for `original_request` so the engine retains an unmodified copy for persistence
/// and ID resolution.
///
/// Dispatches based on `store` flag and which ID is present:
/// - `previous_response_id`: rehydrate from the prior response checkpoint
/// - `conversation_id`:      rehydrate from the conversation
/// - no ids:                 forward only the new input
///
/// # Errors
/// Returns [`ExecutorError`] if storage is unavailable or a referenced ID does not exist.
pub async fn rehydrate_conversation(
    request: RequestPayload,
    exec_ctx: &ExecutionContext,
) -> ExecutorResult<RequestContext> {
    rehydrate_conversation_for_tenant(request, exec_ctx, None).await
}

/// Builds a request context and scopes stored-state lookup to `tenant_id`.
///
/// # Errors
///
/// Returns [`ExecutorError::InvalidRequest`] if both `conversation` and
/// `previous_response_id` are supplied. Propagates storage errors when the selected
/// state does not exist in the tenant scope or cannot be loaded.
pub async fn rehydrate_conversation_for_tenant(
    request: RequestPayload,
    exec_ctx: &ExecutionContext,
    tenant_id: Option<String>,
) -> ExecutorResult<RequestContext> {
    let response_id = uuid7_str("resp_");
    let new_input_items: Vec<InputItem> = Vec::from(&request.input);

    // One clone for the unmodified original; `request` is moved as enriched_request.
    let original_request = request.clone();
    let mut ctx = RequestContext {
        enriched_request: request,
        original_request,
        new_input_items,
        response_id,
        conversation_id: None,
        conversation_version: None,
        tenant_id,
    };

    if ctx.original_request.conversation_id.is_some() && ctx.original_request.previous_response_id.is_some() {
        return Err(ExecutorError::InvalidRequest(
            "provide only one of conversation or previous_response_id".into(),
        ));
    }

    if ctx.original_request.conversation_id.is_some() {
        from_conversation(&mut ctx, exec_ctx).await?;
        return Ok(ctx);
    }

    if ctx.original_request.previous_response_id.is_some() {
        from_response(&mut ctx, exec_ctx).await?;
        return Ok(ctx);
    }

    ctx.enriched_request.input = ResponsesInput::Items(ctx.new_input_items.clone());
    Ok(ctx)
}

/// Hydrates `ctx` from the previous response chain.
///
/// Loads the stored response, rehydrates its history items, resolves effective
/// tools and tool choice from the stored metadata, and prepends the history to
/// the enriched request input.
async fn from_response(ctx: &mut RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<()> {
    let stored = exec_ctx.resp_handler.get(ctx).await?;
    let history = exec_ctx.resp_handler.rehydrate(ctx).await?;

    let mut items = InOutItem::into_input_items(history);
    items.reserve(ctx.new_input_items.len());
    items.extend(ctx.new_input_items.iter().cloned());

    ctx.enriched_request.previous_response_id = None;
    ctx.enriched_request.input = ResponsesInput::Items(items);
    ctx.enriched_request.tools = resolve_tools(
        ctx.original_request.tools.as_deref(),
        stored.metadata.effective_tools.as_deref(),
        ctx.original_request.tools.is_some(),
    );
    ctx.enriched_request.tool_choice = Some(resolve_tool_choice(
        ctx.original_request.tool_choice.as_ref(),
        &stored.metadata.effective_tool_choice,
        ctx.original_request.tool_choice.is_some(),
    ));
    ctx.conversation_id = stored.conversation_id;
    Ok(())
}

/// Hydrates `ctx` from the conversation store.
///
/// history in parallel, then prepends the history items to the enriched request input.
async fn from_conversation(ctx: &mut RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<()> {
    let (conv_data, snapshot) = tokio::try_join!(
        async { exec_ctx.conv_handler.get(ctx).await },
        exec_ctx.conv_handler.rehydrate_snapshot(ctx),
    )?;

    let mut items = InOutItem::into_input_items(snapshot.items);
    items.reserve(ctx.new_input_items.len());
    items.extend(ctx.new_input_items.iter().cloned());

    ctx.enriched_request.input = ResponsesInput::Items(items);
    ctx.conversation_id = Some(conv_data.conversation_id);
    ctx.conversation_version = Some(snapshot.version);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::executor::modes::{ConversationHandler, ResponseHandler};
    use crate::storage::{
        ConversationStore, ConversationVersion, InOutItem, ResponseMetadata, ResponseStore, create_pool_with_schema,
    };
    use crate::types::request_response::RequestPayload;

    fn request(conversation_id: Option<&str>, previous_response_id: Option<&str>) -> RequestPayload {
        RequestPayload {
            model: "test".into(),
            input: ResponsesInput::Text("new input".into()),
            instructions: None,
            previous_response_id: previous_response_id.map(str::to_owned),
            conversation_id: conversation_id.map(str::to_owned),
            tools: None,
            tool_choice: None,
            stream: false,
            store: true,
            include: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            metadata: None,
            parallel_tool_calls: None,
            cache_salt: None,
            context_management: None,
        }
    }

    fn execution_context(conversation_store: ConversationStore, response_store: ResponseStore) -> ExecutionContext {
        ExecutionContext::new(
            ConversationHandler::new(conversation_store),
            ResponseHandler::new(response_store),
            Arc::new(reqwest::Client::new()),
            "http://localhost:8000".to_owned(),
        )
    }

    #[tokio::test]
    async fn new_conversation_rehydration_captures_empty_version() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let conversation_store = ConversationStore::new(pool);
        let conversation = conversation_store.create().await?;
        let exec_ctx = execution_context(conversation_store, ResponseStore::disabled());

        let ctx = rehydrate_conversation(request(Some(&conversation.conversation_id), None), &exec_ctx).await?;

        assert_eq!(ctx.conversation_version, Some(ConversationVersion::Empty));
        Ok(())
    }

    #[tokio::test]
    async fn existing_conversation_rehydration_captures_last_sequence() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let conversation_store = ConversationStore::new(pool);
        let conversation = conversation_store.create().await?;
        let prior_items = Vec::<InputItem>::from(&ResponsesInput::Text("prior input".into()))
            .into_iter()
            .map(InOutItem::Input)
            .collect();
        conversation_store
            .persist(
                &conversation.conversation_id,
                "resp_prior",
                None,
                prior_items,
                &ResponseMetadata::default(),
            )
            .await?;
        let exec_ctx = execution_context(conversation_store, ResponseStore::disabled());

        let ctx = rehydrate_conversation(request(Some(&conversation.conversation_id), None), &exec_ctx).await?;

        assert_eq!(ctx.conversation_version, Some(ConversationVersion::LastSequence(0)));
        Ok(())
    }

    #[tokio::test]
    async fn request_without_continuation_has_no_conversation_version() -> Result<(), Box<dyn std::error::Error>> {
        let exec_ctx = execution_context(ConversationStore::disabled(), ResponseStore::disabled());

        let ctx = rehydrate_conversation(request(None, None), &exec_ctx).await?;

        assert_eq!(ctx.conversation_version, None);
        Ok(())
    }

    #[tokio::test]
    async fn previous_response_rehydration_has_no_conversation_version() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let response_store = ResponseStore::new(pool);
        response_store
            .persist("resp_prior", None, Vec::new(), &ResponseMetadata::default())
            .await?;
        let exec_ctx = execution_context(ConversationStore::disabled(), response_store);

        let ctx = rehydrate_conversation(request(None, Some("resp_prior")), &exec_ctx).await?;

        assert_eq!(ctx.conversation_version, None);
        Ok(())
    }
}
