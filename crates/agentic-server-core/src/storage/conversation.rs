//! Conversation storage operations.

use std::convert::TryFrom;
use std::sync::Arc;

use super::models::{conversation, item, response};
use super::pool::DbPool;
use super::types::{
    ConversationData, ConversationSnapshot, ConversationVersion, InOutItem, ResponseMetadata, StorageError, StoreResult,
};
use crate::storage::{DbTransaction, Item};
use crate::types::conversations::{ConversationMetadata, ItemOrder};
use crate::utils::common::{serialize_to_string, uuid7_str};

/// Conversation storage operations.
#[derive(Clone, Debug)]
pub struct ConversationStore {
    pool: Option<Arc<DbPool>>,
}

impl ConversationStore {
    /// Creates a disabled conversation store.
    #[must_use]
    pub fn disabled() -> Self {
        Self { pool: None }
    }

    /// Creates a new conversation store with database pool.
    #[must_use]
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self { pool: Some(pool) }
    }

    /// Returns a reference to the database pool.
    ///
    /// # Errors
    ///
    /// Returns error if store is disabled (no pool configured).
    pub fn pool(&self) -> StoreResult<&DbPool> {
        self.pool.as_deref().ok_or(StorageError::NotConfigured)
    }

    /// Creates a new conversation.
    ///
    /// # Errors
    ///
    /// Returns error if database query fails.
    pub async fn create(&self) -> StoreResult<ConversationData> {
        let pool = self.pool()?;
        let row = conversation::create(pool, &uuid7_str("conv_")).await?;
        Ok(row.into())
    }

    /// Gets a conversation or creates it if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns error if database query fails.
    pub async fn get_or_create(&self, conversation_id: &str) -> StoreResult<ConversationData> {
        let pool = self.pool()?;
        let row = conversation::get_or_create(pool, conversation_id).await?;
        Ok(row.into())
    }

    /// Gets a conversation by ID.
    ///
    /// # Errors
    ///
    /// Returns error if conversation not found or database query fails.
    pub async fn get(&self, conversation_id: &str) -> StoreResult<ConversationData> {
        let pool = self.pool()?;
        let row = conversation::get(pool, conversation_id)
            .await?
            .ok_or_else(|| StorageError::not_found("Conversation", conversation_id))?;
        Ok(row.into())
    }

    /// Rehydrates a conversation with all its items.
    ///
    /// # Errors
    ///
    /// Returns an error if a stored item is invalid or missing its sequence number,
    /// or if the database query fails.
    pub async fn rehydrate(&self, conversation_id: &str) -> StoreResult<Vec<InOutItem>> {
        Ok(self.rehydrate_snapshot(conversation_id).await?.items)
    }

    /// Rehydrates a conversation with its items and storage version.
    ///
    /// # Errors
    ///
    /// Returns an error if a stored item is invalid or missing its sequence number,
    /// or if the database query fails.
    pub async fn rehydrate_snapshot(&self, conversation_id: &str) -> StoreResult<ConversationSnapshot> {
        let pool = self.pool()?;
        let snapshot_rows = conversation::get_snapshot(pool, conversation_id).await?;

        let mut last_sequence = None;
        for row in &snapshot_rows.items {
            last_sequence = Some(row.seq.ok_or_else(|| StorageError::InvalidConversationSequence {
                conversation_id: conversation_id.to_string(),
                item_id: row.id.clone(),
            })?);
        }

        Ok(ConversationSnapshot {
            items: snapshot_rows
                .items
                .into_iter()
                .map(|row| InOutItem::try_from(&row))
                .collect::<StoreResult<_>>()?,
            version: ConversationVersion {
                last_sequence,
                response_id: snapshot_rows.latest_response_id,
                revision: snapshot_rows.revision,
            },
        })
    }

    /// Loads metadata from the persisted turn at a captured version.
    ///
    /// # Errors
    ///
    /// Returns an error if the captured response is missing, its metadata is invalid,
    /// or the database lookup fails. Legacy versions without a response return `None`.
    pub async fn response_metadata_at_version(
        &self,
        conversation_id: &str,
        version: &ConversationVersion,
    ) -> StoreResult<Option<ResponseMetadata>> {
        let Some(response_id) = &version.response_id else {
            return Ok(None);
        };
        let pool = self.pool()?;
        let invalid_metadata = || StorageError::InvalidResponseMetadata {
            response_id: response_id.clone(),
        };
        let response = response::get_conversation_turn(pool, conversation_id, response_id)
            .await?
            .ok_or_else(invalid_metadata)?;
        response.metadata_as().map_err(|_| invalid_metadata())
    }

    /// Persists conversation turn with new items and response metadata.
    ///
    /// Creates items in the conversation and stores the associated response record.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if conversation not found or database operation fails.
    pub async fn persist(
        &self,
        conversation_id: &str,
        response_id: &str,
        previous_response_id: Option<&str>,
        new_items: Vec<InOutItem>,
        metadata: &ResponseMetadata,
    ) -> StoreResult<()> {
        self.persist_impl(
            conversation_id,
            None,
            response_id,
            previous_response_id,
            new_items,
            metadata,
        )
        .await
    }

    /// Persists a conversation turn only if its stored version still matches.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the conversation changed, was not found, or a database operation fails.
    pub async fn persist_if_version(
        &self,
        conversation_id: &str,
        expected_version: ConversationVersion,
        response_id: &str,
        previous_response_id: Option<&str>,
        new_items: Vec<InOutItem>,
        metadata: &ResponseMetadata,
    ) -> StoreResult<()> {
        self.persist_impl(
            conversation_id,
            Some(expected_version),
            response_id,
            previous_response_id,
            new_items,
            metadata,
        )
        .await
    }

    async fn persist_impl(
        &self,
        conversation_id: &str,
        expected_version: Option<ConversationVersion>,
        response_id: &str,
        previous_response_id: Option<&str>,
        new_items: Vec<InOutItem>,
        metadata: &ResponseMetadata,
    ) -> StoreResult<()> {
        let pool = self.pool()?;

        let items_ = item::serialize_new_items(new_items, item::ItemSource::ResponseHistory)?;
        let metadata_json = String::try_from(metadata)?;

        let mut tx = pool.begin().await?;

        let locked_conversation = match conversation::lock_in_tx(&mut tx, conversation_id).await {
            Ok(conversation) => conversation,
            Err(sqlx::Error::RowNotFound) => {
                return Err(StorageError::not_found("Conversation", conversation_id));
            }
            Err(error) => return Err(error.into()),
        };
        if let Some(expected_version) = expected_version {
            let current_version = ConversationVersion {
                last_sequence: item::last_conversation_sequence_in_tx(&mut tx, conversation_id).await?,
                response_id: locked_conversation.latest_response_id,
                revision: locked_conversation.revision,
            };
            if current_version != expected_version {
                return Err(StorageError::ConversationConflict {
                    conversation_id: conversation_id.to_owned(),
                });
            }
        }
        item::create_in_tx(&mut tx, items_, Some(conversation_id)).await?;
        // A response branch must see the conversation as it existed at this turn,
        // including manually added items that may later be detached or deleted.
        let history_item_ids = item::ids_for_conversation_in_tx(&mut tx, conversation_id).await?;
        let history_item_ids_json = serialize_to_string(&history_item_ids)?;

        response::create_in_tx(
            &mut tx,
            response_id,
            Some(conversation_id),
            previous_response_id,
            Some(&history_item_ids_json),
            Some(&metadata_json),
        )
        .await?;
        conversation::set_latest_response_in_tx(&mut tx, conversation_id, response_id).await?;
        conversation::bump_revision_in_tx(&mut tx, conversation_id).await?;
        tx.commit().await?;

        Ok(())
    }

    /// Create a conversation and its initial items atomically.
    ///
    /// # Errors
    /// Returns an error if serialization or persistence fails.
    pub async fn create_with_metadata_and_items(
        &self,
        tenant_id: Option<&str>,
        metadata: Option<ConversationMetadata>,
        initial_items: Vec<InOutItem>,
    ) -> StoreResult<ConversationData> {
        let items = item::serialize_new_items(initial_items, item::ItemSource::ConversationApi)?;
        let metadata = metadata.map(|value| serialize_to_string(&value)).transpose()?;
        let id = uuid7_str("conv_");
        let mut tx = self.pool()?.begin().await?;
        let row = conversation::create_in_tx(&mut tx, &id, tenant_id, metadata.as_deref()).await?;
        if !items.is_empty() {
            item::create_in_tx(&mut tx, items, Some(&id)).await?;
            conversation::bump_revision_in_tx(&mut tx, &id).await?;
        }
        tx.commit().await?;
        Ok(row.into())
    }

    /// Retrieve a conversation owned by the tenant.
    ///
    /// # Errors
    /// Returns an error if the resource is missing or the query fails.
    pub async fn retrieve(&self, tenant_id: &str, id: &str) -> StoreResult<ConversationData> {
        conversation::get_by_tenant(self.pool()?, tenant_id, id)
            .await?
            .map(Into::into)
            .ok_or_else(|| StorageError::not_found("Conversation", id))
    }

    /// Replace metadata on a conversation owned by the tenant.
    ///
    /// # Errors
    /// Returns an error if the resource is missing or the update fails.
    pub async fn update_metadata(
        &self,
        tenant_id: &str,
        id: &str,
        metadata: ConversationMetadata,
    ) -> StoreResult<ConversationData> {
        let metadata = serialize_to_string(&metadata)?;
        conversation::update_metadata(self.pool()?, tenant_id, id, &metadata)
            .await
            .map(Into::into)
            .map_err(|error| conversation_error(error, id))
    }

    /// Delete a conversation without deleting items referenced by stored responses.
    ///
    /// # Errors
    /// Returns an error if the resource is missing or the transaction fails.
    pub async fn delete(&self, tenant_id: &str, id: &str) -> StoreResult<()> {
        let mut tx = self.pool()?.begin().await?;
        lock_owned(&mut tx, tenant_id, id).await?;
        item::detach_from_conversation_in_tx(&mut tx, id, None).await?;
        conversation::delete_in_tx(&mut tx, id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Append items in order, sharing sequence allocation with Responses persistence.
    ///
    /// # Errors
    /// Returns an error if the resource is missing or persistence fails.
    pub async fn create_items(&self, tenant_id: &str, id: &str, items: Vec<InOutItem>) -> StoreResult<Vec<Item>> {
        let items = item::serialize_new_items(items, item::ItemSource::ConversationApi)?;
        let mut tx = self.pool()?.begin().await?;
        lock_owned(&mut tx, tenant_id, id).await?;
        let rows = item::create_in_tx(&mut tx, items, Some(id)).await?;
        if !rows.is_empty() {
            conversation::bump_revision_in_tx(&mut tx, id).await?;
        }
        tx.commit().await?;
        Ok(rows)
    }

    /// List a page, resolving the cursor within the authorized conversation.
    ///
    /// # Errors
    /// Returns an error for a missing conversation/cursor or a failed query.
    pub async fn list_items(
        &self,
        tenant_id: &str,
        id: &str,
        limit: i64,
        after: Option<&str>,
        order: ItemOrder,
    ) -> StoreResult<Vec<Item>> {
        self.retrieve(tenant_id, id).await?;
        item::list_for_conversation(self.pool()?, tenant_id, id, limit, after, order)
            .await
            .map_err(|error| match (error, after) {
                (sqlx::Error::RowNotFound, Some(cursor)) => StorageError::ItemCursorNotFound { id: cursor.to_owned() },
                (other, _) => other.into(),
            })
    }

    /// Retrieve one item belonging to the authorized conversation.
    ///
    /// # Errors
    /// Returns an error if the item is missing or the query fails.
    pub async fn retrieve_item(&self, tenant_id: &str, id: &str, item_id: &str) -> StoreResult<Item> {
        item::get_for_conversation(self.pool()?, tenant_id, id, item_id)
            .await?
            .ok_or_else(|| StorageError::not_found("Conversation item", item_id))
    }

    /// Remove an item from conversation history and advance the snapshot revision.
    ///
    /// # Errors
    /// Returns an error if the resource is missing or the transaction fails.
    pub async fn delete_item(&self, tenant_id: &str, id: &str, item_id: &str) -> StoreResult<ConversationData> {
        let mut tx = self.pool()?.begin().await?;
        let row = lock_owned(&mut tx, tenant_id, id).await?;
        if item::detach_from_conversation_in_tx(&mut tx, id, Some(item_id)).await? == 0 {
            return Err(StorageError::not_found("Conversation item", item_id));
        }
        conversation::bump_revision_in_tx(&mut tx, id).await?;
        tx.commit().await?;
        Ok(row.into())
    }
}

async fn lock_owned(tx: &mut DbTransaction<'_>, tenant_id: &str, id: &str) -> StoreResult<conversation::Conversation> {
    let row = conversation::lock_in_tx(tx, id)
        .await
        .map_err(|error| conversation_error(error, id))?;
    if row.tenant_id.as_deref() != Some(tenant_id) {
        return Err(StorageError::not_found("Conversation", id));
    }
    Ok(row)
}

fn conversation_error(error: sqlx::Error, id: &str) -> StorageError {
    match error {
        sqlx::Error::RowNotFound => StorageError::not_found("Conversation", id),
        other => other.into(),
    }
}
