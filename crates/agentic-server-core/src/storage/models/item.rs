//! Conversation history item stored in the database.

mod legacy_reasoning;

use serde::Deserialize;
use serde_json::Value;
use std::convert::TryFrom;
use std::fmt::Write;
use tracing::warn;

use super::super::pool::{DbPool, DbResult, DbTransaction};
use super::super::types::item::{InOutItem, ItemKind, STORED_ITEM_KIND_KEY};
use crate::storage::{StorageError, StoreResult};
use crate::types::conversations::ItemOrder;
use crate::types::io::{InputItem, OutputItem};
use crate::utils::common::{deserialize_from_str_opt, utcnow_str, uuid7_str};

const ITEM_COLUMN_COUNT: usize = 5;
const SEQUENCE_COLUMN_INDEX: usize = 4;
const MAX_BIND_PARAMETERS: usize = 999;
const MAX_ITEMS_PER_INSERT: usize = (MAX_BIND_PARAMETERS - 2) / (ITEM_COLUMN_COUNT + 1);

/// Conversation history item stored in the database.
///
/// Maps to the `items` table and represents a single message/event
/// in a conversation timeline.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Item {
    /// Unique database identity of this history entry.
    pub id: String,

    /// Original public item identity when this row references an existing item.
    pub reference_id: Option<String>,

    /// Item data stored as JSON text.
    /// Deserialized based on context (`message`, `tool_call`, etc.)
    pub data: String,

    /// Creation timestamp as Unix timestamp in seconds.
    pub created_at: i64,

    /// Optional conversation ID for grouping items.
    pub conversation_id: Option<String>,

    /// Optional sequence number within conversation.
    pub seq: Option<i64>,

    /// Tenant identifier for multi-tenancy isolation.
    pub tenant_id: Option<String>,
}

impl Item {
    /// Public identity, shared by every reference to the original item.
    #[must_use]
    pub fn public_id(&self) -> &str {
        self.reference_id.as_deref().unwrap_or(&self.id)
    }

    fn data_without_storage_marker(&self) -> Option<Value> {
        let mut value = deserialize_from_str_opt::<Value>(&self.data)?;
        if let Some(object) = value.as_object_mut() {
            object.remove(STORED_ITEM_KIND_KEY);
        }
        Some(value)
    }

    /// Deserialize data column as `InputItem`, projecting pre-typed reasoning rows.
    #[must_use]
    pub fn as_input(&self) -> Option<InputItem> {
        let data = self.data_without_storage_marker()?;
        InputItem::deserialize(&data)
            .ok()
            .or_else(|| self.legacy_reasoning(&data).map(InputItem::Reasoning))
    }

    /// Deserialize data column as `OutputItem`, projecting pre-typed reasoning rows.
    #[must_use]
    pub fn as_output(&self) -> Option<OutputItem> {
        let data = self.data_without_storage_marker()?;
        OutputItem::deserialize(&data)
            .ok()
            .or_else(|| self.legacy_reasoning(&data).map(OutputItem::Reasoning))
    }

    /// Deserialize data column as either `InputItem` or `OutputItem`.
    #[must_use]
    pub fn as_inout(&self) -> Option<InOutItem> {
        if let Some(kind) = self.stored_item_kind() {
            match kind {
                ItemKind::Input => {
                    if let Some(input) = self.as_input() {
                        return Some(InOutItem::Input(input));
                    }
                }
                ItemKind::Output => {
                    if let Some(output) = self.as_output() {
                        return Some(InOutItem::Output(output));
                    }
                }
            }
        }

        let output = self.as_output();
        if output.as_ref().is_some_and(|item| !matches!(item, OutputItem::Unknown)) {
            return output.map(InOutItem::Output);
        }

        let input = self.as_input();
        if input.as_ref().is_some_and(|item| !matches!(item, InputItem::Unknown)) {
            return input.map(InOutItem::Input);
        }

        match (input, output) {
            (Some(input), _) => Some(InOutItem::Input(input)),
            (_, Some(output)) => Some(InOutItem::Output(output)),
            _ => {
                warn!(item_id = %self.id, "unrecognized item type in stored data");
                None
            }
        }
    }

