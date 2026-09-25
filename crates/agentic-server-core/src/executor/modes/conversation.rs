//! Conversation storage handler — owns all conversation store operations.

use crate::storage::{
    ConversationData, ConversationSnapshot, ConversationStore, ConversationVersion, InOutItem, Item, ResponseMetadata,
    StorageError,
};
use crate::types::conversations::{ConversationItem, ConversationMetadata, ItemResponse, ListItemsResponse};
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

    /// Creates a tenant-owned conversation with optional metadata and initial history.
    ///
    /// # Errors
    /// Returns an error if too many items are supplied or storage fails.
    pub async fn create_with_metadata_and_items(
        &self,
        tenant_id: &str,
        metadata: Option<ConversationMetadata>,
        items: Vec<ConversationItem>,
    ) -> ExecutorResult<ConversationData> {
        if items.len() > 20 {
            return Err(ExecutorError::InvalidRequest(
                "a conversation accepts at most 20 initial items".into(),
            ));
        }
        let items = items.into_iter().map(into_stored_item).collect();
        self.store
            .create_with_metadata_and_items(Some(tenant_id), metadata, items)
            .await
            .map_err(ExecutorError::Storage)
    }

    /// Retrieves a tenant-owned conversation.
    ///
    /// # Errors
    /// Returns an error if the conversation does not exist or storage fails.
    pub async fn retrieve(&self, tenant_id: &str, conversation_id: &str) -> ExecutorResult<ConversationData> {
        self.store
            .retrieve(tenant_id, conversation_id)
            .await
            .map_err(ExecutorError::Storage)
    }

    /// Updates metadata on a tenant-owned conversation.
    ///
    /// # Errors
    /// Returns an error if the conversation does not exist or storage fails.
    pub async fn update_metadata(
        &self,
        tenant_id: &str,
        conversation_id: &str,
        metadata: ConversationMetadata,
    ) -> ExecutorResult<ConversationData> {
        self.store
            .update_metadata(tenant_id, conversation_id, metadata)
            .await
            .map_err(ExecutorError::Storage)
    }

    /// Deletes a tenant-owned conversation.
    ///
    /// # Errors
    /// Returns an error if the conversation does not exist or storage fails.
    pub async fn delete(&self, tenant_id: &str, conversation_id: &str) -> ExecutorResult<()> {
        self.store
            .delete(tenant_id, conversation_id)
            .await
            .map_err(ExecutorError::Storage)
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
        Ok(self.rehydrate_snapshot(ctx).await?.items)
    }

    /// Loads the conversation's history items and storage version.
    ///
    /// Reads `conversation_id` from `ctx.original_request`.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation_id` is absent, the store is
    /// disabled, or the database query fails.
    pub async fn rehydrate_snapshot(&self, ctx: &RequestContext) -> ExecutorResult<ConversationSnapshot> {
        let conv_id = ctx
            .original_request
            .conversation_id
            .as_deref()
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation_id is required for rehydrate".into()))?;
        self.store
            .rehydrate_snapshot(conv_id)
            .await
            .map_err(ExecutorError::Storage)
    }

    /// Loads metadata for the persisted turn matching a captured conversation version.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation_id` is absent, the store is
    /// disabled, or the database query fails.
    pub(crate) async fn response_metadata_at_version(
        &self,
        ctx: &RequestContext,
        version: &ConversationVersion,
    ) -> ExecutorResult<Option<ResponseMetadata>> {
        let conv_id = ctx.original_request.conversation_id.as_deref().ok_or_else(|| {
            ExecutorError::InvalidRequest("conversation_id is required for response metadata lookup".into())
        })?;
        self.store
            .response_metadata_at_version(conv_id, version)
            .await
            .map_err(ExecutorError::Storage)
    }

    /// Creates items in a conversation, returning typed responses.
    ///
    /// # Errors
    /// Returns `ExecutorError` if conversation not found, items invalid, or database operation fails.
    pub async fn create_items(
        &self,
        tenant_id: &str,
        conversation_id: &str,
        items: Vec<ConversationItem>,
    ) -> ExecutorResult<Vec<ItemResponse>> {
        if items.is_empty() || items.len() > 20 {
            return Err(ExecutorError::InvalidRequest(
                "items must contain between 1 and 20 items".into(),
            ));
        }

        let stored_items: Vec<InOutItem> = items.into_iter().map(into_stored_item).collect();

        let created = self
            .store
            .create_items(tenant_id, conversation_id, stored_items)
            .await
            .map_err(ExecutorError::Storage)?;

        // Perform fallible conversion - propagate errors instead of silently dropping items
        let mut item_responses = Vec::with_capacity(created.len());
        for db_item in created {
            let conversation_item = convert_item(&db_item)?;
            item_responses.push(ItemResponse::new(db_item.public_id().to_owned(), conversation_item));
        }

        Ok(item_responses)
    }

    /// Lists items in a conversation with pagination.
    ///
    /// # Errors
    /// Returns `ExecutorError` if conversation not found or database query fails.
    pub async fn list_items(
        &self,
        tenant_id: &str,
        conversation_id: &str,
        limit: i64,
        after: Option<&str>,
        order: &str,
    ) -> ExecutorResult<ListItemsResponse> {
        // Validate order parameter
        if order != "asc" && order != "desc" {
            return Err(ExecutorError::InvalidRequest("order must be 'asc' or 'desc'".into()));
        }

        let limit = limit.clamp(1, 100);
        let order_enum = if order == "desc" {
            crate::types::conversations::ItemOrder::Desc
        } else {
            crate::types::conversations::ItemOrder::Asc
        };

        // Fetch limit + 1 to determine if there are more items
        let mut items = self
            .store
            .list_items(tenant_id, conversation_id, limit + 1, after, order_enum)
            .await
            .map_err(ExecutorError::Storage)?;

        // Safe: limit is clamped to [1, 100], far below usize range
        #[allow(clippy::cast_possible_wrap)]
        let has_more = (items.len() as i64) > limit;
        if has_more {
            items.pop(); // Remove the extra item
        }

        // Perform fallible conversion - propagate errors instead of silently dropping items
        let mut item_responses = Vec::with_capacity(items.len());
        for db_item in items {
            let conversation_item = convert_item(&db_item)?;
            item_responses.push(ItemResponse::new(db_item.public_id().to_owned(), conversation_item));
        }

        Ok(ListItemsResponse::new(item_responses, has_more))
    }

    /// Retrieves a single item by ID.
    ///
    /// # Errors
    /// Returns `ExecutorError` if item not found or database query fails.
    pub async fn retrieve_item(
        &self,
        tenant_id: &str,
        conversation_id: &str,
        item_id: &str,
    ) -> ExecutorResult<ItemResponse> {
        let db_item = self
            .store
            .retrieve_item(tenant_id, conversation_id, item_id)
            .await
            .map_err(ExecutorError::Storage)?;

        let conversation_item = convert_item(&db_item)?;
        Ok(ItemResponse::new(db_item.public_id().to_owned(), conversation_item))
    }

    /// Deletes an item by ID, returning the updated conversation.
    ///
    /// # Errors
    /// Returns `ExecutorError` if item not found or database operation fails.
    pub async fn delete_item(
        &self,
        tenant_id: &str,
        conversation_id: &str,
        item_id: &str,
    ) -> ExecutorResult<ConversationData> {
        self.store
            .delete_item(tenant_id, conversation_id, item_id)
            .await
            .map_err(ExecutorError::Storage)
    }

    /// Persists one conversation turn — only the new items from this turn.
    ///
    /// Takes `ctx` and `output_items` by value so fields can be moved directly
    /// into [`ResponseMetadata`]. The store tracks sequence
    /// numbers and appends, so prior history must not be re-inserted.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation_id` is absent on the context,
    /// the store is disabled, or the database operation fails.
    pub async fn execute_turn(&self, mut ctx: RequestContext, output_items: Vec<OutputItem>) -> ExecutorResult<()> {
        let metadata = ResponseMetadata {
            model: std::mem::take(&mut ctx.enriched_request.model),
            previous_response_id: ctx.original_request.previous_response_id.take(),
            effective_tools: ctx.enriched_request.tools.take(),
            tool_search_loaded_tools: None,
            effective_tool_choice: ctx.enriched_request.tool_choice.take().unwrap_or_default(),
            effective_instructions: ctx.enriched_request.instructions.take(),
        };

        self.execute_turn_with_metadata(ctx, output_items, metadata).await
    }

    /// Persists a conversation turn using metadata prepared by request-scoped tool behavior.
    pub(crate) async fn execute_turn_with_metadata(
        &self,
        ctx: RequestContext,
        output_items: Vec<OutputItem>,
        metadata: ResponseMetadata,
    ) -> ExecutorResult<()> {
        let conversation_id = ctx
            .conversation_id
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation_id is required for execute_turn".into()))?;
        let conversation_version = ctx
            .conversation_version
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation version is required for execute_turn".into()))?;

        let mut new_items = Vec::with_capacity(ctx.new_input_items.len() + output_items.len());
        new_items.extend(ctx.new_input_items.into_iter().map(InOutItem::Input));
        new_items.extend(output_items.into_iter().map(InOutItem::Output));

        self.store
            .persist_if_version(
                &conversation_id,
                conversation_version,
                &ctx.response_id,
                metadata.previous_response_id.as_deref(),
                new_items,
                &metadata,
            )
            .await
            .map_err(|error| match error {
                source @ StorageError::ConversationConflict { .. } => ExecutorError::ConversationLocked { source },
                other => ExecutorError::Storage(other),
            })
    }
}

