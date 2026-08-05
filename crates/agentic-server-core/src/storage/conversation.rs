//! Conversation storage operations.

#![allow(clippy::missing_errors_doc)]

use std::convert::TryFrom;
use std::sync::Arc;

use serde_json::Value;

use super::models::{conversation, item, response};
use super::pool::DbPool;
use super::types::{
    ConversationData, ConversationItemData, ConversationItemPage, ConversationSnapshot, ConversationVersion, InOutItem,
    ResponseMetadata, StorageError, StoreResult,
};
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
    fn pool(&self) -> StoreResult<&DbPool> {
        self.pool.as_deref().ok_or(StorageError::NotConfigured)
    }

    /// Creates a new conversation.
    ///
    /// # Errors
    ///
    /// Returns error if database query fails.
    pub async fn create(&self) -> StoreResult<ConversationData> {
        self.create_with_items_for_tenant(None, None, Vec::new()).await
    }

    pub async fn create_with_items_for_tenant(
        &self,
        tenant_id: Option<&str>,
        metadata: Option<&Value>,
        initial_items: Vec<InOutItem>,
    ) -> StoreResult<ConversationData> {
        let pool = self.pool()?;
        let conversation_id = uuid7_str("conv_");
        let metadata_json = metadata.map(serialize_to_string).transpose()?;
        let items = serialize_items(initial_items)?;
        let mut tx = pool.begin().await?;
        let row =
            conversation::create_with_metadata_in_tx(&mut tx, &conversation_id, tenant_id, metadata_json.as_deref())
                .await?;
        if !items.is_empty() {
            item::create_in_tx_with_tenant(&mut tx, items, Some(&conversation_id), tenant_id).await?;
        }
        tx.commit().await?;
        Ok(row.into())
    }

    /// Gets a conversation or creates it if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns error if database query fails.
    pub async fn get_or_create(&self, conversation_id: &str) -> StoreResult<ConversationData> {
        self.get_or_create_for_tenant(conversation_id, None).await
    }

    pub async fn get_or_create_for_tenant(
        &self,
        conversation_id: &str,
        tenant_id: Option<&str>,
    ) -> StoreResult<ConversationData> {
        let pool = self.pool()?;
        let row = conversation::get_or_create_for_tenant(pool, conversation_id, tenant_id).await?;
        Ok(row.into())
    }

    /// Gets a conversation by ID.
    ///
    /// # Errors
    ///
    /// Returns error if conversation not found or database query fails.
    pub async fn get(&self, conversation_id: &str) -> StoreResult<ConversationData> {
        self.get_for_tenant(conversation_id, None).await
    }

    pub async fn get_for_tenant(
        &self,
        conversation_id: &str,
        tenant_id: Option<&str>,
    ) -> StoreResult<ConversationData> {
        let pool = self.pool()?;
        let row = conversation::get_for_tenant(pool, conversation_id, tenant_id)
            .await?
            .ok_or_else(|| StorageError::not_found("Conversation", conversation_id))?;
        Ok(row.into())
    }

    pub async fn update_metadata_for_tenant(
        &self,
        conversation_id: &str,
        tenant_id: Option<&str>,
        metadata: Option<&Value>,
    ) -> StoreResult<ConversationData> {
        let pool = self.pool()?;
        let metadata_json = metadata.map(serialize_to_string).transpose()?;
        let row = conversation::update_metadata(pool, conversation_id, tenant_id, metadata_json.as_deref())
            .await?
            .ok_or_else(|| StorageError::not_found("Conversation", conversation_id))?;
        Ok(row.into())
    }

    pub async fn delete_for_tenant(&self, conversation_id: &str, tenant_id: Option<&str>) -> StoreResult<()> {
        let pool = self.pool()?;
        if !conversation::delete(pool, conversation_id, tenant_id).await? {
            return Err(StorageError::not_found("Conversation", conversation_id));
        }
        Ok(())
    }

    pub async fn append_items_for_tenant(
        &self,
        conversation_id: &str,
        tenant_id: Option<&str>,
        items: Vec<InOutItem>,
    ) -> StoreResult<Vec<ConversationItemData>> {
        let pool = self.pool()?;
        self.get_for_tenant(conversation_id, tenant_id).await?;
        let serialized = serialize_items(items)?;
        let mut tx = pool.begin().await?;
        match conversation::lock_in_tx_for_tenant(&mut tx, conversation_id, tenant_id).await {
            Ok(()) => {}
            Err(sqlx::Error::RowNotFound) => {
                return Err(StorageError::not_found("Conversation", conversation_id));
            }
            Err(error) => return Err(error.into()),
        }
        let rows = item::create_in_tx_with_tenant(&mut tx, serialized, Some(conversation_id), tenant_id).await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(ConversationItemData::from).collect())
    }

    pub async fn list_items_for_tenant(
        &self,
        conversation_id: &str,
        tenant_id: Option<&str>,
        after: Option<&str>,
        limit: usize,
        descending: bool,
    ) -> StoreResult<ConversationItemPage> {
        let pool = self.pool()?;
        self.get_for_tenant(conversation_id, tenant_id).await?;
        let mut rows = item::get_items_by_conversation_for_tenant(pool, conversation_id, tenant_id).await?;
        if descending {
            rows.reverse();
        }
        let start = match after {
            Some(after) => rows
                .iter()
                .position(|row| row.id == after)
                .map(|index| index + 1)
                .ok_or_else(|| StorageError::not_found("Conversation item", after))?,
            None => 0,
        };
        let end = start.saturating_add(limit).min(rows.len());
        let has_more = end < rows.len();
        Ok(ConversationItemPage {
            data: rows[start..end]
                .iter()
                .cloned()
                .map(ConversationItemData::from)
                .collect(),
            has_more,
        })
    }

    pub async fn get_item_for_tenant(
        &self,
        conversation_id: &str,
        item_id: &str,
        tenant_id: Option<&str>,
    ) -> StoreResult<ConversationItemData> {
        let pool = self.pool()?;
        self.get_for_tenant(conversation_id, tenant_id).await?;
        let row = item::get_item(pool, conversation_id, item_id, tenant_id)
            .await?
            .ok_or_else(|| StorageError::not_found("Conversation item", item_id))?;
        Ok(row.into())
    }

    pub async fn delete_item_for_tenant(
        &self,
        conversation_id: &str,
        item_id: &str,
        tenant_id: Option<&str>,
    ) -> StoreResult<()> {
        let pool = self.pool()?;
        self.get_for_tenant(conversation_id, tenant_id).await?;
        if !item::delete(pool, conversation_id, item_id, tenant_id).await? {
            return Err(StorageError::not_found("Conversation item", item_id));
        }
        Ok(())
    }

    /// Rehydrates a conversation with all its items.
    ///
    /// # Errors
    ///
    /// Returns an error if a stored item is missing its sequence number or if the database query fails.
    pub async fn rehydrate(&self, conversation_id: &str) -> StoreResult<Vec<InOutItem>> {
        Ok(self.rehydrate_snapshot(conversation_id).await?.items)
    }

    /// Rehydrates a conversation with its items and storage version.
    ///
    /// # Errors
    ///
    /// Returns an error if a stored item is missing its sequence number or if the database query fails.
    pub async fn rehydrate_snapshot(&self, conversation_id: &str) -> StoreResult<ConversationSnapshot> {
        self.rehydrate_snapshot_for_tenant(conversation_id, None).await
    }

    pub async fn rehydrate_snapshot_for_tenant(
        &self,
        conversation_id: &str,
        tenant_id: Option<&str>,
    ) -> StoreResult<ConversationSnapshot> {
        let pool = self.pool()?;
        self.get_for_tenant(conversation_id, tenant_id).await?;
        let rows = item::get_items_by_conversation_for_tenant(pool, conversation_id, tenant_id).await?;

        let mut last_sequence = None;
        for row in &rows {
            last_sequence = Some(row.seq.ok_or_else(|| StorageError::InvalidConversationSequence {
                conversation_id: conversation_id.to_string(),
                item_id: row.id.clone(),
            })?);
        }

        Ok(ConversationSnapshot {
            items: rows.into_iter().filter_map(|row| row.as_inout()).collect(),
            version: ConversationVersion::from_last_sequence(last_sequence),
        })
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
            None,
            Some(expected_version),
            response_id,
            previous_response_id,
            new_items,
            metadata,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn persist_if_version_for_tenant(
        &self,
        conversation_id: &str,
        tenant_id: Option<&str>,
        expected_version: ConversationVersion,
        response_id: &str,
        previous_response_id: Option<&str>,
        new_items: Vec<InOutItem>,
        metadata: &ResponseMetadata,
    ) -> StoreResult<()> {
        self.persist_impl(
            conversation_id,
            tenant_id,
            Some(expected_version),
            response_id,
            previous_response_id,
            new_items,
            metadata,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn persist_impl(
        &self,
        conversation_id: &str,
        tenant_id: Option<&str>,
        expected_version: Option<ConversationVersion>,
        response_id: &str,
        previous_response_id: Option<&str>,
        new_items: Vec<InOutItem>,
        metadata: &ResponseMetadata,
    ) -> StoreResult<()> {
        let pool = self.pool()?;

        let items_ = serialize_items(new_items)?;
        let item_ids: Vec<String> = items_.iter().map(|(id, _)| id.clone()).collect();
        let history_item_ids_json = serialize_to_string(&item_ids)?;
        let metadata_json = String::try_from(metadata)?;

        let mut tx = pool.begin().await?;

        match conversation::lock_in_tx_for_tenant(&mut tx, conversation_id, tenant_id).await {
            Ok(()) => {}
            Err(sqlx::Error::RowNotFound) => {
                return Err(StorageError::not_found("Conversation", conversation_id));
            }
            Err(error) => return Err(error.into()),
        }
        if let Some(expected_version) = expected_version {
            let current_version = ConversationVersion::from_last_sequence(
                item::last_conversation_sequence_in_tx_for_tenant(&mut tx, conversation_id, tenant_id).await?,
            );
            if current_version != expected_version {
                return Err(StorageError::ConversationConflict {
                    conversation_id: conversation_id.to_owned(),
                });
            }
        }
        item::create_in_tx_with_tenant(&mut tx, items_, Some(conversation_id), tenant_id).await?;

        response::create_in_tx_with_tenant(
            &mut tx,
            response_id,
            Some(conversation_id),
            previous_response_id,
            Some(&history_item_ids_json),
            Some(&metadata_json),
            tenant_id,
        )
        .await?;
        tx.commit().await?;

        Ok(())
    }
}

fn serialize_items(items: Vec<InOutItem>) -> StoreResult<Vec<(String, String)>> {
    items
        .into_iter()
        .map(|item| {
            let id = uuid7_str("item_");
            let data = String::try_from(&item)?;
            Ok((id, data))
        })
        .collect()
}
