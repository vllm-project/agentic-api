//! Response storage handler — owns all response store operations.

use std::collections::HashSet;

use crate::storage::{ResponseData, ResponseMetadata, ResponseStore, StorageError};
use crate::types::io::OutputItem;
use crate::types::io::{InOutItem, InputItem};
use crate::types::io::{ItemOrder, ItemPage, ListItemsResponse};
use crate::types::request_response::ResponsePayload;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::request::RequestContext;

/// The conflict for a response stored without a snapshot, such as a `store: false` session checkpoint.
const NOT_RETRIEVABLE: &str = "stored response has no retrievable payload; create a new response with store=true";

/// Handles all response store operations: lookup, rehydration, and persistence.
#[derive(Clone, Debug)]
pub struct ResponseHandler {
    store: ResponseStore,
}

impl ResponseHandler {
    #[must_use]
    pub fn new(store: ResponseStore) -> Self {
        Self { store }
    }

    /// Retrieves the terminal payload of a response created with `store=true`.
    ///
    /// # Errors
    /// Returns `ResponseNotFound` for missing IDs, a storage error for unavailable storage, and a conflict
    /// for older records that contain continuation history but no response snapshot.
    pub async fn get_snapshot(&self, response_id: &str) -> ExecutorResult<ResponsePayload> {
        self.store
            .get(response_id)
            .await?
            .metadata
            .response_snapshot
            .map(|snapshot| *snapshot)
            .ok_or_else(|| ExecutorError::Conflict(NOT_RETRIEVABLE.into()))
    }

    /// Deletes a stored response.
    ///
    /// # Errors
    /// Returns an error if the response is missing or storage is unavailable.
    pub async fn delete(&self, response_id: &str) -> ExecutorResult<()> {
        self.store.delete(response_id).await.map_err(ExecutorError::Storage)
    }

    /// Resolve client references without overwriting the stored item with supplied content.
    pub(crate) async fn resolve_input_items(&self, items: &mut [InOutItem]) -> ExecutorResult<()> {
        let mut seen = HashSet::new();
        for (index, item) in items.iter_mut().enumerate() {
            let Some(id) = item.id() else { continue };
            if !seen.insert(id.to_owned()) {
                return Err(ExecutorError::UnsupportedValue {
                    param: "input".into(),
                    code: None,
                    message: format!(
                        "Duplicate item found with id {id}. Remove duplicate items from your input and try again."
                    ),
                });
            }
            // Validate message identities at admission, never while persisting upstream outputs.
            if matches!(
                item,
                InOutItem::Input(InputItem::Message(_)) | InOutItem::Output(OutputItem::Message(_))
            ) {
                item.validate_client_id(format!("input[{index}].id"))?;
            }
            if let Some(stored) = self.store.get_input_item(id).await? {
                *item = stored.into_replayed_input();
            }
        }
        Ok(())
    }

    /// Lists stored context, excluding this response's own output.
    ///
    /// # Errors
    /// Returns an error for invalid pagination, missing history, or a legacy response
    /// without a retrievable snapshot.
    pub async fn list_input_items(&self, response_id: &str, page: ItemPage<'_>) -> ExecutorResult<ListItemsResponse> {
        let (stored, mut items) = self.store.get_items(response_id).await?;
        let snapshot = stored
            .metadata
            .response_snapshot
            .ok_or_else(|| ExecutorError::Conflict(NOT_RETRIEVABLE.into()))?;
        // Both persistence paths append public outputs after the turn's inputs.
        // Bound the suffix by this snapshot's output count so an inherited output
        // remains input even when a turn supplies no new items. Session execution
        // can already have recorded some outputs as canonical input history.
        for _ in &snapshot.output {
            if !items.last().is_some_and(|item| matches!(item, InOutItem::Output(_))) {
                break;
            }
            items.pop();
        }
        if page.order == ItemOrder::Desc {
            items.reverse();
        }
        let start = match page.after.filter(|cursor| !cursor.is_empty()) {
            Some(cursor) => {
                // Match conversation pagination when a public ID occurs repeatedly.
                items
                    .iter()
                    .rposition(|item| item.id() == Some(cursor))
                    .ok_or_else(|| StorageError::ItemCursorNotFound { id: cursor.to_owned() })?
                    + 1
            }
            None => 0,
        };
        let limit = page.limit.get();
        let has_more = items.len() - start > limit;
        Ok(ListItemsResponse::new(
            items.into_iter().skip(start).take(limit).collect(),
            has_more,
        ))
    }

