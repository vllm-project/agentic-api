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
        let pool = self.pool()?;
        let row = response::get(pool, response_id)
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
        let pool = self.pool()?;
        let response = self.get(response_id).await?;
        let rows = item::get_items(pool, &response.history_item_ids).await?;
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
        let pool = self.pool()?;

        let mut item_ids: Vec<String> = match previous_response_id {
            Some(prev_id) => self.get(prev_id).await?.history_item_ids,
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

        item::create_in_tx(&mut tx, items_, None).await?;

        response::create_in_tx(
            &mut tx,
            response_id,
            None,
            previous_response_id,
            Some(&history_item_ids_json),
            Some(&metadata_json),
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
