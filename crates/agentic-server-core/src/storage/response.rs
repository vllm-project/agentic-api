//! Response storage operations and queries.

use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::Arc;

use super::models::{item, response};
use super::pool::DbPool;
use super::types::item::{decode_public, encode};
use super::types::response::snapshot_output_count;
use super::types::{ResponseCheckpoint, ResponseData, ResponseMetadata, StorageError, StoreResult};
use crate::types::io::InOutItem;
use crate::types::request_response::ResponsePayload;
use crate::utils::common::serialize_to_string;

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

    /// Retrieves and decodes a stored response by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the response is missing or invalid, the database query fails, or the store is disabled.
    pub async fn get(&self, response_id: &str) -> StoreResult<ResponseData> {
        response::get(self.pool()?, response_id)
            .await?
            .ok_or_else(|| StorageError::ResponseNotFound {
                id: response_id.to_owned(),
            })
            .and_then(ResponseData::try_from)
    }

    /// Retrieves the snapshot of a response: the response exactly as returned, or `None` when it was stored
    /// without one (a `store: false` session checkpoint).
    ///
    /// # Errors
    ///
    /// Returns an error if the response is missing, the snapshot is invalid, the database query fails, or the
    /// store is disabled.
    pub async fn get_snapshot(&self, response_id: &str) -> StoreResult<Option<ResponsePayload>> {
        response::get(self.pool()?, response_id)
            .await?
            .ok_or_else(|| StorageError::ResponseNotFound {
                id: response_id.to_owned(),
            })?
            .snapshot
            .as_deref()
            .map(ResponsePayload::try_from)
            .transpose()
    }

    /// Deletes a response without deleting shared history items.
    ///
    /// # Errors
    /// Returns an error if the response is missing, storage is disabled, or deletion fails.
    pub async fn delete(&self, response_id: &str) -> StoreResult<()> {
        let mut tx = self.pool()?.begin().await?;
        if !response::delete_in_tx(&mut tx, response_id).await? {
            return Err(StorageError::ResponseNotFound {
                id: response_id.to_owned(),
            });
        }
        tx.commit().await?;
        Ok(())
    }

    /// Resolve supplied public item IDs to their stored public items, in one query, when storage is configured.
    ///
    /// # Errors
    /// Returns an error if the database query fails or a stored item is invalid.
    pub async fn get_input_items_by_id(&self, ids: &[String]) -> StoreResult<HashMap<String, InOutItem>> {
        let Some(pool) = self.pool.as_deref().filter(|_| !ids.is_empty()) else {
            return Ok(HashMap::new());
        };
        let mut items = HashMap::new();
        for row in item::get_items_by_public_id(pool, ids).await? {
            items.insert(row.public_id.clone(), decode_public(&row)?);
        }
        Ok(items)
    }

    /// Loads the public items a response was generated from: its history without its own output, in history
    /// order. Returns `None` for a response stored without a snapshot, whose output is unknown.
    ///
    /// # Errors
    /// Returns an error for a missing response, missing or invalid history, unavailable storage, or query failure.
    pub async fn get_input_items(&self, response_id: &str) -> StoreResult<Option<Vec<InOutItem>>> {
        let row = response::get(self.pool()?, response_id)
            .await?
            .ok_or_else(|| StorageError::ResponseNotFound {
                id: response_id.to_owned(),
            })?;
        let Some(output_count) = row.snapshot.as_deref().map(snapshot_output_count).transpose()? else {
            return Ok(None);
        };
        let response = ResponseData::try_from(row)?;
        let mut items = self
            .history(&response)
            .await?
            .iter()
            .map(decode_public)
            .collect::<StoreResult<Vec<_>>>()?;
        // Both persistence paths append public outputs after the turn's inputs. Bound the suffix by the snapshot's
        // output count so an inherited output remains input even when a turn supplies no new items.
        for _ in 0..output_count {
            if !items.last().is_some_and(|item| matches!(item, InOutItem::Output(_))) {
                break;
            }
            items.pop();
        }
        Ok(Some(items))
    }

    /// Loads a response's checkpoint together with its full history.
    ///
    /// # Errors
    ///
    /// Returns an error if the response or a history item is missing or invalid, the database query
    /// fails, or the store is disabled.
    pub async fn rehydrate(&self, response_id: &str) -> StoreResult<(ResponseCheckpoint, Vec<InOutItem>)> {
        let response = self.get(response_id).await?;
        let items = self
            .history(&response)
            .await?
            .iter()
            .map(InOutItem::try_from)
            .collect::<StoreResult<_>>()?;
        Ok((response.into(), items))
    }

    /// The history rows of a response, in history order; a missing row is invalid history.
    async fn history(&self, response: &ResponseData) -> StoreResult<Vec<item::Item>> {
        let ids = &response.history_item_ids;
        let mut rows: HashMap<_, _> = item::get_items(self.pool()?, ids)
            .await?
            .into_iter()
            .map(|row| (row.id.clone(), row))
            .collect();
        ids.iter()
            .map(|id| {
                rows.remove(id)
                    .ok_or_else(|| StorageError::InvalidHistoryItem { item_id: id.clone() })
            })
            .collect()
    }

    /// Persists a response with its new items, snapshot and metadata, extending the history of its `parent`, the
    /// checkpoint rehydration captured. Returns the response's history rows, which a child extends.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if database operation fails or store is disabled.
    pub async fn persist(
        &self,
        response_id: &str,
        parent: Option<&ResponseCheckpoint>,
        new_items: Vec<InOutItem>,
        snapshot: Option<&ResponsePayload>,
        metadata: &ResponseMetadata,
    ) -> StoreResult<Vec<String>> {
        let pool = self.pool()?;

        let previous_response_id = parent.and_then(|parent| parent.response_id.as_deref());
        let mut item_ids = parent.map(|parent| parent.history_item_ids.clone()).unwrap_or_default();
        let new_items = new_items.iter().map(encode).collect::<StoreResult<Vec<_>>>()?;
        let snapshot_json = snapshot.map(String::try_from).transpose()?;
        let metadata_json = String::try_from(metadata)?;

        let mut tx = pool.begin().await?;

        // History rows get their own row IDs, so repeated public IDs keep every position.
        let created = item::create_in_tx(&mut tx, new_items, None).await?;
        item_ids.extend(created.into_iter().map(|item| item.id));
        let history_item_ids_json = serialize_to_string(&item_ids)?;

        response::create_in_tx(
            &mut tx,
            response_id,
            None,
            previous_response_id,
            Some(&history_item_ids_json),
            snapshot_json.as_deref(),
            Some(&metadata_json),
        )
        .await?;
        tx.commit().await?;

        Ok(item_ids)
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::ResponseMetadata;
    use super::*;
    use crate::storage::create_pool_with_schema;
    use crate::types::event::MessageStatus;
    use crate::types::io::{
        CodeInterpreterCall, CodeInterpreterCallStatus, InputItem, OutputItem, OutputMessage, ResponsesInput,
    };
    use serde_json::Value;

    /// A retrievable response without output, so its input items are its whole history.
    fn snapshot() -> ResponsePayload {
        serde_json::from_value(serde_json::json!({
            "id": "resp_items", "object": "response", "created_at": 0, "model": "test", "status": "completed",
            "output": []
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn new_items_reuse_public_ids_and_generate_storage_ids() {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await.unwrap();
        let store = ResponseStore::new(pool);
        let input: InputItem = serde_json::from_value(serde_json::json!({
            "type": "message", "id": "msg_supplied", "role": "user", "content": "hello"
        }))
        .unwrap();
        let output = OutputItem::Message(OutputMessage::new("msg_generated", MessageStatus::Completed));
        let without_id: InputItem = serde_json::from_value(serde_json::json!({
            "type": "message", "role": "user", "content": "next"
        }))
        .unwrap();
        let function_call: InputItem = serde_json::from_value(serde_json::json!({
            "type": "function_call", "id": "fc_supplied", "call_id": "call_1",
            "name": "lookup", "arguments": "{}"
        }))
        .unwrap();
        store
            .persist(
                "resp_items",
                None,
                vec![
                    InOutItem::Input(input),
                    InOutItem::Output(output),
                    InOutItem::Input(without_id),
                    InOutItem::Input(function_call),
                ],
                Some(&snapshot()),
                &ResponseMetadata::default(),
            )
            .await
            .unwrap();
        let items = store
            .get_input_items("resp_items")
            .await
            .unwrap()
            .expect("stored snapshot");
        assert_eq!(items[0].id(), Some("msg_supplied"));
        assert_eq!(items[1].id(), Some("msg_generated"));
        assert!(items[2].id().is_some_and(|id| id.starts_with("msg_")));
        assert_eq!(items[3].id(), Some("fc_supplied"));
    }

    #[tokio::test]
    async fn response_history_preserves_code_interpreter_public_id() {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await.unwrap();
        let store = ResponseStore::new(pool);
        let output = OutputItem::CodeInterpreterCall(CodeInterpreterCall {
            agent: None,
            id: "ci_public".to_owned(),
            container_id: "cntr_public".to_owned(),
            code: "print(42)".to_owned(),
            status: CodeInterpreterCallStatus::Completed,
            outputs: Some(vec![]),
        });
        store
            .persist(
                "resp_items",
                None,
                vec![InOutItem::Output(output)],
                None,
                &ResponseMetadata::default(),
            )
            .await
            .expect("serialize code interpreter response history");

        let response = store.get("resp_items").await.unwrap();
        let rows = store.history(&response).await.unwrap();
        assert_eq!(rows[0].public_id, "ci_public");
        let data: Value = serde_json::from_str(&rows[0].data).expect("stored output JSON");
        assert_eq!(data["id"], "ci_public");
        assert_eq!(data["type"], "code_interpreter_call");
    }

    #[tokio::test]
    async fn code_interpreter_history_keeps_the_public_item_and_lowers_it_upstream() {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await.unwrap();
        let store = ResponseStore::new(pool);
        let projection: OutputItem = serde_json::from_value(serde_json::json!({
            "type": "code_interpreter_call",
            "id": "ci_gateway",
            "container_id": "cntr_gateway",
            "code": "print(42)",
            "status": "completed",
            "outputs": [{"type": "logs", "logs": "42\n"}]
        }))
        .expect("public projection");
        store
            .persist(
                "resp_items",
                None,
                vec![InOutItem::Output(projection.clone())],
                None,
                &ResponseMetadata::default(),
            )
            .await
            .expect("serialize response history");

        let (_, history) = store.rehydrate("resp_items").await.unwrap();
        assert_eq!(history, vec![InOutItem::Output(projection)]);
        let inputs = ResponsesInput::from(InOutItem::into_input_items(history));
        let model = Vec::<InputItem>::from(inputs.normalized_model_input(&[]).unwrap());
        assert!(
            matches!(&model[..], [InputItem::FunctionCall(call), InputItem::FunctionCallOutput(output)]
            if call.name == "code_interpreter" && call.call_id == "ci_gateway" && output.call_id == "ci_gateway")
        );
    }

    #[tokio::test]
    async fn create_items_rejects_empty_id() {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await.unwrap();
        let store = ResponseStore::new(pool);
        let input: InputItem = serde_json::from_value(serde_json::json!({
            "type": "message", "id": "", "role": "user", "content": "test"
        }))
        .unwrap();
        let result = store
            .persist(
                "resp_items",
                None,
                vec![InOutItem::Input(input)],
                None,
                &ResponseMetadata::default(),
            )
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_validation());
        assert!(matches!(err, StorageError::Validation(_)));
    }

    #[tokio::test]
    async fn create_items_accepts_missing_id() {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await.unwrap();
        let store = ResponseStore::new(pool);
        let input: InputItem = serde_json::from_value(serde_json::json!({
            "type": "message", "role": "user", "content": "test"
        }))
        .unwrap();
        let result = store
            .persist(
                "resp_items",
                None,
                vec![InOutItem::Input(input)],
                Some(&snapshot()),
                &ResponseMetadata::default(),
            )
            .await;
        assert!(result.is_ok());
        result.unwrap();
        let items = store
            .get_input_items("resp_items")
            .await
            .unwrap()
            .expect("stored snapshot");
        assert_eq!(items.len(), 1);
        assert!(items[0].id().is_some_and(|id| id.starts_with("msg_")));
    }

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
