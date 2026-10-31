//! Conversation history item stored in the database.

use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write;
use tracing::warn;

mod insert;
pub(crate) use insert::InsertItem;

use super::super::pool::{DbPool, DbResult, DbTransaction};
use super::super::types::item::{ItemKind, STORED_CODE_INTERPRETER_ORIGIN_KEY, STORED_ITEM_KIND_KEY};
use crate::storage::{StorageError, StoreResult};
use crate::types::io::InOutItem;
use crate::types::io::ItemOrder;
use crate::types::io::code_interpreter::CodeInterpreterCallOrigin;
use crate::types::io::{InputItem, OutputItem, ReasoningOutput};
use crate::utils::common::{deserialize_from_str_opt, utcnow_str, uuid7_str};

const ITEM_INSERT_COLUMNS: usize = 8;
const ENTRY_INSERT_COLUMNS: usize = 5;
const MAX_BIND_PARAMETERS: usize = 999;
const MAX_ITEMS_PER_INSERT: usize = MAX_BIND_PARAMETERS / ITEM_INSERT_COLUMNS;

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

    /// Public identity of an independent history row, which keeps its own copy of the item data.
    pub public_id: Option<String>,

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
        self.public_id
            .as_deref()
            .or(self.reference_id.as_deref())
            .unwrap_or(&self.id)
    }

    fn data_without_storage_marker(&self) -> Option<(Value, bool)> {
        let mut value = deserialize_from_str_opt::<Value>(&self.data)?;
        let gateway_origin = value.get(STORED_CODE_INTERPRETER_ORIGIN_KEY).and_then(Value::as_str) == Some("gateway");
        if let Some(object) = value.as_object_mut() {
            object.remove(STORED_ITEM_KIND_KEY);
            object.remove(STORED_CODE_INTERPRETER_ORIGIN_KEY);
        }
        Some((value, gateway_origin))
    }

    /// Deserialize data column as `InputItem`, projecting pre-typed reasoning rows.
    #[must_use]
    pub fn as_input(&self) -> Option<InputItem> {
        let (value, _) = self.data_without_storage_marker()?;
        serde_json::from_value(value)
            .ok()
            .or_else(|| self.legacy_reasoning().map(InputItem::Reasoning))
    }

    /// Deserialize data column as `OutputItem`, projecting pre-typed reasoning rows.
    #[must_use]
    pub fn as_output(&self) -> Option<OutputItem> {
        let (value, gateway_origin) = self.data_without_storage_marker()?;
        let mut output = serde_json::from_value(value)
            .ok()
            .or_else(|| self.legacy_reasoning().map(OutputItem::Reasoning))?;
        if gateway_origin && let OutputItem::CodeInterpreterCall(call) = &mut output {
            call.origin = CodeInterpreterCallOrigin::Gateway;
        }
        Some(output)
    }

    /// Project a reasoning row written before typed reasoning. Typed decoding already
    /// failed, so this rare path parses the row again instead of copying every row up front.
    fn legacy_reasoning(&self) -> Option<ReasoningOutput> {
        let (value, _) = self.data_without_storage_marker()?;
        ReasoningOutput::from_legacy_value(&value)
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

        self.as_input()
            .map(InOutItem::Input)
            .or_else(|| output.map(InOutItem::Output))
            .or_else(|| {
                warn!(item_id = %self.id, "unrecognized item type in stored data");
                None
            })
    }

    fn stored_item_kind(&self) -> Option<ItemKind> {
        let value = deserialize_from_str_opt::<Value>(&self.data)?;
        ItemKind::from_stored_str(value.get(STORED_ITEM_KIND_KEY)?.as_str()?)
    }
}

/// Item columns for rows read directly from `items`.
pub(crate) const ITEM_COLUMNS: &str = "items.id, items.reference_id, items.public_id, items.data, items.created_at, \
     items.conversation_id, items.seq, items.tenant_id";

/// Item columns for rows read through a conversation entry, which supplies list position and identity.
const ENTRY_ITEM_COLUMNS: &str = "items.id, items.reference_id, entries.public_id AS public_id, items.data, \
     items.created_at, entries.conversation_id AS conversation_id, entries.seq AS seq, items.tenant_id";