    fn stored_item_kind(&self) -> Option<ItemKind> {
        let value = deserialize_from_str_opt::<Value>(&self.data)?;
        ItemKind::from_stored_str(value.get(STORED_ITEM_KIND_KEY)?.as_str()?)
    }
}

fn item_values_clause(row_count: usize, first_bind_index: usize, sequence_from_cte: bool) -> String {
    let mut clause = String::new();
    let mut bind_index = first_bind_index;

    for row_index in 0..row_count {
        if row_index > 0 {
            clause.push_str(", ");
        }
        clause.push('(');
        for column_index in 0..(ITEM_COLUMN_COUNT + usize::from(sequence_from_cte)) {
            if column_index > 0 {
                clause.push_str(", ");
            }
            if sequence_from_cte && column_index == SEQUENCE_COLUMN_INDEX {
                write!(clause, "(SELECT start + ${bind_index} FROM next_seq)").expect("writing to String cannot fail");
            } else {
                write!(clause, "${bind_index}").expect("writing to String cannot fail");
            }
            bind_index += 1;
        }
        clause.push(')');
    }

    clause
}

/// Distinguishes client-supplied conversation items from persisted model history.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemSource {
    ConversationApi,
    ResponseHistory,
}

/// Serialize new history items, retaining public IDs already assigned to input or output items.
/// Items without IDs receive a storage-generated ID.
///
/// # Errors
/// Returns an error if serialization fails or a supplied ID is empty or has an invalid prefix.
pub(crate) fn serialize_new_items(items: Vec<InOutItem>, source: ItemSource) -> StoreResult<Vec<(String, String)>> {
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let (existing_id, prefix) = match &item {
                InOutItem::Input(input) => (input.id(), input.id_prefix()),
                InOutItem::Output(output) => (output.id(), output.id_prefix()),
            };
            // Upstream model IDs are preserved; some vLLM reasoning items use
            // msg_ IDs. Prefix errors apply to supplied Conversation Items IDs.
            if source == ItemSource::ConversationApi
                && let (Some(id), Some(prefix)) = (existing_id, prefix)
                && !id.starts_with(prefix)
            {
                return Err(StorageError::InvalidItemId {
                    param: format!("items[{index}].id"),
                    id: id.to_owned(),
                    prefix: prefix.trim_end_matches('_'),
                });
            }
            if existing_id == Some("") {
                return Err(StorageError::Validation("item id cannot be an empty string".to_owned()));
            }
            let id = existing_id.map_or_else(|| uuid7_str(prefix.unwrap_or("item_")), str::to_owned);
            let data = String::try_from(&item)?;
            Ok((id, data))
        })
        .collect()
}

/// Create items in a transaction with optional conversation context.
///
/// If `conversation_id` is provided, the next sequence range is computed in the insert statement so
/// concurrent `SQLite` writers do not take a stale read snapshot before writing.
///
/// # Errors
/// Returns an error if insertion fails or an item already belongs to the conversation.
pub async fn create_in_tx(
    tx: &mut DbTransaction<'_>,
    items: Vec<(String, String)>,
    conversation_id: Option<&str>,
) -> StoreResult<Vec<Item>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }

    let mut created: Vec<Item> = Vec::with_capacity(items.len());
    for batch in items.chunks(MAX_ITEMS_PER_INSERT) {
        let mut rows = if let Some(conversation_id) = conversation_id {
            // Later SQL batches check history from before this call, excluding
            // rows inserted by earlier batches of the same request.
            create_in_tx_with_next_conversation_seq(
                tx,
                batch,
                conversation_id,
                created.iter().filter_map(|item| item.seq).min(),
            )
            .await?
        } else {
            create_in_tx_without_conversation(tx, batch).await?
        };
        if conversation_id.is_some() {
            rows.sort_by_key(|row| row.seq);
        }
        created.append(&mut rows);
    }
    Ok(created)
}

