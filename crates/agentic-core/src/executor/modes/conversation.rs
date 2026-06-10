//! Conversation storage handler — owns all conversation store operations.

use crate::storage::{ConversationData, ConversationStore, InOutItem, ResponseMetadata};
use crate::types::io::OutputItem;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::request::RequestContext;

/// Handles all conversation store operations: creation, rehydration, and persistence.
#[derive(Clone, Debug)]
pub struct ConversationHandler {
    store: ConversationStore,
}

impl ConversationHandler {
    #[must_use]
    pub fn new(store: ConversationStore) -> Self {
        Self { store }
    }

    /// Gets an existing conversation or creates one.
    ///
    /// Reads `conversation_id` from `ctx.original_request`.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation_id` is absent, the store is
    /// disabled, or the database query fails.
    pub async fn get_or_create(&self, ctx: &RequestContext) -> ExecutorResult<ConversationData> {
        let conv_id = ctx
            .original_request
            .conversation_id
            .as_deref()
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation_id is required for get_or_create".into()))?;
        self.store.get_or_create(conv_id).await.map_err(ExecutorError::Storage)
    }

    /// Gets an existing conversation.
    ///
    /// Reads `conversation_id` from `ctx.original_request`.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation_id` is absent, the store is
    /// disabled, the conversation does not exist, or the database query fails.
    pub async fn get(&self, ctx: &RequestContext) -> ExecutorResult<ConversationData> {
        let conv_id = ctx
            .original_request
            .conversation_id
            .as_deref()
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation_id is required for get".into()))?;
        self.store.get(conv_id).await.map_err(ExecutorError::Storage)
    }

    /// Creates a brand-new conversation with a freshly generated ID.
    ///
    /// # Errors
    /// Returns `ExecutorError` if the store is disabled or the database query fails.
    pub async fn create(&self) -> ExecutorResult<ConversationData> {
        self.store.create().await.map_err(ExecutorError::Storage)
    }

    /// Loads all history items for the conversation referenced by the request.
    ///
    /// Reads `conversation_id` from `ctx.original_request`. Returns an empty vec
    /// if the conversation exists but has no items yet.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation_id` is absent, the store is
    /// disabled, or the database query fails.
    pub async fn rehydrate(&self, ctx: &RequestContext) -> ExecutorResult<Vec<InOutItem>> {
        let conv_id = ctx
            .original_request
            .conversation_id
            .as_deref()
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation_id is required for rehydrate".into()))?;
        self.store.rehydrate(conv_id).await.map_err(ExecutorError::Storage)
    }

    /// Persists one conversation turn — only the new items from this turn.
    ///
    /// Takes `ctx` and `output_items` by value so fields can be moved directly
    /// into [`ResponseMetadata`] without cloning. The store tracks sequence
    /// numbers and appends, so prior history must not be re-inserted.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation_id` is absent on the context,
    /// the store is disabled, or the database operation fails.
    pub async fn execute_turn(&self, ctx: RequestContext, output_items: Vec<OutputItem>) -> ExecutorResult<()> {
        let conversation_id = ctx
            .conversation_id
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation_id is required for execute_turn".into()))?;

        let metadata = ResponseMetadata {
            model: ctx.enriched_request.model,
            previous_response_id: ctx.original_request.previous_response_id,
            effective_tools: ctx.original_request.tools,
            effective_tool_choice: ctx.original_request.tool_choice,
            effective_instructions: ctx.original_request.instructions,
        };

        let mut new_items = Vec::with_capacity(ctx.new_input_items.len() + output_items.len());
        new_items.extend(ctx.new_input_items.into_iter().map(InOutItem::Input));
        new_items.extend(output_items.into_iter().map(InOutItem::Output));

        self.store
            .persist(
                &conversation_id,
                &ctx.response_id,
                metadata.previous_response_id.as_deref(),
                new_items,
                &metadata,
            )
            .await
            .map_err(ExecutorError::Storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::{ResponsesInput, ToolChoice};
    use crate::types::request_response::RequestPayload;

    fn disabled_handler() -> ConversationHandler {
        ConversationHandler::new(ConversationStore::disabled())
    }

    fn make_ctx(conversation_id: Option<&str>) -> RequestContext {
        let req = RequestPayload {
            model: "test".into(),
            input: ResponsesInput::Text("hi".into()),
            instructions: None,
            previous_response_id: None,
            conversation_id: conversation_id.map(str::to_string),
            tools: None,
            tool_choice: ToolChoice::Auto,
            stream: false,
            store: true,
            include: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            metadata: None,
        };
        RequestContext {
            enriched_request: req.clone(),
            original_request: req,
            new_input_items: vec![],
            response_id: "resp_test".into(),
            conversation_id: conversation_id.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn test_get_or_create_missing_id_returns_error() {
        let result = disabled_handler().get_or_create(&make_ctx(None)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rehydrate_missing_id_returns_error() {
        let result = disabled_handler().rehydrate(&make_ctx(None)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_or_create_disabled_store_returns_error() {
        let result = disabled_handler().get_or_create(&make_ctx(Some("conv_1"))).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_disabled_store_returns_error() {
        let result = disabled_handler().get(&make_ctx(Some("conv_1"))).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rehydrate_disabled_store_returns_error() {
        let result = disabled_handler().rehydrate(&make_ctx(Some("conv_1"))).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_turn_missing_conv_id_returns_error() {
        let result = disabled_handler().execute_turn(make_ctx(None), vec![]).await;
        assert!(result.is_err());
    }
}
