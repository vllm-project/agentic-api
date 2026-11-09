//! Conversation storage operations.

use std::convert::TryFrom;
use std::sync::Arc;

use super::models::conversation::ConversationHistory;
use super::models::{conversation, item, response};
use super::pool::{DbPool, DbTransaction};
use super::types::conversation::encode_metadata;
use super::types::item::{decode_public, encode};
use super::types::response::{decode_turn_metadata, encode_history};
use super::types::{
    ConversationData, ConversationSnapshot, ResponseCheckpoint, ResponseMetadata, StorageError, StoreResult,
};
use crate::types::conversations::ConversationMetadata;
use crate::types::io::InOutItem;
use crate::types::io::ItemOrder;
use crate::types::request_response::ResponsePayload;
use crate::utils::common::uuid7_str;

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
    pub(in crate::storage) fn pool(&self) -> StoreResult<&DbPool> {
        self.pool.as_deref().ok_or(StorageError::NotConfigured)
    }

    /// Creates a tenant-owned conversation and its initial items atomically.
    ///
    /// # Errors
    /// Returns an error if an item ID is invalid, or serialization or persistence fails.
    pub async fn create(
        &self,
        tenant_id: &str,
        metadata: &ConversationMetadata,
        items: &[InOutItem],
    ) -> StoreResult<ConversationData> {
        let items = items.iter().map(encode).collect::<StoreResult<Vec<_>>>()?;
        let metadata = encode_metadata(metadata)?;
        let id = uuid7_str("conv_");
        let mut tx = self.pool()?.begin().await?;
        let row = conversation::create_in_tx(&mut tx, &id, Some(tenant_id), Some(&metadata)).await?;
        item::create_in_tx(&mut tx, items, Some(&id)).await?;
        tx.commit().await?;
        row.try_into()
    }

    /// Gets a conversation or creates it if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns error if database query fails.
    pub async fn get_or_create(&self, conversation_id: &str) -> StoreResult<ConversationData> {
        let pool = self.pool()?;
        conversation::get_or_create(pool, conversation_id).await?.try_into()
    }

    /// Gets a conversation by ID.
    ///
    /// # Errors
    ///
    /// Returns error if conversation not found or database query fails.
    pub async fn get(&self, conversation_id: &str) -> StoreResult<ConversationData> {
        let pool = self.pool()?;
        conversation::get(pool, conversation_id)
            .await?
            .ok_or_else(|| StorageError::not_found("Conversation", conversation_id))?
            .try_into()
    }

    /// Rehydrates a conversation: its items and the checkpoint a turn continues (the latest turn's metadata and the
    /// history rows).
    ///
    /// # Errors
    ///
    /// Returns an error if a stored item is invalid or missing its sequence number, the turn's metadata
    /// is missing or invalid, or the database query fails.
    pub async fn rehydrate(&self, conversation_id: &str) -> StoreResult<ConversationSnapshot> {
        let pool = self.pool()?;
        let Some(ConversationHistory {
            latest_response_id,
            latest_turn,
            items: rows,
        }) = conversation::get_with_items(pool, conversation_id).await?
        else {
            return Ok(ConversationSnapshot::default());
        };

        if let Some(row) = rows.iter().find(|row| row.seq.is_none()) {
            return Err(StorageError::InvalidConversationSequence {
                conversation_id: conversation_id.to_owned(),
                item_id: row.id.clone(),
            });
        }
        let items = rows.iter().map(InOutItem::try_from).collect::<StoreResult<_>>()?;
        let history_item_ids = rows.into_iter().map(|row| row.id).collect();
        let metadata = match &latest_response_id {
            None => None,
            Some(response_id) => decode_turn_metadata(response_id, latest_turn.as_ref())?,
        };
        let response_id = metadata.is_some().then_some(latest_response_id).flatten();

        Ok(ConversationSnapshot {
            items,
            checkpoint: ResponseCheckpoint {
                response_id,
                history: Vec::new(),
                metadata: metadata.unwrap_or_default(),
                durable: true,
                history_item_ids,
                requires_latest: false,
            },
        })
    }

    /// Persists a conversation turn: appends its new items after whatever the conversation holds now, as `OpenAI`
    /// does, and stores the response with its snapshot and metadata as the latest turn. `checkpoint` is the
    /// conversation as this turn rehydrated it.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ConversationConflict`] if the checkpoint requires the latest turn and another turn was
    /// stored since, and [`StorageError`] if the conversation was not found or a database operation fails.
    pub async fn persist(
        &self,
        conversation_id: &str,
        checkpoint: &ResponseCheckpoint,
        response_id: &str,
        new_items: Vec<InOutItem>,
        snapshot: Option<&ResponsePayload>,
        metadata: &ResponseMetadata,
    ) -> StoreResult<()> {
        let pool = self.pool()?;
        let new_items = new_items.iter().map(encode).collect::<StoreResult<Vec<_>>>()?;
        let snapshot_json = snapshot.map(String::try_from).transpose()?;
        let metadata_json = String::try_from(metadata)?;

        let mut tx = pool.begin().await?;
        let conversation = conversation::lock_in_tx(&mut tx, conversation_id)
            .await
            .map_err(|error| conversation_error(error, conversation_id))?;
        if checkpoint.requires_latest && conversation.latest_response_id != checkpoint.response_id {
            return Err(StorageError::ConversationConflict {
                conversation_id: conversation_id.to_owned(),
            });
        }
        // The response keeps the history the turn continued, as OpenAI does: items added to or removed from the
        // conversation while the turn ran are not part of it.
        let mut history_item_ids = checkpoint.history_item_ids.clone();
        let created = item::create_in_tx(&mut tx, new_items, Some(conversation_id)).await?;
        history_item_ids.extend(created.into_iter().map(|row| row.id));
        let history_item_ids_json = encode_history(&history_item_ids)?;

        response::create_in_tx(
            &mut tx,
            response_id,
            Some(conversation_id),
            // A conversation turn never continues a previous response.
            None,
            Some(&history_item_ids_json),
            snapshot_json.as_deref(),
            Some(&metadata_json),
        )
        .await?;
        conversation::set_latest_response_in_tx(&mut tx, conversation_id, response_id).await?;
        tx.commit().await?;

        Ok(())
    }

    /// Retrieve a conversation owned by the tenant.
    ///
    /// # Errors
    /// Returns an error if the resource is missing or the query fails.
    pub async fn retrieve(&self, tenant_id: &str, id: &str) -> StoreResult<ConversationData> {
        conversation::get_by_tenant(self.pool()?, tenant_id, id)
            .await?
            .ok_or_else(|| StorageError::not_found("Conversation", id))?
            .try_into()
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
        let metadata = encode_metadata(&metadata)?;
        conversation::update_metadata(self.pool()?, tenant_id, id, &metadata)
            .await
            .map_err(|error| conversation_error(error, id))?
            .try_into()
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
    pub async fn create_items(&self, tenant_id: &str, id: &str, items: Vec<InOutItem>) -> StoreResult<Vec<InOutItem>> {
        let items = items.iter().map(encode).collect::<StoreResult<Vec<_>>>()?;

        let mut tx = self.pool()?.begin().await?;
        lock_owned(&mut tx, tenant_id, id).await?;
        let rows = item::create_in_tx(&mut tx, items, Some(id)).await?;
        tx.commit().await?;
        rows.iter().map(decode_public).collect()
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
    ) -> StoreResult<Vec<InOutItem>> {
        let rows = match item::list_for_conversation(self.pool()?, tenant_id, id, limit, after, order).await {
            Ok(rows) => rows,
            Err(sqlx::Error::RowNotFound) => {
                // A cursor outside the page's conversation; a missing conversation is reported first.
                self.retrieve(tenant_id, id).await?;
                return Err(StorageError::ItemCursorNotFound {
                    id: after.unwrap_or_default().to_owned(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        // An empty page comes from an empty conversation or a missing one; only then is the conversation looked up.
        if rows.is_empty() {
            self.retrieve(tenant_id, id).await?;
        }
        rows.iter().map(decode_public).collect()
    }

    /// Retrieve one item belonging to the authorized conversation.
    ///
    /// # Errors
    /// Returns an error if the item is missing or the query fails.
    pub async fn retrieve_item(&self, tenant_id: &str, id: &str, item_id: &str) -> StoreResult<InOutItem> {
        let row = item::get_for_conversation(self.pool()?, tenant_id, id, item_id)
            .await?
            .ok_or_else(|| StorageError::not_found("Conversation item", item_id))?;
        decode_public(&row)
    }

    /// Remove an item from conversation history.
    ///
    /// # Errors
    /// Returns an error if the resource is missing or the transaction fails.
    pub async fn delete_item(&self, tenant_id: &str, id: &str, item_id: &str) -> StoreResult<ConversationData> {
        let mut tx = self.pool()?.begin().await?;
        let row = lock_owned(&mut tx, tenant_id, id).await?;
        if item::detach_from_conversation_in_tx(&mut tx, id, Some(item_id)).await? == 0 {
            return Err(StorageError::not_found("Conversation item", item_id));
        }
        tx.commit().await?;
        row.try_into()
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