async fn create_in_tx_without_conversation(
    tx: &mut DbTransaction<'_>,
    items: &[(String, String)],
) -> DbResult<Vec<Item>> {
    let now = utcnow_str();
    let values_clause = item_values_clause(items.len(), 1, false);
    let sql = format!(
        "WITH incoming (id, data, created_at, conversation_id, seq) AS (VALUES {values_clause}) \
                 INSERT INTO items (id, data, created_at, conversation_id, seq, tenant_id) \
                 SELECT incoming.*, 'default_tenant' FROM incoming RETURNING *"
    );

    let mut query = sqlx::query_as::<_, Item>(&sql);
    for (id, data) in items {
        query = query.bind(id).bind(data).bind(now).bind(None::<&str>).bind(None::<i64>);
    }

    query.fetch_all(&mut **tx).await
}

async fn create_in_tx_with_next_conversation_seq(
    tx: &mut DbTransaction<'_>,
    items: &[(String, String)],
    conversation_id: &str,
    first_inserted_sequence: Option<i64>,
) -> StoreResult<Vec<Item>> {
    let now = utcnow_str();
    let values_clause = item_values_clause(items.len(), 3, true);
    let sql = format!(
        "WITH next_seq AS ( \
             SELECT COALESCE(MAX(seq), -1) + 1 AS start FROM items WHERE conversation_id = $1 \
         ), incoming (id, data, created_at, conversation_id, seq, entry_id) AS ( \
             VALUES {values_clause} \
         ), owner AS ( \
             SELECT COALESCE(tenant_id, 'default_tenant') AS tenant_id FROM conversations WHERE id = $1 \
         ) \
         INSERT INTO items (id, data, created_at, conversation_id, seq, reference_id, tenant_id) \
         SELECT CASE WHEN source.id IS NULL THEN incoming.id ELSE incoming.entry_id END, \
                COALESCE(source.data, incoming.data), COALESCE(source.created_at, incoming.created_at), \
                incoming.conversation_id, incoming.seq, source.id, owner.tenant_id \
         FROM incoming CROSS JOIN owner \
         LEFT JOIN items source ON source.id = incoming.id AND source.tenant_id = owner.tenant_id \
             AND (CAST($2 AS BIGINT) IS NULL OR source.conversation_id IS NULL \
                  OR source.conversation_id <> $1 OR source.seq < $2) \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM items existing JOIN incoming ON incoming.id = COALESCE(existing.reference_id, existing.id) \
             WHERE existing.conversation_id = $1 \
               AND (CAST($2 AS BIGINT) IS NULL OR existing.seq < $2) \
         ) \
         ORDER BY incoming.seq \
         RETURNING *"
    );

    let mut query = sqlx::query_as::<_, Item>(&sql)
        .bind(conversation_id)
        .bind(first_inserted_sequence);
    #[allow(clippy::cast_possible_wrap)]
    for (idx, (id, data)) in items.iter().enumerate() {
        query = query
            .bind(id)
            .bind(data)
            .bind(now)
            .bind(conversation_id)
            .bind(idx as i64)
            .bind(uuid7_str("item_"));
    }

    let rows = query.fetch_all(&mut **tx).await?;
    if rows.is_empty() {
        return Err(StorageError::ItemAlreadyInConversation);
    }
    Ok(rows)
}

/// Get items by IDs.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_items(pool: &DbPool, ids: &[String]) -> DbResult<Vec<Item>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let mut rows = Vec::with_capacity(ids.len());
    for batch in ids.chunks(MAX_BIND_PARAMETERS) {
        let placeholders = (1..=batch.len())
            .map(|index| format!("${index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT * FROM items WHERE id IN ({placeholders})");
        let mut query = sqlx::query_as::<_, Item>(&sql);
        for id in batch {
            query = query.bind(id);
        }
        rows.extend(query.fetch_all(pool).await?);
    }
    Ok(rows)
}

/// Get items by conversation ID ordered by sequence.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_items_by_conversation(pool: &DbPool, conversation_id: &str) -> DbResult<Vec<Item>> {
    sqlx::query_as::<_, Item>("SELECT * FROM items WHERE conversation_id = $1 ORDER BY seq ASC")
        .bind(conversation_id)
        .fetch_all(pool)
        .await
}

