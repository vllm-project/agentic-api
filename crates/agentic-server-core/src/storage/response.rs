//! Response storage operations and queries.

use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::Arc;

use super::models::{item, response};
use super::pool::DbPool;
use super::types::{InOutItem, ResponseData, ResponseMetadata, StorageError, StoreResult};
use crate::utils::common::{serialize_to_string, uuid7_str};

/// Response storage operations.
#[derive(Clone, Debug)]
pub struct ResponseStore {
    pool: Option<Arc<DbPool>>,
}

impl ResponseStore {
    /// Creates a disabled response store (no persistence).
    ///
    /// Useful for testing or when response storage is not configured.
    #[must_use]
    pub fn disabled() -> Self {
        Self { pool: None }
    }

    /// Creates a new response store with database pool.
    ///
    /// # Arguments
    ///
    /// * `pool` - Connection pool for database access
    #[must_use]
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self { pool: Some(pool) }
    }

    /// Returns a reference to the database pool.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotConfigured`] if store is disabled (no pool configured).
    fn pool(&self) -> StoreResult<&DbPool> {
        self.pool.as_deref().ok_or(StorageError::NotConfigured)
    }

    /// Retrieves a response by ID.
    ///
    /// # Errors
    ///
    /// Returns error if response not found, database query fails, or store is disabled.
    pub async fn get(&self, response_id: &str) -> StoreResult<ResponseData> {
        self.get_for_tenant(response_id, None).await
    }

    /// Retrieves a response within the optional tenant scope.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotConfigured`] if storage is disabled, a not-found
    /// error if the response is outside the tenant scope or absent, or a database
    /// error if the query fails.
    pub async fn get_for_tenant(&self, response_id: &str, tenant_id: Option<&str>) -> StoreResult<ResponseData> {
        let pool = self.pool()?;
        let row = response::get_for_tenant(pool, response_id, tenant_id)
            .await?
            .ok_or_else(|| StorageError::not_found("Response", response_id))?;
        Ok(row.into())
    }

    /// Rehydrates a response with full history.
    ///
    /// Fetches all history items referenced by a response.
    ///
    /// # Errors
    ///
    /// Returns error if database query fails or store is disabled.
    pub async fn rehydrate(&self, response_id: &str) -> StoreResult<Vec<InOutItem>> {
        self.rehydrate_for_tenant(response_id, None).await
    }

    /// Rehydrates a response's ordered item history within the optional tenant scope.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotConfigured`] if storage is disabled, a not-found
    /// error if the response is outside the tenant scope or absent, or a database
    /// error if the response or its items cannot be queried.
    pub async fn rehydrate_for_tenant(
        &self,
        response_id: &str,
        tenant_id: Option<&str>,
    ) -> StoreResult<Vec<InOutItem>> {
        let pool = self.pool()?;
        let response = self.get_for_tenant(response_id, tenant_id).await?;
        let rows = item::get_items_for_tenant(pool, &response.history_item_ids, tenant_id).await?;
        let mut items_by_id: HashMap<String, InOutItem> = rows
            .into_iter()
            .filter_map(|row| {
                let id = row.id.clone();
                row.as_inout().map(|item| (id, item))
            })
            .collect();

        let ordered_items = response
            .history_item_ids
            .iter()
            .filter_map(|id| items_by_id.remove(id))
            .collect();

        Ok(ordered_items)
    }

    /// Persists a response with its items and metadata.
    ///
    /// Creates items and stores the associated response record.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if database operation fails or store is disabled.
    pub async fn persist(
        &self,
        response_id: &str,
        previous_response_id: Option<&str>,
        new_items: Vec<InOutItem>,
        metadata: &ResponseMetadata,
    ) -> StoreResult<()> {
        self.persist_with_conversation_id_for_tenant(response_id, None, previous_response_id, new_items, metadata, None)
            .await
    }

    pub(crate) async fn persist_with_conversation_id_for_tenant(
        &self,
        response_id: &str,
        conversation_id: Option<&str>,
        previous_response_id: Option<&str>,
        new_items: Vec<InOutItem>,
        metadata: &ResponseMetadata,
        tenant_id: Option<&str>,
    ) -> StoreResult<()> {
        let pool = self.pool()?;

        let mut item_ids: Vec<String> = match previous_response_id {
            Some(prev_id) => self.get_for_tenant(prev_id, tenant_id).await?.history_item_ids,
            None => Vec::new(),
        };
        let mut items_: Vec<(String, String)> = Vec::new();
        for any_item in new_items {
            let item_id = uuid7_str("item_");
            item_ids.push(item_id.clone());
            let data_str = String::try_from(&any_item)?;
            items_.push((item_id, data_str));
        }
        let history_item_ids_json = serialize_to_string(&item_ids)?;
        let metadata_json = String::try_from(metadata)?;

        let mut tx = pool.begin().await?;

        item::create_in_tx_with_tenant(&mut tx, items_, None, tenant_id).await?;

        response::create_in_tx_with_tenant(
            &mut tx,
            response_id,
            conversation_id,
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

#[cfg(test)]
mod tests {
    use super::super::types::ResponseMetadata;
    use super::*;

    #[test]
    fn test_response_store_disabled() {
        let store = ResponseStore::disabled();
        assert!(store.pool().is_err());
    }

    #[test]
    fn test_response_metadata_default() {
        let meta = ResponseMetadata::default();
        assert!(meta.model.is_empty());
        assert!(meta.previous_response_id.is_none());
    }
}