/// Converts a stored Item to a `ConversationItem` with fallible error handling.
///
/// # Errors
/// Returns `ExecutorError` if the stored item cannot be deserialized.
fn convert_item(item: &Item) -> ExecutorResult<ConversationItem> {
    let io_item = item
        .as_inout()
        .ok_or_else(|| ExecutorError::InvalidRequest("Failed to deserialize stored item".into()))?;

    match io_item {
        InOutItem::Input(item_input) => Ok(ConversationItem::Input(item_input)),
        InOutItem::Output(item_output) => Ok(ConversationItem::Output(item_output)),
    }
}

fn into_stored_item(item: ConversationItem) -> InOutItem {
    match item {
        ConversationItem::Input(input) => InOutItem::Input(input),
        ConversationItem::Output(output) => InOutItem::Output(output),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ConversationVersion, ResponseMetadata, create_pool_with_schema};
    use crate::types::io::ResponsesInput;
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
            tool_choice: None,
            stream: false,
            store: true,
            include: None,
            reasoning: None,
            text: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            ignore_eos: None,
            truncation: None,
            metadata: None,
            parallel_tool_calls: None,
            prompt_cache_key: None,
            cache_salt: None,
            context_management: None,
        };
        RequestContext {
            enriched_request: req.clone(),
            original_request: req,
            new_input_items: vec![],
            response_id: "resp_test".into(),
            conversation_id: conversation_id.map(str::to_string),
            conversation_version: None,
            continuation: None,
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

    #[tokio::test]
    async fn execute_turn_rejects_missing_conversation_version_without_writing()
    -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let store = ConversationStore::new(pool);
        let conversation = store.create().await?;
        let handler = ConversationHandler::new(store.clone());
        let mut ctx = make_ctx(Some(&conversation.conversation_id));
        ctx.new_input_items = Vec::from(&ctx.original_request.input);

        let error = handler
            .execute_turn(ctx, vec![])
            .await
            .expect_err("missing captured version must reject the turn");

        assert!(matches!(
            error,
            ExecutorError::InvalidRequest(message)
                if message == "conversation version is required for execute_turn"
        ));
        assert!(store.rehydrate(&conversation.conversation_id).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn execute_turn_persists_with_captured_conversation_version() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let store = ConversationStore::new(pool);
        let conversation = store.create().await?;
        let handler = ConversationHandler::new(store.clone());
        let mut ctx = make_ctx(Some(&conversation.conversation_id));
        ctx.new_input_items = Vec::from(&ctx.original_request.input);
        ctx.conversation_version = Some(ConversationVersion::default());

        handler.execute_turn(ctx, vec![]).await?;

        let snapshot = store.rehydrate_snapshot(&conversation.conversation_id).await?;
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(
            snapshot.version,
            ConversationVersion {
                response_id: Some("resp_test".to_owned()),
                revision: 1,
                last_sequence: Some(0),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn execute_turn_rejects_a_stale_captured_conversation_version() -> Result<(), Box<dyn std::error::Error>> {
        use std::error::Error;

        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let store = ConversationStore::new(pool);
        let conversation = store.create().await?;
        let handler = ConversationHandler::new(store.clone());
        let mut ctx = make_ctx(Some(&conversation.conversation_id));
        ctx.new_input_items = Vec::from(&ctx.original_request.input);
        ctx.conversation_version = Some(ConversationVersion::default());
        let competing_items = Vec::from(&ResponsesInput::Text("competing input".into()))
            .into_iter()
            .map(InOutItem::Input)
            .collect();
        store
            .persist(
                &conversation.conversation_id,
                "resp_competing",
                None,
                competing_items,
                &ResponseMetadata::default(),
            )
            .await?;

        let error = handler
            .execute_turn(ctx, vec![])
            .await
            .expect_err("stale captured version must reject the turn");

        let source = error.source().expect("conversation conflict source must be retained");
        assert!(matches!(
            source.downcast_ref::<StorageError>(),
            Some(StorageError::ConversationConflict { conversation_id })
                if conversation_id == &conversation.conversation_id
        ));
        assert!(matches!(error, ExecutorError::ConversationLocked { .. }));
        Ok(())
    }
}
