//! Paginated vector-store listing with current attachment aggregates.

use std::collections::HashMap;

use super::{Collection, FileSearchError, FileSearchStorage, ListParams, VectorStoreObject};

impl FileSearchStorage {
    /// Returns one keyset page with current aggregate counts, using one
    /// attachment query for the page instead of one query per store.
    pub(crate) async fn list_stores(&self, params: &ListParams) -> Result<Vec<VectorStoreObject>, FileSearchError> {
        let mut stores: Vec<VectorStoreObject> = self.list(Collection::Stores, None, params).await?;
        if stores.is_empty() {
            return Ok(stores);
        }
        let placeholders = (1..=stores.len())
            .map(|index| format!("${index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT store_id, COUNT(*), CAST(COALESCE(SUM(usage_bytes), 0) AS BIGINT) \
             FROM file_search_attachments WHERE store_id IN ({placeholders}) GROUP BY store_id"
        );
        let mut query = sqlx::query_as::<_, (String, i64, i64)>(&sql);
        for store in &stores {
            query = query.bind(&store.id);
        }
        let counts: HashMap<_, _> = query
            .fetch_all(self.pool.as_ref())
            .await?
            .into_iter()
            .map(|(id, count, bytes)| (id, (count, bytes)))
            .collect();
        for store in &mut stores {
            let (count, bytes) = counts.get(&store.id).copied().unwrap_or_default();
            store.file_counts.completed = count;
            store.file_counts.total = count;
            store.usage_bytes = bytes;
        }
        Ok(stores)
    }
}