/// Collect active item IDs in conversation order while holding the turn's transaction.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn ids_for_conversation_in_tx(tx: &mut DbTransaction<'_>, conversation_id: &str) -> DbResult<Vec<String>> {
    sqlx::query_scalar("SELECT id FROM items WHERE conversation_id = $1 ORDER BY seq ASC")
        .bind(conversation_id)
        .fetch_all(&mut **tx)
        .await
}

/// Returns the last stored item sequence for a conversation inside a transaction.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn last_conversation_sequence_in_tx(
    tx: &mut DbTransaction<'_>,
    conversation_id: &str,
) -> DbResult<Option<i64>> {
    sqlx::query_scalar("SELECT MAX(seq) FROM items WHERE conversation_id = $1")
        .bind(conversation_id)
        .fetch_one(&mut **tx)
        .await
}

/// List a page using keyset pagination with (seq, id) ordering.
///
/// # Errors
/// Returns an error if the query fails or the cursor is not in this conversation.
pub async fn list_for_conversation(
    pool: &DbPool,
    tenant_id: &str,
    conversation_id: &str,
    limit: i64,
    after_id: Option<&str>,
    order: ItemOrder,
) -> DbResult<Vec<Item>> {
    let (comparison, direction) = match order {
        ItemOrder::Asc => (">", "ASC"),
        ItemOrder::Desc => ("<", "DESC"),
    };
    let base = "SELECT items.* FROM items \
                JOIN conversations ON conversations.id = items.conversation_id \
                WHERE conversations.id = $1 AND conversations.tenant_id = $2";
    let ordering = format!("ORDER BY COALESCE(items.seq, -1) {direction}, items.id {direction}");

    if let Some(cursor_id) = after_id {
        // An ID can name several occurrences. Advance beyond the furthest
        // occurrence in this ordering so the public cursor cannot loop forever.
        let cursor_direction = if order == ItemOrder::Asc { "DESC" } else { "ASC" };
        let cursor_query = format!(
            "{base} AND COALESCE(items.reference_id, items.id) = $3 \
             ORDER BY items.seq {cursor_direction}, items.id {cursor_direction} LIMIT 1"
        );
        let cursor = sqlx::query_as::<_, Item>(&cursor_query)
            .bind(conversation_id)
            .bind(tenant_id)
            .bind(cursor_id)
            .fetch_optional(pool)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        let query = format!("{base} AND (COALESCE(items.seq, -1), items.id) {comparison} ($3, $4) {ordering} LIMIT $5");
        sqlx::query_as(&query)
            .bind(conversation_id)
            .bind(tenant_id)
            .bind(cursor.seq.unwrap_or(-1))
            .bind(cursor.id)
            .bind(limit)
            .fetch_all(pool)
            .await
    } else {
        let query = format!("{base} {ordering} LIMIT $3");
        sqlx::query_as(&query)
            .bind(conversation_id)
            .bind(tenant_id)
            .bind(limit)
            .fetch_all(pool)
            .await
    }
}

/// Get an item through its conversation, including items written by Responses persistence.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get_for_conversation(
    pool: &DbPool,
    tenant_id: &str,
    conversation_id: &str,
    item_id: &str,
) -> DbResult<Option<Item>> {
    sqlx::query_as(
        "SELECT items.* FROM items JOIN conversations ON conversations.id = items.conversation_id \
         WHERE conversations.id = $1 AND conversations.tenant_id = $2 AND COALESCE(items.reference_id, items.id) = $3 \
         ORDER BY items.seq ASC LIMIT 1",
    )
    .bind(conversation_id)
    .bind(tenant_id)
    .bind(item_id)
    .fetch_optional(pool)
    .await
}