fn numbered_rows(row_count: usize, columns: usize, first_bind_index: usize) -> String {
    let mut clause = String::new();
    for row in 0..row_count {
        if row > 0 {
            clause.push_str(", ");
        }
        clause.push('(');
        for column in 0..columns {
            if column > 0 {
                clause.push_str(", ");
            }
            write!(clause, "${}", first_bind_index + row * columns + column).expect("writing to String cannot fail");
        }
        clause.push(')');
    }
    clause
}

/// Store a response's own items in order. Each occurrence gets a fresh row ID; the supplied ID
/// stays its public identity, so a repeated public ID is stored once per occurrence.
///
/// # Errors
/// Returns an error if insertion fails.
pub(crate) async fn create_for_response_in_tx(
    tx: &mut DbTransaction<'_>,
    tenant_id: &str,
    response_id: &str,
    items: Vec<InsertItem>,
) -> DbResult<Vec<Item>> {
    insert_rows(tx, tenant_id, Some(response_id), items).await
}

async fn insert_rows(
    tx: &mut DbTransaction<'_>,
    tenant_id: &str,
    response_id: Option<&str>,
    items: Vec<InsertItem>,
) -> DbResult<Vec<Item>> {
    let now = utcnow_str();
    let rows = items
        .into_iter()
        .enumerate()
        .map(|(position, item)| (i64::try_from(position).unwrap_or(i64::MAX), uuid7_str("item_"), item))
        .collect::<Vec<_>>();
    for batch in rows.chunks(MAX_ITEMS_PER_INSERT) {
        let sql = format!(
            "INSERT INTO items (id, public_id, data, reasoning_provenance, position, created_at, tenant_id, response_id) \
             VALUES {}",
            numbered_rows(batch.len(), ITEM_INSERT_COLUMNS, 1)
        );
        let mut query = sqlx::query(&sql);
        for (position, row_id, item) in batch {
            query = query
                .bind(row_id)
                .bind(&item.id)
                .bind(&item.data)
                .bind(&item.reasoning_provenance)
                .bind(response_id.map(|_| *position))
                .bind(now)
                .bind(tenant_id)
                .bind(response_id);
        }
        query.execute(&mut **tx).await?;
    }
    Ok(rows
        .into_iter()
        .map(|(_, id, item)| Item {
            id,
            reference_id: None,
            public_id: Some(item.id),
            data: item.data,
            created_at: now,
            conversation_id: None,
            seq: None,
            tenant_id: Some(tenant_id.to_owned()),
        })
        .collect())
}

