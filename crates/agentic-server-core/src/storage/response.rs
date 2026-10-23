//! Response storage operations and queries.

use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::Arc;

use super::models::{item, response};
use super::pool::DbPool;
use super::types::{ResponseData, ResponseMetadata, StorageError, StoreResult};
use crate::types::io::InOutItem;
use crate::utils::common::{deserialize_from_str, serialize_to_string, uuid7_str};

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
    /// Returns an error if the response is missing or invalid, the database query
    /// fails, or the store is disabled.
    pub async fn get(&self, response_id: &str) -> StoreResult<ResponseData> {
        let pool = self.pool()?;
        let row = response::get(pool, response_id)
            .await?
            .ok_or_else(|| StorageError::not_found("Response", response_id))?;
        row.try_into()
    }

    /// Deletes a response without deleting shared history items.
    ///
    /// # Errors
    /// Returns an error if the response is missing, storage is disabled, or deletion fails.
    pub async fn delete(&self, response_id: &str) -> StoreResult<()> {
        let mut tx = self.pool()?.begin().await?;
        if !response::delete_in_tx(&mut tx, response_id).await? {
            return Err(StorageError::not_found("Response", response_id));
        }
        tx.commit().await?;
        Ok(())
    }

    /// Resolve a supplied public item ID when storage is configured.
    ///
    /// # Errors
    /// Returns an error if the database query fails.
    pub async fn get_input_item(&self, id: &str) -> StoreResult<Option<item::Item>> {
        let Some(pool) = self.pool.as_deref() else {
            return Ok(None);
        };
        Ok(item::get_by_public_id(pool, id).await?)
    }

    /// Loads the response's existing item rows in history order.
    ///
    /// # Errors
    /// Returns an error for missing or invalid history, unavailable storage, or query failure.
    pub async fn get_items(&self, response_id: &str) -> StoreResult<Vec<item::Item>> {
        let pool = self.pool()?;
        let json = response::get_item_ids(pool, response_id)
            .await?
            .ok_or_else(|| StorageError::not_found("Response", response_id))?;
        let ids: Vec<String> = deserialize_from_str(&json).map_err(|_| StorageError::InvalidResponseHistory {
            response_id: response_id.to_owned(),
        })?;
        let mut rows: HashMap<_, _> = item::get_items(pool, &ids)
            .await?
            .into_iter()
            .map(|row| (row.id.clone(), row))
            .collect();
        ids.into_iter()
            .map(|id| rows.remove(&id).ok_or(StorageError::InvalidHistoryItem { item_id: id }))
            .collect()
    }

    /// Rehydrates a response with full history.
    ///
    /// Fetches all history items referenced by a response.
    ///
    /// # Errors
    ///
    /// Returns an error if a history item is missing or invalid, the database query
    /// fails, or the store is disabled.
    pub async fn rehydrate(&self, response_id: &str) -> StoreResult<Vec<InOutItem>> {
        let pool = self.pool()?;
        let response = self.get(response_id).await?;
        let rows = item::get_items(pool, &response.history_item_ids).await?;
        let mut items_by_id: HashMap<String, InOutItem> = rows
            .into_iter()
            .map(|row| {
                let item = InOutItem::try_from(&row)?;
                Ok((row.id, item))
            })
            .collect::<StoreResult<_>>()?;

        let ordered_items = response
            .history_item_ids
            .iter()
            .map(|id| {
                items_by_id
                    .remove(id)
                    .ok_or_else(|| StorageError::InvalidHistoryItem { item_id: id.clone() })
            })
            .collect::<StoreResult<_>>()?;

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
        self.persist_with_conversation_id(response_id, None, previous_response_id, new_items, metadata)
            .await
    }

    /// Persists a response with its optional conversation association.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if database operation fails or store is disabled.
    pub(crate) async fn persist_with_conversation_id(
        &self,
        response_id: &str,
        conversation_id: Option<&str>,
        previous_response_id: Option<&str>,
        new_items: Vec<InOutItem>,
        metadata: &ResponseMetadata,
    ) -> StoreResult<()> {
        let pool = self.pool()?;

        let mut item_ids: Vec<String> = match previous_response_id {
            Some(prev_id) => self.get(prev_id).await?.history_item_ids,
            None => Vec::new(),
        };
        let new_items = new_items
            .into_iter()
            .map(|item| {
                let prefix = match &item {
                    InOutItem::Input(input) => input.id_prefix(),
                    InOutItem::Output(output) => output.id_prefix(),
                };
                if item.id() == Some("") {
                    return Err(StorageError::Validation("item id cannot be an empty string".to_owned()));
                }
                let id = item
                    .id()
                    .map_or_else(|| uuid7_str(prefix.unwrap_or("item_")), str::to_owned);
                let data = String::try_from(&item)?;
                Ok((id, data))
            })
            .collect::<StoreResult<Vec<_>>>()?;

        let metadata_json = String::try_from(metadata)?;

        let mut tx = pool.begin().await?;

        // History rows get their own row IDs, so repeated public IDs keep every position.
        let created = item::create_in_tx(&mut tx, new_items, None).await?;
        item_ids.extend(created.into_iter().map(|item| item.id));
        let history_item_ids_json = serialize_to_string(&item_ids)?;

        response::create_in_tx(
            &mut tx,
            response_id,
            conversation_id,
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
    use crate::storage::create_pool_with_schema;
    use crate::storage::types::item::STORED_CODE_INTERPRETER_ORIGIN_KEY;
    use crate::types::event::MessageStatus;
    use crate::types::io::code_interpreter::CodeInterpreterCallOrigin;
    use crate::types::io::{
        CodeInterpreterCall, CodeInterpreterCallStatus, InputItem, OutputItem, OutputMessage, ResponsesInput,
    };
    use serde_json::Value;

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
                &ResponseMetadata::default(),
            )
            .await
            .unwrap();
        let rows = store.get_items("resp_items").await.unwrap();
        assert_eq!(rows[0].public_id(), "msg_supplied");
        assert_eq!(rows[1].public_id(), "msg_generated");
        assert!(rows[2].public_id().starts_with("msg_"));
        assert_eq!(rows[3].public_id(), "fc_supplied");
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
            origin: CodeInterpreterCallOrigin::default(),
        });
        store
            .persist(
                "resp_items",
                None,
                vec![InOutItem::Output(output)],
                &ResponseMetadata::default(),
            )
            .await
            .expect("serialize code interpreter response history");

        let rows = store.get_items("resp_items").await.unwrap();
        assert_eq!(rows[0].public_id(), "ci_public");
        let data: Value = serde_json::from_str(&rows[0].data).expect("stored output JSON");
        assert_eq!(data["id"], "ci_public");
        assert_eq!(data["type"], "code_interpreter_call");
    }

    #[tokio::test]
    async fn code_interpreter_history_replays_native_call_without_gateway_duplicate() {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await.unwrap();
        let store = ResponseStore::new(pool);
        let gateway_call: InputItem = serde_json::from_value(serde_json::json!({
            "type": "function_call",
            "id": "fc_gateway",
            "call_id": "call_gateway",
            "name": "code_interpreter",
            "arguments": "{\"code\":\"print(42)\"}",
            "status": "completed"
        }))
        .expect("gateway model-facing call");
        let gateway_result: InputItem = serde_json::from_value(serde_json::json!({
            "type": "function_call_output",
            "call_id": "call_gateway",
            "output": "42"
        }))
        .expect("gateway model-facing output");
        let native_call: OutputItem = serde_json::from_value(serde_json::json!({
            "type": "code_interpreter_call",
            "id": "ci_native",
            "container_id": "cntr_native",
            "code": "print(7)",
            "status": "completed",
            "outputs": [{"type": "logs", "logs": "7\n"}]
        }))
        .expect("native upstream call");
        let mut gateway_projection = native_call.clone();
        let OutputItem::CodeInterpreterCall(call) = &mut gateway_projection else {
            panic!("code interpreter call");
        };
        call.id = "ci_gateway".to_owned();
        call.container_id = "cntr_gateway".to_owned();
        call.origin = CodeInterpreterCallOrigin::Gateway;

        store
            .persist(
                "resp_items",
                None,
                vec![
                    InOutItem::Input(gateway_call),
                    InOutItem::Input(gateway_result),
                    InOutItem::Output(gateway_projection),
                    InOutItem::Output(native_call),
                ],
                &ResponseMetadata::default(),
            )
            .await
            .expect("serialize response history");
        let stored = store.get_items("resp_items").await.unwrap();
        let gateway_data: Value = serde_json::from_str(&stored[2].data).expect("stored gateway call");
        let native_data: Value = serde_json::from_str(&stored[3].data).expect("stored native call");
        assert_eq!(gateway_data[STORED_CODE_INTERPRETER_ORIGIN_KEY], "gateway");
        assert!(native_data.get(STORED_CODE_INTERPRETER_ORIGIN_KEY).is_none());

        let history = stored
            .into_iter()
            .map(|row| row.as_inout().expect("rehydrate stored item"))
            .collect();
        let inputs = InOutItem::into_input_items(history);
        assert_eq!(inputs.len(), 3);
        assert!(matches!(&inputs[0], InputItem::FunctionCall(call) if call.call_id == "call_gateway"));
        assert!(matches!(&inputs[1], InputItem::FunctionCallOutput(output) if output.call_id == "call_gateway"));
        assert!(
            matches!(&inputs[2], InputItem::CodeInterpreterCall(call) if call.id == "ci_native"
            && call.code == "print(7)" && call.outputs.as_ref().is_some_and(|outputs| outputs.len() == 1))
        );
        let model_input =
            serde_json::to_value(ResponsesInput::from(inputs).model_input()).expect("model input serializes");
        assert_eq!(model_input[2]["type"], "code_interpreter_call");
        assert!(model_input[2].get(STORED_CODE_INTERPRETER_ORIGIN_KEY).is_none());
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
                &ResponseMetadata::default(),
            )
            .await;
        assert!(result.is_ok());
        result.unwrap();
        let rows = store.get_items("resp_items").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].public_id().starts_with("msg_"));
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
