//! Conversation storage handler — owns all conversation store operations.

use crate::storage::{
    ConversationData, ConversationSnapshot, ConversationStore, ConversationVersion, ResponseMetadata, StorageError,
};
use crate::types::conversations::ConversationMetadata;
use crate::types::io::InOutItem;
use crate::types::io::OutputItem;
use crate::types::io::{ItemPage, ListItemsResponse};

use super::CommittedResponse;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::multi_agent::ValidatedTreeCheckpoint;
use crate::executor::request::RequestContext;

/// Handles all conversation store operations: creation, rehydration, and persistence.
#[derive(Clone, Debug)]
pub struct ConversationHandler {
    store: ConversationStore,
}

impl ConversationHandler {
    pub(crate) async fn commit_tree(
        &self,
        ctx: RequestContext,
        output_items: Vec<OutputItem>,
        mut metadata: ResponseMetadata,
        tree: ValidatedTreeCheckpoint,
    ) -> ExecutorResult<CommittedResponse> {
        let response_id = ctx.response_id.clone();
        metadata.multi_agent_tree = Some(tree.into_snapshot());
        self.execute_turn_with_metadata(ctx, output_items, metadata).await?;
        Ok(CommittedResponse { response_id })
    }
    #[must_use]
    pub fn new(store: ConversationStore) -> Self {
        Self { store }
    }

    /// Gets an existing conversation or creates one.
    ///
    /// Reads `conversation` from `ctx.original_request`.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation` is absent, the store is
    /// disabled, or the database query fails.
    pub async fn get_or_create(&self, ctx: &RequestContext) -> ExecutorResult<ConversationData> {
        let conv_id = ctx
            .original_request
            .conversation
            .as_deref()
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation is required for get_or_create".into()))?;
        self.store.get_or_create(conv_id).await.map_err(ExecutorError::Storage)
    }