/// Append items to a locked conversation as entries added at `revision`.
///
/// An ID already stored for the tenant references that item instead of copying it, so the
/// submitted content is ignored; other items are written as new rows owned by
/// `owner_response_id` (a conversation turn) or by nobody (the items API). Rows are returned in
/// list order.
///
/// # Errors
/// Returns [`StorageError::ItemAlreadyInConversation`] if an ID is already in the conversation.
pub(crate) async fn attach_to_conversation_in_tx(
    tx: &mut DbTransaction<'_>,
    conversation_id: &str,
    tenant_id: &str,
    revision: i64,
    items: Vec<InsertItem>,
    owner_response_id: Option<&str>,
) -> StoreResult<Vec<Item>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let public_ids = items.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
    let mut sources: HashMap<String, Item> = HashMap::new();
    for batch in public_ids.chunks(MAX_BIND_PARAMETERS - 1) {
        let placeholders = (2..batch.len() + 2)
            .map(|index| format!("${index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT 1 FROM conversation_entries \
             WHERE conversation_id = $1 AND removed_revision IS NULL AND public_id IN ({placeholders}) LIMIT 1"
        );
        let mut query = sqlx::query(&sql).bind(conversation_id);
        for id in batch {
            query = query.bind(id);
        }
        if query.fetch_optional(&mut **tx).await?.is_some() {
            return Err(StorageError::ItemAlreadyInConversation);
        }
        // A repeated public ID resolves to the row whose own ID matches it, else the earliest one.
        let sql = format!(
            "SELECT {ITEM_COLUMNS} FROM items \
             WHERE items.tenant_id = $1 AND COALESCE(items.public_id, items.reference_id, items.id) IN ({placeholders}) \
             ORDER BY items.id"
        );
        let mut query = sqlx::query_as::<_, Item>(&sql).bind(tenant_id);
        for id in batch {
            query = query.bind(id);
        }
        for row in query.fetch_all(&mut **tx).await? {
            let public_id = row.public_id().to_owned();
            let preferred = sources
                .get(&public_id)
                .is_none_or(|found| found.id != public_id && row.id == public_id);
            if preferred {
                sources.insert(public_id, row);
            }
        }
    }
    let fresh = insert_rows(
        tx,
        tenant_id,
        owner_response_id,
        items
            .into_iter()
            .filter(|item| !sources.contains_key(&item.id))
            .collect(),
    )
    .await?;
    let mut fresh = fresh
        .into_iter()
        .map(|item| (item.public_id().to_owned(), item))
        .collect::<HashMap<_, _>>();
    let first_seq: i64 = sqlx::query_scalar("SELECT next_seq FROM conversations WHERE id = $1")
        .bind(conversation_id)
        .fetch_one(&mut **tx)
        .await?;
    let mut entries = Vec::with_capacity(public_ids.len());
    for (offset, public_id) in public_ids.into_iter().enumerate() {
        let mut item = match sources.get(&public_id) {
            Some(source) => source.clone(),
            None => fresh.remove(&public_id).ok_or_else(|| {
                StorageError::Validation(format!("item ID '{public_id}' appears more than once in one request"))
            })?,
        };
        item.public_id = Some(public_id);
        item.conversation_id = Some(conversation_id.to_owned());
        item.seq = Some(first_seq + i64::try_from(offset).unwrap_or(i64::MAX));
        entries.push(item);
    }
    for batch in entries.chunks(MAX_BIND_PARAMETERS / ENTRY_INSERT_COLUMNS) {
        let sql = format!(
            "INSERT INTO conversation_entries (conversation_id, seq, item_id, public_id, added_revision) VALUES {}",
            numbered_rows(batch.len(), ENTRY_INSERT_COLUMNS, 1)
        );
        let mut query = sqlx::query(&sql);
        for item in batch {
            query = query
                .bind(conversation_id)
                .bind(item.seq)
                .bind(&item.id)
                .bind(item.public_id())
                .bind(revision);
        }
        query.execute(&mut **tx).await?;
    }
    let count = i64::try_from(entries.len()).unwrap_or(i64::MAX);
    sqlx::query("UPDATE conversations SET next_seq = next_seq + $1 WHERE id = $2")
        .bind(count)
        .bind(conversation_id)
        .execute(&mut **tx)
        .await?;
    Ok(entries)
}

/// Get items by row IDs (legacy `history_item_ids` lists).
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
        let sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE id IN ({placeholders})");
        let mut query = sqlx::query_as::<_, Item>(&sql);
        for id in batch {
            query = query.bind(id);
        }
        rows.extend(query.fetch_all(pool).await?);
    }
    Ok(rows)
}

