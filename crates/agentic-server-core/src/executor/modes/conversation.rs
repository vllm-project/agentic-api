//! Conversation storage handler — owns all conversation store operations.

use crate::storage::{ConversationData, ConversationSnapshot, ConversationStore, ResponseMetadata};
use crate::types::conversations::ConversationMetadata;
use crate::types::io::InOutItem;
use crate::types::io::OutputItem;
use crate::types::io::{ItemPage, ListItemsResponse};
use crate::types::request_response::ResponsePayload;

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

    /// Creates a tenant-owned conversation with its metadata and initial history.
    ///
    /// # Errors
    /// Returns an error if more than 20 items are supplied or storage fails.
    pub async fn create(
        &self,
        tenant_id: &str,
        metadata: &ConversationMetadata,
        items: &[InOutItem],
    ) -> ExecutorResult<ConversationData> {
        if items.len() > 20 {
            return Err(ExecutorError::InvalidRequest(
                "a conversation accepts at most 20 initial items".into(),
            ));
        }
        self.store
            .create(tenant_id, metadata, items)
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

    /// Loads the conversation referenced by the request: its history items, storage version,
    /// and the metadata of the turn at that version.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation` is absent, the store is
    /// disabled, or the database query fails.
    pub async fn rehydrate(&self, ctx: &RequestContext) -> ExecutorResult<ConversationSnapshot> {
        let conv_id = ctx
            .original_request
            .conversation
            .as_deref()
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation is required for rehydrate".into()))?;
        self.store.rehydrate(conv_id).await.map_err(ExecutorError::Storage)
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

        self.store
            .create_items(tenant_id, conversation_id, items)
            .await
            .map_err(ExecutorError::Storage)
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
        Ok(ListItemsResponse::new(items, has_more))
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
        self.store
            .retrieve_item(tenant_id, conversation_id, item_id)
            .await
            .map_err(ExecutorError::Storage)
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

    /// Persists one conversation turn: only the new items from this turn, with its snapshot and metadata.
    ///
    /// The store tracks sequence numbers and appends, so prior history must not be re-inserted.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `conversation` is absent on the context,
    /// the store is disabled, or the database operation fails.
    pub async fn persist(
        &self,
        ctx: RequestContext,
        output_items: Vec<OutputItem>,
        snapshot: Option<&ResponsePayload>,
        metadata: ResponseMetadata,
    ) -> ExecutorResult<()> {
        let conversation_id = ctx
            .conversation_id
            .ok_or_else(|| ExecutorError::InvalidRequest("conversation is required for persist".into()))?;

        let mut new_items = Vec::with_capacity(ctx.new_input_items.len() + output_items.len());
        let output_items: Vec<_> = output_items
            .into_iter()
            .filter(|item| !item.is_already_in_history(&ctx.new_input_items))
            .collect();
        new_items.extend(ctx.new_input_items);
        new_items.extend(output_items.into_iter().map(InOutItem::Output));

        self.store
            .persist(&conversation_id, &ctx.response_id, new_items, snapshot, &metadata)
            .await
            .map_err(ExecutorError::Storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ResponseMetadata, create_pool_with_schema};
    use crate::types::io::ResponsesInput;
    use crate::types::io::{InputItem, InputMessageContent};
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
            checkpoint: None,
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
    async fn test_persist_missing_conv_id_returns_error() {
        let result = disabled_handler()
            .persist(make_ctx(None), vec![], None, ResponseMetadata::default())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn persist_records_the_turn_and_its_metadata() -> Result<(), Box<dyn std::error::Error>> {
        use crate::types::io::ToolChoice;

        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let store = ConversationStore::new(pool);
        let conversation = store
            .create("default_tenant", &ConversationMetadata::new(), &[])
            .await?;
        let handler = ConversationHandler::new(store.clone());
        let mut ctx = make_ctx(Some(&conversation.conversation_id));
        ctx.new_input_items = ctx.original_request.input.clone().into_canonical_items();
        ctx.enriched_request.tool_choice = Some(ToolChoice::Required);
        ctx.relax_tool_choice();

        let responses = crate::executor::ResponseHandler::new(crate::storage::ResponseStore::disabled());
        crate::executor::persist::persist_prepared_turn(ctx, None, vec![], None, &handler, &responses).await?;

        let snapshot = store.rehydrate(&conversation.conversation_id).await?;
        let metadata = snapshot.turn_metadata.clone().expect("persisted turn metadata");
        assert_eq!(metadata.effective_tool_choice, ToolChoice::Required);
        assert_eq!(snapshot.items.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn persist_appends_after_a_turn_stored_while_it_ran() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let store = ConversationStore::new(pool);
        let conversation = store
            .create("default_tenant", &ConversationMetadata::new(), &[])
            .await?;
        let handler = ConversationHandler::new(store.clone());
        let mut ctx = make_ctx(Some(&conversation.conversation_id));
        ctx.new_input_items = ctx.original_request.input.clone().into_canonical_items();
        let competing_items = Vec::from(&ResponsesInput::Text("competing input".into()))
            .into_iter()
            .map(InOutItem::Input)
            .collect();
        store
            .persist(
                &conversation.conversation_id,
                "resp_competing",
                competing_items,
                None,
                &ResponseMetadata::default(),
            )
            .await?;

        // OpenAI stores a turn after whatever the conversation gained while it ran.
        handler.persist(ctx, vec![], None, ResponseMetadata::default()).await?;

        let snapshot = store.rehydrate(&conversation.conversation_id).await?;
        assert_eq!(snapshot.items.len(), 2);
        assert!(matches!(
            &snapshot.items[0],
            InOutItem::Input(InputItem::Message(message))
                if matches!(&message.content, InputMessageContent::Text(text) if text == "competing input")
        ));
        Ok(())
    }
}
