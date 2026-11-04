//! Response storage operations and queries.

use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::Arc;

use super::models::{item, response};
use super::pool::DbPool;
use super::types::item::{decode_public, encode};
use super::types::{ResponseData, ResponseMetadata, StorageError, StoreResult};
use crate::types::io::InOutItem;
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
            .ok_or_else(|| StorageError::ResponseNotFound {
                id: response_id.to_owned(),
            })?;
        row.try_into()
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

    /// Resolve a supplied public item ID to its stored public item when storage is configured.
    ///
    /// # Errors
    /// Returns an error if the database query fails or the stored item is invalid.
    pub async fn get_input_item(&self, id: &str) -> StoreResult<Option<InOutItem>> {
        let Some(pool) = self.pool.as_deref() else {
            return Ok(None);
        };
        item::get_by_public_id(pool, id)
            .await?
            .as_ref()
            .map(decode_public)
            .transpose()
    }

    /// Loads a response together with its public history items in history order.
    ///
    /// # Errors
    /// Returns an error for missing or invalid history, unavailable storage, or query failure.
    pub async fn get_items(&self, response_id: &str) -> StoreResult<(ResponseData, Vec<InOutItem>)> {
        let (response, rows) = self.history(response_id).await?;
        let items = rows.iter().map(decode_public).collect::<StoreResult<_>>()?;
        Ok((response, items))
    }

    /// Loads a response together with its full history.
    ///
    /// # Errors
    ///
    /// Returns an error if the response or a history item is missing or invalid, the database query
    /// fails, or the store is disabled.
    pub async fn rehydrate(&self, response_id: &str) -> StoreResult<(ResponseData, Vec<InOutItem>)> {
        let (response, rows) = self.history(response_id).await?;
        let items = rows.iter().map(InOutItem::try_from).collect::<StoreResult<_>>()?;
        Ok((response, items))
    }

    /// The response and its history rows in history order; a missing row is invalid history.
    async fn history(&self, response_id: &str) -> StoreResult<(ResponseData, Vec<item::Item>)> {
        let response = self.get(response_id).await?;
        let mut rows: HashMap<_, _> = item::get_items(self.pool()?, &response.history_item_ids)
            .await?
            .into_iter()
            .map(|row| (row.id.clone(), row))
            .collect();
        let rows = response
            .history_item_ids
            .iter()
            .map(|id| {
                rows.remove(id)
                    .ok_or_else(|| StorageError::InvalidHistoryItem { item_id: id.clone() })
            })
            .collect::<StoreResult<_>>()?;
        Ok((response, rows))
    }

    /// Persists a response with its new items and metadata, extending the history of `previous_response_id`.
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
        let new_items = new_items.iter().map(encode).collect::<StoreResult<Vec<_>>>()?;
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
        let (_, items) = store.get_items("resp_items").await.unwrap();
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

        let (_, rows) = store.history("resp_items").await.unwrap();
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
        let (_, stored) = store.history("resp_items").await.unwrap();
        let gateway_data: Value = serde_json::from_str(&stored[2].data).expect("stored gateway call");
        let native_data: Value = serde_json::from_str(&stored[3].data).expect("stored native call");
        assert_eq!(gateway_data[STORED_CODE_INTERPRETER_ORIGIN_KEY], "gateway");
        assert!(native_data.get(STORED_CODE_INTERPRETER_ORIGIN_KEY).is_none());

        let (_, history) = store.rehydrate("resp_items").await.unwrap();
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
        let (_, items) = store.get_items("resp_items").await.unwrap();
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