    /// Retrieves the stored response for `previous_response_id`.
    ///
    /// Reads `previous_response_id` from `ctx.original_request`.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `previous_response_id` is absent, the response
    /// is not found, the store is disabled, or the database query fails.
    pub async fn get(&self, ctx: &RequestContext) -> ExecutorResult<ResponseData> {
        let prev_id = ctx
            .original_request
            .previous_response_id
            .as_deref()
            .ok_or_else(|| ExecutorError::InvalidRequest("previous_response_id is required for get".into()))?;
        let mut stored = self.store.get(prev_id).await.map_err(|error| {
            if error.is_not_found() {
                StorageError::PreviousResponseNotFound { id: prev_id.to_owned() }.into()
            } else {
                ExecutorError::Storage(error)
            }
        })?;
        // Continuation checkpoints retain history and settings, not a duplicate payload.
        stored.metadata.response_snapshot = None;
        Ok(stored)
    }

    /// Validates that the response for `previous_response_id` exists.
    ///
    /// Used in the `store=false` path where we only need to confirm the ID is
    /// valid without loading any history.
    ///
    /// # Errors
    /// Returns `ExecutorError` if `previous_response_id` is absent, the response
    /// is not found, or the store is disabled.
    pub async fn validate_exists(&self, ctx: &RequestContext) -> ExecutorResult<()> {
        self.get(ctx).await.map(|_| ())
    }

    /// Loads the previous response and its full history in one storage query.
    ///
    /// Returns `None` when the request does not continue a previous response. The stored response
    /// snapshot is dropped: continuation needs only the effective settings.
    ///
    /// # Errors
    /// Returns [`StorageError::PreviousResponseNotFound`] for an unknown `previous_response_id`, or
    /// `ExecutorError` if the store is disabled or the history is invalid.
    pub async fn rehydrate(&self, ctx: &RequestContext) -> ExecutorResult<Option<(ResponseData, Vec<InOutItem>)>> {
        let Some(prev_id) = ctx.original_request.previous_response_id.as_deref() else {
            return Ok(None);
        };
        let (mut stored, history) = self.store.rehydrate(prev_id).await.map_err(|error| {
            if error.is_not_found() {
                StorageError::PreviousResponseNotFound { id: prev_id.to_owned() }.into()
            } else {
                ExecutorError::Storage(error)
            }
        })?;
        stored.metadata.response_snapshot = None;
        Ok(Some((stored, history)))
    }