/// Find the original stored item carrying a public identity.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn get_by_public_id(pool: &DbPool, id: &str) -> DbResult<Option<Item>> {
    sqlx::query_as::<_, Item>(&format!(
        "SELECT {ITEM_COLUMNS} FROM items WHERE COALESCE(public_id, reference_id, id) = $1 \
         ORDER BY created_at, id LIMIT 1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Get a conversation's current items in list order.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_items_by_conversation(pool: &DbPool, conversation_id: &str) -> DbResult<Vec<Item>> {
    sqlx::query_as::<_, Item>(&format!(
        "SELECT {ENTRY_ITEM_COLUMNS} FROM conversation_entries entries JOIN items ON items.id = entries.item_id \
         WHERE entries.conversation_id = $1 AND entries.removed_revision IS NULL ORDER BY entries.seq ASC"
    ))
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// Returns the last current item sequence for a conversation inside a transaction.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn last_conversation_sequence_in_tx(
    tx: &mut DbTransaction<'_>,
    conversation_id: &str,
) -> DbResult<Option<i64>> {
    sqlx::query_scalar(
        "SELECT MAX(seq) FROM conversation_entries WHERE conversation_id = $1 AND removed_revision IS NULL",
    )
    .bind(conversation_id)
    .fetch_one(&mut **tx)
    .await
}

/// List a page using keyset pagination over the conversation's list order.
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
    let base = format!(
        "SELECT {ENTRY_ITEM_COLUMNS} FROM conversation_entries entries \
         JOIN conversations ON conversations.id = entries.conversation_id \
         JOIN items ON items.id = entries.item_id \
         WHERE conversations.id = $1 AND conversations.tenant_id = $2 AND conversations.deleted_at IS NULL \
           AND entries.removed_revision IS NULL"
    );

    if let Some(cursor_id) = after_id {
        // An ID can name several occurrences. Advance beyond the furthest
        // occurrence in this ordering so the public cursor cannot loop forever.
        let furthest = if order == ItemOrder::Asc { "MAX" } else { "MIN" };
        let cursor: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT {furthest}(entries.seq) FROM conversation_entries entries \
             JOIN conversations ON conversations.id = entries.conversation_id \
             WHERE conversations.id = $1 AND conversations.tenant_id = $2 AND conversations.deleted_at IS NULL \
               AND entries.removed_revision IS NULL AND entries.public_id = $3"
        ))
        .bind(conversation_id)
        .bind(tenant_id)
        .bind(cursor_id)
        .fetch_one(pool)
        .await?;
        let cursor = cursor.ok_or(sqlx::Error::RowNotFound)?;
        sqlx::query_as(&format!(
            "{base} AND entries.seq {comparison} $3 ORDER BY entries.seq {direction} LIMIT $4"
        ))
        .bind(conversation_id)
        .bind(tenant_id)
        .bind(cursor)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as(&format!("{base} ORDER BY entries.seq {direction} LIMIT $3"))
            .bind(conversation_id)
            .bind(tenant_id)
            .bind(limit)
            .fetch_all(pool)
            .await
    }
}

/// Get the first current occurrence of an item in a conversation.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn get_for_conversation(
    pool: &DbPool,
    tenant_id: &str,
    conversation_id: &str,
    item_id: &str,
) -> DbResult<Option<Item>> {
    sqlx::query_as(&format!(
        "SELECT {ENTRY_ITEM_COLUMNS} FROM conversation_entries entries \
         JOIN conversations ON conversations.id = entries.conversation_id \
         JOIN items ON items.id = entries.item_id \
         WHERE conversations.id = $1 AND conversations.tenant_id = $2 AND conversations.deleted_at IS NULL \
           AND entries.removed_revision IS NULL AND entries.public_id = $3 \
         ORDER BY entries.seq ASC LIMIT 1"
    ))
    .bind(conversation_id)
    .bind(tenant_id)
    .bind(item_id)
    .fetch_optional(pool)
    .await
}