    /// Gets an existing conversation.
    ///
    /// Reads `conversation` from `ctx.original_request`.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation` is absent, the store is
    /// disabled, the conversation does not exist, or the database query fails.
    pub async fn get(&self, ctx: &RequestContext) -> ExecutorResult<ConversationData> {
        let conv_id = ctx
            .original_request
            .conversation
            .as_deref()
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation is required for get".into()))?;
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
        items: Vec<InOutItem>,
    ) -> ExecutorResult<ConversationData> {
        if items.len() > 20 {
            return Err(ExecutorError::InvalidRequest(
                "a conversation accepts at most 20 initial items".into(),
            ));
        }
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
    /// Reads `conversation` from `ctx.original_request`. Returns an empty vec
    /// if the conversation exists but has no items yet.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation` is absent, the store is
    /// disabled, or the database query fails.
    pub async fn rehydrate(&self, ctx: &RequestContext) -> ExecutorResult<Vec<InOutItem>> {
        Ok(self.rehydrate_snapshot(ctx).await?.items)
    }

    /// Loads the conversation's history items and storage version.
    ///
    /// Reads `conversation` from `ctx.original_request`.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation` is absent, the store is
    /// disabled, or the database query fails.
    pub async fn rehydrate_snapshot(&self, ctx: &RequestContext) -> ExecutorResult<ConversationSnapshot> {
        let conv_id = ctx
            .original_request
            .conversation
            .as_deref()
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation is required for rehydrate".into()))?;
        self.store
            .rehydrate_snapshot(conv_id)
            .await
            .map_err(ExecutorError::Storage)
    }

    /// Loads metadata for the persisted turn matching a captured conversation version.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation` is absent, the store is
    /// disabled, or the database query fails.
    pub(crate) async fn response_metadata_at_version(
        &self,
        ctx: &RequestContext,
        version: &ConversationVersion,
    ) -> ExecutorResult<Option<ResponseMetadata>> {
        let conv_id = ctx.original_request.conversation.as_deref().ok_or_else(|| {
            ExecutorError::InvalidRequest("conversation is required for response metadata lookup".into())
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
        items: Vec<InOutItem>,
    ) -> ExecutorResult<Vec<InOutItem>> {
        if items.is_empty() || items.len() > 20 {
            return Err(ExecutorError::InvalidRequest(
                "items must contain between 1 and 20 items".into(),
            ));
        }

        let created = self
            .store
            .create_items(tenant_id, conversation_id, items)
            .await
            .map_err(ExecutorError::Storage)?;

        // Perform fallible conversion - propagate errors instead of silently dropping items
        let mut item_responses = Vec::with_capacity(created.len());
        for db_item in created {
            let conversation_item = InOutItem::try_from(&db_item)?;
            item_responses.push(conversation_item.into_public(db_item.public_id().to_owned()));
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
        page: ItemPage<'_>,
    ) -> ExecutorResult<ListItemsResponse> {
        // Fetch one extra item to determine if there are more items
        let mut items = self
            .store
            .list_items(tenant_id, conversation_id, page.limit.probe(), page.after, page.order)
            .await
            .map_err(ExecutorError::Storage)?;
        let has_more = items.len() > page.limit.get();
        items.truncate(page.limit.get());

        // Perform fallible conversion - propagate errors instead of silently dropping items
        let mut item_responses = Vec::with_capacity(items.len());
        for db_item in items {
            let conversation_item = InOutItem::try_from(&db_item)?;
            item_responses.push(conversation_item.into_public(db_item.public_id().to_owned()));
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
    ) -> ExecutorResult<InOutItem> {
        let db_item = self
            .store
            .retrieve_item(tenant_id, conversation_id, item_id)
            .await
            .map_err(ExecutorError::Storage)?;

        let conversation_item = InOutItem::try_from(&db_item)?;
        Ok(conversation_item.into_public(db_item.public_id().to_owned()))
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
    /// Returns `ExecutorError` if `conversation` is absent on the context,
    /// the store is disabled, or the database operation fails.
    pub async fn execute_turn(&self, mut ctx: RequestContext, output_items: Vec<OutputItem>) -> ExecutorResult<()> {
        let metadata = ResponseMetadata {
            multi_agent_tree: ctx.multi_agent_tree.take().map(ValidatedTreeCheckpoint::into_snapshot),
            model: std::mem::take(&mut ctx.enriched_request.model),
            previous_response_id: ctx.original_request.previous_response_id.take(),
            effective_tools: ctx.enriched_request.tools.take(),
            tool_search_loaded_tools: None,
            response_snapshot: None,
            effective_tool_choice: ctx
                .original_request
                .tool_choice
                .take()
                .or_else(|| ctx.enriched_request.tool_choice.take())
                .unwrap_or_default(),
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
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation is required for execute_turn".into()))?;
        let conversation_version = ctx
            .conversation_version
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation version is required for execute_turn".into()))?;

        let mut new_items = Vec::with_capacity(ctx.new_input_items.len() + output_items.len());
        let output_items: Vec<_> = output_items
            .into_iter()
            .filter(|item| !item.is_already_in_history(&ctx.new_input_items))
            .collect();
        new_items.extend(ctx.new_input_items);
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
            store: true,
            conversation: conversation_id.map(str::to_string),
            ..Default::default()
        };
        RequestContext {
            multi_agent_tree: None,
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
        ctx.new_input_items = ctx.original_request.input.clone().into_canonical_items();

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
        use crate::types::io::ToolChoice;

        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let store = ConversationStore::new(pool);
        let conversation = store.create().await?;
        let handler = ConversationHandler::new(store.clone());
        let mut ctx = make_ctx(Some(&conversation.conversation_id));
        ctx.new_input_items = ctx.original_request.input.clone().into_canonical_items();
        ctx.conversation_version = Some(ConversationVersion::default());
        ctx.enriched_request.tool_choice = Some(ToolChoice::Required);
        ctx.relax_tool_choice();

        handler.execute_turn(ctx, vec![]).await?;

        let snapshot = store.rehydrate_snapshot(&conversation.conversation_id).await?;
        let metadata = store
            .response_metadata_at_version(&conversation.conversation_id, &snapshot.version)
            .await?
            .expect("persisted turn metadata");
        assert_eq!(metadata.effective_tool_choice, ToolChoice::Required);
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
        ctx.new_input_items = ctx.original_request.input.clone().into_canonical_items();
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