/// Remove one or all items from a locked conversation while preserving stored response history.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn detach_from_conversation_in_tx(
    tx: &mut DbTransaction<'_>,
    conversation_id: &str,
    item_id: Option<&str>,
) -> DbResult<u64> {
    let result = sqlx::query(
        "UPDATE items SET conversation_id = NULL, seq = NULL \
         WHERE conversation_id = $1 AND (CAST($2 AS TEXT) IS NULL OR COALESCE(reference_id, id) = $2)",
    )
    .bind(conversation_id)
    .bind(item_id)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::event::MessageStatus;
    use crate::types::io::{InputItem, OutputItem, OutputMessage, ReasoningOutput, ReasoningTextContent};

    #[test]
    fn new_items_reuse_public_ids_and_generate_storage_ids() {
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
        let rows = serialize_new_items(
            vec![
                InOutItem::Input(input),
                InOutItem::Output(output),
                InOutItem::Input(without_id),
                InOutItem::Input(function_call),
            ],
            ItemSource::ConversationApi,
        )
        .unwrap();
        assert_eq!(rows[0].0, "msg_supplied");
        assert_eq!(rows[1].0, "msg_generated");
        assert!(rows[2].0.starts_with("msg_"));
        assert_eq!(rows[3].0, "fc_supplied");
    }

    #[test]
    fn serialize_new_items_rejects_empty_id() {
        let input: InputItem = serde_json::from_value(serde_json::json!({
            "type": "message", "id": "", "role": "user", "content": "test"
        }))
        .unwrap();
        let result = serialize_new_items(vec![InOutItem::Input(input)], ItemSource::ConversationApi);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_validation());
        assert!(matches!(err, StorageError::InvalidItemId { param, .. } if param == "items[0].id"));
    }

    #[test]
    fn serialize_new_items_accepts_missing_id() {
        let input: InputItem = serde_json::from_value(serde_json::json!({
            "type": "message", "role": "user", "content": "test"
        }))
        .unwrap();
        let result = serialize_new_items(vec![InOutItem::Input(input)], ItemSource::ConversationApi);
        assert!(result.is_ok());
        let rows = result.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].0.starts_with("msg_"));
    }

    #[test]
    fn item_values_clause_numbers_plain_rows() {
        assert_eq!(
            item_values_clause(2, 1, false),
            "($1, $2, $3, $4, $5), ($6, $7, $8, $9, $10)"
        );
    }

    #[test]
    fn item_values_clause_numbers_conversation_rows_after_cte_bind() {
        assert_eq!(
            item_values_clause(2, 2, true),
            "($2, $3, $4, $5, (SELECT start + $6 FROM next_seq), $7), \
             ($8, $9, $10, $11, (SELECT start + $12 FROM next_seq), $13)"
        );
    }

    #[tokio::test]
    async fn item_queries_chunk_above_portable_bind_limit() {
        let pool = crate::storage::create_pool_with_schema(Some("sqlite://?mode=memory"))
            .await
            .expect("create in-memory database");
        let items = (0..=MAX_BIND_PARAMETERS)
            .map(|index| (format!("item_{index}"), "{}".to_owned()))
            .collect::<Vec<_>>();
        let ids = items.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
        let mut transaction = pool.begin().await.expect("begin transaction");
        let created = create_in_tx(&mut transaction, items, None)
            .await
            .expect("insert item batches");
        transaction.commit().await.expect("commit item batches");
        let loaded = get_items(&pool, &ids).await.expect("load item batches");

        assert_eq!(created.len(), MAX_BIND_PARAMETERS + 1);
        assert_eq!(loaded.len(), MAX_BIND_PARAMETERS + 1);
    }

    #[tokio::test]
    async fn conversation_item_batches_keep_contiguous_sequences() {
        let pool = crate::storage::create_pool_with_schema(Some("sqlite://?mode=memory"))
            .await
            .expect("create in-memory database");
        let conversation_id = "conv_batch";
        crate::storage::models::conversation::create(&pool, conversation_id)
            .await
            .expect("create conversation");
        let item_count = MAX_ITEMS_PER_INSERT + 1;
        let items = (0..item_count)
            .map(|index| (format!("conversation_item_{index}"), "{}".to_owned()))
            .collect::<Vec<_>>();
        let mut transaction = pool.begin().await.expect("begin transaction");
        let created = create_in_tx(&mut transaction, items, Some(conversation_id))
            .await
            .expect("insert conversation item batches");
        transaction.commit().await.expect("commit item batches");
        let stored = get_items_by_conversation(&pool, conversation_id)
            .await
            .expect("load conversation item batches");
        let expected_sequences = (0..i64::try_from(item_count).expect("item count fits in i64")).collect::<Vec<_>>();

        assert_eq!(
            created
                .iter()
                .map(|item| item.seq.expect("created sequence"))
                .collect::<Vec<_>>(),
            expected_sequences
        );
        assert_eq!(
            stored
                .iter()
                .map(|item| item.seq.expect("stored sequence"))
                .collect::<Vec<_>>(),
            expected_sequences
        );
    }

    #[test]
    fn test_item_basic() {
        let item = Item {
            id: "item_123".to_string(),
            data: r#"{"role":"user","content":"hello"}"#.to_string(),
            created_at: 1_704_067_200,
            conversation_id: Some("conv_456".to_string()),
            seq: Some(1),
            tenant_id: None,
            reference_id: None,
        };

        assert_eq!(item.id, "item_123");
        assert_eq!(item.conversation_id, Some("conv_456".to_string()));
        assert_eq!(item.seq, Some(1));
    }

    #[test]
    fn test_item_optional_fields() {
        let item = Item {
            id: "item_789".to_string(),
            data: r#"{"role":"assistant"}"#.to_string(),
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
            tenant_id: None,
            reference_id: None,
        };

        assert!(item.conversation_id.is_none());
        assert!(item.seq.is_none());
    }

    #[test]
    fn complete_reasoning_round_trip_strips_storage_marker() {
        let mut reasoning = ReasoningOutput::new("rs_1");
        reasoning.content.extend([
            ReasoningTextContent::new("first thought"),
            ReasoningTextContent::new("second thought"),
        ]);
        reasoning
            .summary
            .push(crate::types::ReasoningSummaryContent::new("concise summary"));
        reasoning.encrypted_content = Some(crate::types::OpaqueReasoning::try_from("opaque".to_owned()).unwrap());
        reasoning.status = Some(crate::types::ReasoningStatus::Completed);
        let stored = InOutItem::Output(OutputItem::Reasoning(reasoning));
        let stored_json = String::try_from(&stored).expect("serialization failed");
        assert!(stored_json.contains(STORED_ITEM_KIND_KEY));
        let item = Item {
            id: "item_reasoning".to_string(),
            data: stored_json,
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
            tenant_id: None,
            reference_id: None,
        };

        let Some(InOutItem::Output(OutputItem::Reasoning(reasoning))) = item.as_inout() else {
            panic!("expected stored reasoning output");
        };
        assert_eq!(reasoning.id, "rs_1");
        assert_eq!(reasoning.content.len(), 2);
        assert_eq!(reasoning.summary[0].text, "concise summary");
        assert_eq!(
            reasoning
                .encrypted_content
                .as_ref()
                .map(crate::types::OpaqueReasoning::as_str),
            Some("opaque")
        );
        assert_eq!(reasoning.status, Some(crate::types::ReasoningStatus::Completed));

        let reconstructed = serde_json::to_value(OutputItem::Reasoning(reasoning)).expect("reasoning value");
        assert!(reconstructed.get(STORED_ITEM_KIND_KEY).is_none());
    }

    #[test]
    fn test_legacy_output_message_rehydrates_as_output_before_unknown_input() {
        let item = Item {
            id: "item_message".to_string(),
            data: serde_json::json!({
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "hello", "annotations": []}]
            })
            .to_string(),
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
            tenant_id: None,
            reference_id: None,
        };

        let stored = item.as_inout().expect("stored item");
        assert!(matches!(stored, InOutItem::Output(OutputItem::Message(_))));

        let inputs = InOutItem::into_input_items(vec![stored]);
        assert!(matches!(inputs[0], InputItem::Message(_)));
    }

    #[test]
    fn test_namespaced_function_call_rehydrates_without_storage_marker() {
        let stored = InOutItem::Output(OutputItem::FunctionCall(crate::types::io::FunctionToolCall {
            id: "fc_1".to_string(),
            call_id: "call_1".to_string(),
            name: "run".to_string(),
            namespace: Some("mcp__shell".to_string()),
            arguments: "{\"cmd\":\"pwd\"}".to_string(),
            status: MessageStatus::Completed,
        }));
        let item = Item {
            id: "item_function_call".to_string(),
            data: String::try_from(&stored).expect("serialization failed"),
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
            tenant_id: None,
            reference_id: None,
        };

        let inputs = InOutItem::into_input_items(vec![item.as_inout().expect("stored item")]);
        let value = serde_json::to_value(&inputs[0]).expect("input value");

        assert_eq!(value["type"], "function_call");
        assert_eq!(value["namespace"], "mcp__shell");
        assert_eq!(value["name"], "run");
        assert!(value.get(STORED_ITEM_KIND_KEY).is_none());

        println!("namespace round-trip: mcp__shell.run -> storage -> input function_call");
        println!("storage marker stripped: _agentic_item_kind absent");
    }

    #[test]
    fn shell_call_round_trips_through_storage_and_rehydration() {
        let output: OutputItem = serde_json::from_value(serde_json::json!({
            "type": "shell_call",
            "id": "sh_1",
            "call_id": "call_shell",
            "action": {
                "commands": ["pwd"],
                "timeout_ms": 1_000,
                "max_output_length": 4_096
            },
            "status": "completed"
        }))
        .expect("shell output item");
        let stored = InOutItem::Output(output);
        let item = Item {
            id: "item_shell_call".to_owned(),
            data: String::try_from(&stored).expect("serialization failed"),
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
            tenant_id: None,
            reference_id: None,
        };

        let inputs = InOutItem::into_input_items(vec![item.as_inout().expect("stored shell item")]);
        let value = serde_json::to_value(&inputs[0]).expect("rehydrated shell input");

        assert_eq!(value["type"], "function_call");
        assert_eq!(value["name"], "shell");
        assert_eq!(value["call_id"], "call_shell");
        let action: serde_json::Value = serde_json::from_str(value["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(action["commands"][0], "pwd");
        assert!(value.get(STORED_ITEM_KIND_KEY).is_none());
    }

    #[test]
    fn test_multiple_namespaced_function_calls_rehydrate_without_storage_marker() {
        let stored_items = [
            InOutItem::Output(OutputItem::FunctionCall(crate::types::io::FunctionToolCall {
                id: "fc_1".to_string(),
                call_id: "call_1".to_string(),
                name: "run".to_string(),
                namespace: Some("mcp__shell".to_string()),
                arguments: "{\"cmd\":\"pwd\"}".to_string(),
                status: MessageStatus::Completed,
            })),
            InOutItem::Output(OutputItem::FunctionCall(crate::types::io::FunctionToolCall {
                id: "fc_2".to_string(),
                call_id: "call_2".to_string(),
                name: "run".to_string(),
                namespace: Some("mcp__git".to_string()),
                arguments: "{\"args\":[\"status\",\"--short\"]}".to_string(),
                status: MessageStatus::Completed,
            })),
        ];
        let rows: Vec<InOutItem> = stored_items
            .iter()
            .enumerate()
            .map(|(idx, stored)| Item {
                id: format!("item_function_call_{idx}"),
                data: String::try_from(stored).expect("serialization failed"),
                created_at: 1_704_067_200,
                conversation_id: None,
                seq: Some(idx.try_into().expect("seq")),
                tenant_id: None,
                reference_id: None,
            })
            .map(|item| item.as_inout().expect("stored item"))
            .collect();

        let inputs = InOutItem::into_input_items(rows);
        let values = serde_json::to_value(&inputs).expect("input values");

        assert_eq!(values[0]["type"], "function_call");
        assert_eq!(values[0]["namespace"], "mcp__shell");
        assert_eq!(values[0]["name"], "run");
        assert_eq!(values[0]["call_id"], "call_1");
        assert!(values[0].get(STORED_ITEM_KIND_KEY).is_none());

        assert_eq!(values[1]["type"], "function_call");
        assert_eq!(values[1]["namespace"], "mcp__git");
        assert_eq!(values[1]["name"], "run");
        assert_eq!(values[1]["call_id"], "call_2");
        assert!(values[1].get(STORED_ITEM_KIND_KEY).is_none());

        println!("namespace round-trip: mcp__shell.run -> call_1");
        println!("namespace round-trip: mcp__git.run -> call_2");
        println!("same tool name preserved under separate namespaces");
    }

    #[test]
    fn test_unknown_rehydrated_items_are_omitted() {
        let stored = InOutItem::Output(OutputItem::Unknown);
        let item = Item {
            id: "item_unknown".to_string(),
            data: String::try_from(&stored).expect("serialization failed"),
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
            tenant_id: None,
            reference_id: None,
        };

        let inputs = InOutItem::into_input_items(vec![item.as_inout().expect("stored item")]);

        assert!(inputs.is_empty());
    }
}

#[cfg(test)]
mod item_id_tests {
    use super::*;
    use crate::types::conversations::ConversationItem;
    use serde_json::json;

    fn stored(value: serde_json::Value) -> InOutItem {
        match serde_json::from_value::<ConversationItem>(value).unwrap() {
            ConversationItem::Input(item) => InOutItem::Input(item),
            ConversationItem::Output(item) => InOutItem::Output(item),
        }
    }

    #[test]
    fn supplied_ids_are_validated_by_item_type_without_changing_call_ids() {
        let cases = [
            json!({"type":"message", "id":"msg_1", "role":"user", "content":"hello"}),
            json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"f", "arguments":"{}"}),
            json!({"type":"tool_search_call", "id":"tsc_1", "call_id":"call_1", "arguments":{}}),
            json!({"type":"custom_tool_call", "id":"ctc_1", "call_id":"call_1", "name":"f", "input":"x"}),
            json!({"type":"shell_call", "id":"sh_1", "call_id":"call_1", "action":{"commands":["pwd"]}}),
            json!({"type":"reasoning", "id":"rs_1", "summary":[]}),
            json!({"type":"mcp_list_tools", "id":"mcpl_1", "server_label":"s", "tools":[]}),
            json!({"type":"mcp_call", "id":"mcp_1", "server_label":"s", "name":"f", "arguments":"{}"}),
            json!({"type":"web_search_call", "id":"ws_1", "status":"completed", "action":{"type":"search", "query":"test"}}),
            json!({"type":"compaction", "id":"cmp_1", "encrypted_content":"opaque"}),
        ];
        for mut value in cases {
            let item = stored(value.clone());
            serialize_new_items(vec![item], ItemSource::ConversationApi).expect("matching public ID prefix");
            value["id"] = json!("wrong_1");
            let item = stored(value);
            let error = serialize_new_items(vec![item], ItemSource::ConversationApi).unwrap_err();
            assert!(error.is_validation());
            assert!(matches!(error, StorageError::InvalidItemId { param, .. } if param == "items[0].id"));
        }
    }

    #[test]
    fn optional_ids_and_unverified_prefixes_are_not_restricted() {
        for value in [
            json!({"type":"message", "role":"user", "content":"hello"}),
            json!({"type":"function_call", "call_id":"call_1", "name":"f", "arguments":"{}"}),
            json!({"type":"function_call_output", "call_id":"call_1", "output":"done"}),
            json!({"type":"shell_call_output", "id":"output_1", "call_id":"call_1", "output":[]}),
        ] {
            serialize_new_items(vec![stored(value)], ItemSource::ConversationApi).unwrap();
        }
    }

    #[test]
    fn invalid_id_reports_its_batch_index_and_rejects_empty_ids() {
        for id in ["", "item_wrongprefix"] {
            let items = vec![
                stored(json!({"type":"message", "role":"user", "content":"valid"})),
                stored(json!({"type":"message", "id":id, "role":"user", "content":"invalid"})),
            ];
            let error = serialize_new_items(items, ItemSource::ConversationApi).unwrap_err();
            assert!(matches!(&error, StorageError::InvalidItemId { param, .. } if param == "items[1].id"));
            assert_eq!(
                error.to_string(),
                format!("Invalid 'items[1].id': '{id}'. Expected an ID that begins with 'msg'.")
            );
        }
    }
}