/// Remove one or all items from a locked conversation's current list at `revision`. Stored
/// responses keep reading the conversation as it was at their own turn.
///
/// # Errors
/// Returns an error if the update fails.
pub async fn detach_from_conversation_in_tx(
    tx: &mut DbTransaction<'_>,
    conversation_id: &str,
    item_id: Option<&str>,
    revision: i64,
) -> DbResult<u64> {
    let result = sqlx::query(
        "UPDATE conversation_entries SET removed_revision = $3 \
         WHERE conversation_id = $1 AND removed_revision IS NULL AND (CAST($2 AS TEXT) IS NULL OR public_id = $2)",
    )
    .bind(conversation_id)
    .bind(item_id)
    .bind(revision)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::types::event::MessageStatus;
    use crate::types::io::{InputItem, OutputItem, ReasoningOutput, ReasoningTextContent};

    async fn pool_with_response(response_id: &str) -> Arc<DbPool> {
        let pool = crate::storage::create_pool_with_schema(Some("sqlite://?mode=memory"))
            .await
            .expect("create in-memory database");
        let mut transaction = pool.begin().await.expect("begin transaction");
        crate::storage::models::response::create_in_tx(
            &mut transaction,
            &crate::storage::models::response::NewResponse {
                id: response_id,
                tenant_id: "default_tenant",
                conversation_id: None,
                conversation_revision: None,
                previous_response_id: None,
                history_base: crate::storage::models::response::HistoryBase::Chain,
                metadata: None,
            },
        )
        .await
        .expect("create response");
        transaction.commit().await.expect("commit response");
        pool
    }

    #[test]
    fn numbered_rows_bind_each_column_in_order() {
        assert_eq!(numbered_rows(2, 3, 1), "($1, $2, $3), ($4, $5, $6)");
        assert_eq!(numbered_rows(1, 2, 5), "($5, $6)");
    }

    #[tokio::test]
    async fn item_queries_chunk_above_portable_bind_limit() {
        let pool = pool_with_response("resp_batches").await;
        let items = (0..=MAX_ITEMS_PER_INSERT)
            .map(|index| InsertItem::unmarked(format!("item_{index}"), "{}".to_owned()))
            .collect::<Vec<_>>();
        let public_ids = items.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
        let mut transaction = pool.begin().await.expect("begin transaction");
        let created = create_for_response_in_tx(&mut transaction, "default_tenant", "resp_batches", items)
            .await
            .expect("insert item batches");
        transaction.commit().await.expect("commit item batches");
        let row_ids = created.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
        let loaded = get_items(&pool, &row_ids).await.expect("load item batches");

        assert_eq!(created.len(), MAX_ITEMS_PER_INSERT + 1);
        assert_eq!(loaded.len(), MAX_ITEMS_PER_INSERT + 1);
        assert_eq!(
            created.iter().map(Item::public_id).collect::<Vec<_>>(),
            public_ids.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert!(created.iter().all(|item| item.id != item.public_id()));
    }

    #[tokio::test]
    async fn repeated_public_ids_get_distinct_history_rows() {
        let pool = pool_with_response("resp_repeated").await;
        let items = vec![InsertItem::unmarked("item_repeated".to_owned(), "{}".to_owned()); 2];
        let mut transaction = pool.begin().await.expect("begin transaction");
        let created = create_for_response_in_tx(&mut transaction, "default_tenant", "resp_repeated", items)
            .await
            .expect("insert repeated public ids");
        transaction.commit().await.expect("commit history rows");

        assert_eq!(created.len(), 2);
        assert_ne!(created[0].id, created[1].id);
        assert_eq!(created[0].public_id(), "item_repeated");
        assert_eq!(created[1].public_id(), "item_repeated");
        let row_ids = created.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
        assert_eq!(get_items(&pool, &row_ids).await.expect("reload history rows").len(), 2);
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
        let item_count = MAX_BIND_PARAMETERS / ENTRY_INSERT_COLUMNS + 1;
        let items = (0..item_count)
            .map(|index| InsertItem::unmarked(format!("conversation_item_{index}"), "{}".to_owned()))
            .collect::<Vec<_>>();
        let mut transaction = pool.begin().await.expect("begin transaction");
        let created = attach_to_conversation_in_tx(&mut transaction, conversation_id, "default_tenant", 1, items, None)
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
            public_id: None,
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
            public_id: None,
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
            public_id: None,
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
            public_id: None,
        };

        let stored = item.as_inout().expect("stored item");
        assert!(matches!(stored, InOutItem::Output(OutputItem::Message(_))));

        let inputs = InOutItem::into_input_items(vec![stored]);
        assert!(matches!(inputs[0], InputItem::Message(_)));
    }

    #[test]
    fn test_namespaced_function_call_rehydrates_without_storage_marker() {
        let stored = InOutItem::Output(OutputItem::FunctionCall(crate::types::io::FunctionToolCall {
            async_execution: false,
            agent: None,
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
            public_id: None,
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
            public_id: None,
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
                async_execution: false,
                agent: None,
                id: "fc_1".to_string(),
                call_id: "call_1".to_string(),
                name: "run".to_string(),
                namespace: Some("mcp__shell".to_string()),
                arguments: "{\"cmd\":\"pwd\"}".to_string(),
                status: MessageStatus::Completed,
            })),
            InOutItem::Output(OutputItem::FunctionCall(crate::types::io::FunctionToolCall {
                async_execution: false,
                agent: None,
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
                public_id: None,
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
            public_id: None,
        };

        let inputs = InOutItem::into_input_items(vec![item.as_inout().expect("stored item")]);

        assert!(inputs.is_empty());
    }
}

#[cfg(test)]
mod item_id_tests {
    use super::*;
    use serde_json::json;

    fn stored(value: serde_json::Value) -> InOutItem {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn submitted_gateway_calls_remain_inputs_after_storage_round_trip() {
        for wire in [
            json!({"type":"web_search_call", "id":"ws_1", "status":"completed", "action":{"type":"search", "query":"weather"}}),
            json!({"type":"mcp_call", "id":"mcp_1", "server_label":"weather", "name":"forecast", "arguments":"{}", "output":"sunny"}),
        ] {
            let input = stored(wire);
            let expected = serde_json::to_value(&input).unwrap();
            let row = Item {
                id: "row_1".to_owned(),
                reference_id: None,
                public_id: None,
                data: String::try_from(&input).unwrap(),
                created_at: 0,
                conversation_id: None,
                seq: None,
                tenant_id: None,
            };
            let restored = row.as_inout().unwrap();
            assert!(matches!(restored, InOutItem::Input(_)));
            let replay = InOutItem::into_input_items(vec![restored]);
            assert_eq!(replay.len(), 1);
            assert_eq!(serde_json::to_value(&replay[0]).unwrap(), expected);
        }
    }

    #[test]
    fn supplied_ids_are_validated_by_item_type_without_changing_call_ids() {
        let cases = [
            json!({"type":"message", "id":"msg_1", "role":"user", "content":"hello"}),
            json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"f", "arguments":"{}"}),
            json!({"type":"tool_search_call", "id":"tsc_1", "call_id":"call_1", "arguments":{}}),
            json!({"type":"function_call_output", "id":"fc_1", "call_id":"call_1", "output":"done"}),
            json!({"type":"custom_tool_call_output", "id":"ctco_1", "call_id":"call_1", "output":"done"}),
            json!({"type":"shell_call_output", "id":"sho_1", "call_id":"call_1", "output":[]}),
            json!({"type":"tool_search_output", "id":"tso_1", "call_id":"call_1", "tools":[]}),
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
            item.validate_client_id("items[0].id".into())
                .expect("matching public ID prefix");
            value["id"] = json!("wrong_1");
            let item = stored(value);
            let error = item.validate_client_id("items[0].id".into()).unwrap_err();
            assert!(error.is_validation());
            assert!(matches!(error, StorageError::InvalidItemId { param, .. } if param == "items[0].id"));
        }
    }

    #[test]
    fn optional_ids_are_not_required() {
        for value in [
            json!({"type":"message", "role":"user", "content":"hello"}),
            json!({"type":"function_call", "call_id":"call_1", "name":"f", "arguments":"{}"}),
            json!({"type":"function_call_output", "call_id":"call_1", "output":"done"}),
            json!({"type":"shell_call_output", "call_id":"call_1", "output":[]}),
        ] {
            stored(value).validate_client_id("items[0].id".into()).unwrap();
        }
    }

    #[test]
    fn invalid_id_reports_its_batch_index_and_rejects_empty_ids() {
        for id in ["", "item_wrongprefix"] {
            let items = [
                stored(json!({"type":"message", "role":"user", "content":"valid"})),
                stored(json!({"type":"message", "id":id, "role":"user", "content":"invalid"})),
            ];
            let error = items
                .iter()
                .enumerate()
                .try_for_each(|(index, item)| item.validate_client_id(format!("items[{index}].id")))
                .unwrap_err();
            assert!(matches!(&error, StorageError::InvalidItemId { param, .. } if param == "items[1].id"));
            assert_eq!(
                error.to_string(),
                format!("Invalid 'items[1].id': '{id}'. Expected an ID that begins with 'msg'.")
            );
        }
    }
}