    /// Completes response-scoped persistence and optional transient checkpoint retention.
    ///
    /// Normally only this turn's items are inserted; promoting a transient parent to a stored child requires its
    /// full canonical history because there are no durable parent item IDs to reference.
    ///
    /// # Errors
    /// Returns `ExecutorError` if the store is disabled or the database operation fails.
    pub async fn persist(
        &self,
        mut ctx: RequestContext,
        output_items: Vec<OutputItem>,
        mut metadata: ResponseMetadata,
    ) -> ExecutorResult<()> {
        let continuation = ctx.continuation.take();
        let write_durable = continuation.is_none() || ctx.original_request.store;
        let mut new_items = Vec::with_capacity(ctx.new_input_items.len() + output_items.len());
        let output_items: Vec<_> = output_items
            .into_iter()
            .map(|item| {
                let recorded = item.is_already_in_history(&ctx.new_input_items);
                (item, recorded)
            })
            .collect();
        new_items.extend(ctx.new_input_items);
        new_items.extend(
            output_items
                .into_iter()
                .enumerate()
                .filter(|(index, (item, recorded))| {
                    !recorded
                        && continuation
                            .as_ref()
                            .is_none_or(|lease| lease.retains_output(*index, item))
                })
                .map(|(_, (item, _))| InOutItem::Output(item)),
        );

        let snapshot = metadata.response_snapshot.take();
        let checkpoint = continuation
            .as_ref()
            .map(|lease| lease.checkpoint(ctx.response_id.clone(), &metadata, &new_items, write_durable))
            .transpose()?;

        metadata.response_snapshot = snapshot;

        // A stored child of a transient parent needs its complete canonical
        // checkpoint, not a database reference to a response that was never stored.
        let transient_parent = continuation
            .as_ref()
            .filter(|lease| lease.parent.as_ref().is_some_and(|parent| !parent.durable));
        let previous_durable_id = if let Some(lease) = transient_parent {
            if write_durable {
                let mut full_items = lease.parent_items().cloned().map(InOutItem::Input).collect::<Vec<_>>();
                full_items.append(&mut new_items);
                new_items = full_items;
            }
            None
        } else {
            metadata.previous_response_id.as_deref()
        };

        let result = if write_durable {
            self.store
                .persist(&ctx.response_id, previous_durable_id, new_items, &metadata)
                .await
        } else {
            Ok(())
        };
        match result {
            Err(error) if error.is_unique_violation() => match self.store.get(&ctx.response_id).await {
                Ok(_) => Err(ExecutorError::Conflict(format!(
                    "a turn is already stored under '{}'",
                    ctx.response_id
                ))),
                Err(not_found) if not_found.is_not_found() => Err(ExecutorError::Storage(error)),
                Err(lookup_error) => Err(ExecutorError::Storage(lookup_error)),
            },
            Err(error) => Err(ExecutorError::Storage(error)),
            Ok(()) => {
                if let Some((lease, checkpoint)) = continuation.zip(checkpoint) {
                    lease.publish(checkpoint)?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::ResponsesInput;
    use crate::types::request_response::RequestPayload;

    fn disabled_handler() -> ResponseHandler {
        ResponseHandler::new(ResponseStore::disabled())
    }

    fn make_ctx(previous_response_id: Option<&str>) -> RequestContext {
        let req = RequestPayload {
            model: "test".into(),
            input: ResponsesInput::Text("hi".into()),
            store: true,
            previous_response_id: previous_response_id.map(str::to_string),
            ..Default::default()
        };
        RequestContext {
            multi_agent_tree: None,
            enriched_request: req.clone(),
            original_request: req,
            new_input_items: vec![],
            response_id: "resp_test".into(),
            conversation_id: None,
            conversation_version: None,
            rehydrated_metadata: None,
            continuation: None,
        }
    }

    #[tokio::test]
    async fn test_get_missing_prev_id_returns_error() {
        let result = disabled_handler().get(&make_ctx(None)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_validate_exists_missing_prev_id_returns_error() {
        let result = disabled_handler().validate_exists(&make_ctx(None)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rehydrate_no_prev_id_returns_none() {
        let result = disabled_handler().rehydrate(&make_ctx(None)).await;
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_rehydrate_disabled_store_returns_error() {
        let result = disabled_handler().rehydrate(&make_ctx(Some("resp_prev"))).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_persist_disabled_store_returns_error() {
        let result = disabled_handler()
            .persist(make_ctx(None), vec![], ResponseMetadata::default())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn persisted_metadata_keeps_public_selector_after_inference_relaxation() {
        use crate::storage::create_pool_with_schema;
        use crate::types::io::ToolChoice;

        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await.unwrap();
        let store = ResponseStore::new(pool);
        let handler = ResponseHandler::new(store.clone());
        let mut ctx = make_ctx(None);
        ctx.enriched_request.tool_choice = Some(ToolChoice::Required);
        ctx.relax_tool_choice();
        assert_eq!(ctx.enriched_request.tool_choice, Some(ToolChoice::Auto));

        let conversations = crate::executor::ConversationHandler::new(crate::storage::ConversationStore::disabled());
        crate::executor::persist::persist_prepared_turn(ctx, None, vec![], None, &conversations, &handler)
            .await
            .unwrap();

        let stored = store.get("resp_test").await.unwrap();
        assert_eq!(stored.metadata.effective_tool_choice, ToolChoice::Required);
    }
}
