//! Conversation history item stored in the database.

use std::fmt::Write;

use super::super::pool::{DbPool, DbResult, DbTransaction};
use crate::storage::{StorageError, StoreResult};
use crate::types::io::ItemOrder;

const ITEM_COLUMN_COUNT: usize = 6;
const SEQUENCE_COLUMN_INDEX: usize = 5;
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

    /// Public identity shared by every occurrence of the item.
    pub public_id: String,

    /// Storage metadata for the item as JSON text (its input/output kind).
    pub metadata: String,

    /// The item exactly as serialized, as JSON text.
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

/// Create items in a transaction with optional conversation context.
///
/// If `conversation_id` is provided, the next sequence range is computed in the insert statement so
/// concurrent `SQLite` writers do not take a stale read snapshot before writing.
///
/// Without a conversation, each item is an independent response-history row stored under its own
/// row ID, so a repeated public ID can be stored once per occurrence. Rows are returned in input order.
///
/// # Errors
/// Returns an error if insertion fails or an item already belongs to the conversation.
pub async fn create_in_tx(
    tx: &mut DbTransaction<'_>,
    items: Vec<Item>,
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

async fn create_in_tx_without_conversation(tx: &mut DbTransaction<'_>, items: &[Item]) -> DbResult<Vec<Item>> {
    // Each occurrence has its own row ID; the public ID may repeat.
    let values_clause = (0..items.len())
        .map(|index| {
            let first = 5 * index + 1;
            format!(
                "(${first}, ${}, ${}, ${}, ${})",
                first + 1,
                first + 2,
                first + 3,
                first + 4
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "WITH incoming (id, public_id, metadata, data, created_at) AS (VALUES {values_clause}) \
         INSERT INTO items (id, public_id, metadata, data, created_at, tenant_id) \
         SELECT id, public_id, metadata, data, created_at, 'default_tenant' FROM incoming RETURNING *"
    );

    let mut query = sqlx::query_as::<_, Item>(&sql);
    for item in items {
        query = query
            .bind(&item.id)
            .bind(&item.public_id)
            .bind(&item.metadata)
            .bind(&item.data)
            .bind(item.created_at);
    }

    let mut rows = query.fetch_all(&mut **tx).await?;
    // `RETURNING` does not promise output order; restore it so history matches the request.
    rows.sort_by_key(|row| items.iter().position(|item| item.id == row.id));
    Ok(rows)
}

async fn create_in_tx_with_next_conversation_seq(
    tx: &mut DbTransaction<'_>,
    items: &[Item],
    conversation_id: &str,
    first_inserted_sequence: Option<i64>,
) -> StoreResult<Vec<Item>> {
    let values_clause = item_values_clause(items.len(), 3, true);
    // A repeated public ID resolves to the row whose own ID matches it, else the earliest one.
    let sql = format!(
        "WITH next_seq AS ( \
             SELECT COALESCE(MAX(seq), -1) + 1 AS start FROM items WHERE conversation_id = $1 \
         ), incoming (id, metadata, data, created_at, conversation_id, seq, entry_id) AS ( \
             VALUES {values_clause} \
         ), owner AS ( \
             SELECT COALESCE(tenant_id, 'default_tenant') AS tenant_id FROM conversations WHERE id = $1 \
         ), source_rows AS ( \
             SELECT incoming.seq AS incoming_seq, \
                    COALESCE(MIN(CASE WHEN source.id = incoming.id THEN source.id END), MIN(source.id)) AS source_id \
             FROM incoming CROSS JOIN owner JOIN items source \
                 ON source.public_id = incoming.id \
                AND source.tenant_id = owner.tenant_id \
                AND (CAST($2 AS BIGINT) IS NULL OR source.conversation_id IS NULL \
                     OR source.conversation_id <> $1 OR source.seq < $2) \
             GROUP BY incoming.seq \
         ) \
         INSERT INTO items (id, metadata, data, created_at, conversation_id, seq, public_id, tenant_id) \
         SELECT CASE WHEN source.id IS NULL THEN incoming.id ELSE incoming.entry_id END, \
                CASE WHEN source.id IS NULL THEN incoming.metadata ELSE source.metadata END, \
                COALESCE(source.data, incoming.data), COALESCE(source.created_at, incoming.created_at), \
                incoming.conversation_id, incoming.seq, COALESCE(source.public_id, incoming.id), owner.tenant_id \
         FROM incoming CROSS JOIN owner \
         LEFT JOIN source_rows ON source_rows.incoming_seq = incoming.seq \
         LEFT JOIN items source ON source.id = source_rows.source_id \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM items existing \
             JOIN incoming ON incoming.id = existing.public_id \
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
    for (idx, item) in items.iter().enumerate() {
        query = query
            .bind(&item.public_id)
            .bind(&item.metadata)
            .bind(&item.data)
            .bind(item.created_at)
            .bind(conversation_id)
            .bind(idx as i64)
            .bind(&item.id);
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

/// Find the original stored item carrying a public identity.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn get_by_public_id(pool: &DbPool, id: &str) -> DbResult<Option<Item>> {
    sqlx::query_as::<_, Item>("SELECT * FROM items WHERE public_id = $1 ORDER BY created_at, id LIMIT 1")
        .bind(id)
        .fetch_optional(pool)
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
            "{base} AND items.public_id = $3 \
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
         WHERE conversations.id = $1 AND conversations.tenant_id = $2 \
           AND items.public_id = $3 \
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
         WHERE conversation_id = $1 AND (CAST($2 AS TEXT) IS NULL OR public_id = $2)",
    )
    .bind(conversation_id)
    .bind(item_id)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::super::conversation;
    use super::*;
    use crate::utils::common::{utcnow_str, uuid7_str};

    fn row(public_id: String) -> Item {
        Item {
            id: uuid7_str("item_"),
            public_id,
            metadata: r#"{"kind":"input"}"#.to_owned(),
            data: "{}".to_owned(),
            created_at: utcnow_str(),
            conversation_id: None,
            seq: None,
            tenant_id: None,
        }
    }

    #[test]
    fn item_values_clause_numbers_plain_rows() {
        assert_eq!(
            item_values_clause(2, 1, false),
            "($1, $2, $3, $4, $5, $6), ($7, $8, $9, $10, $11, $12)"
        );
    }

    #[test]
    fn item_values_clause_numbers_conversation_rows_after_cte_bind() {
        assert_eq!(
            item_values_clause(2, 2, true),
            "($2, $3, $4, $5, $6, (SELECT start + $7 FROM next_seq), $8), \
             ($9, $10, $11, $12, $13, (SELECT start + $14 FROM next_seq), $15)"
        );
    }

    #[tokio::test]
    async fn item_queries_chunk_above_portable_bind_limit() {
        let pool = crate::storage::create_pool_with_schema(Some("sqlite://?mode=memory"))
            .await
            .expect("create in-memory database");
        let items = (0..=MAX_BIND_PARAMETERS)
            .map(|index| row(format!("item_{index}")))
            .collect::<Vec<_>>();
        let public_ids = items.iter().map(|item| item.public_id.clone()).collect::<Vec<_>>();
        let mut transaction = pool.begin().await.expect("begin transaction");
        let created = create_in_tx(&mut transaction, items, None)
            .await
            .expect("insert item batches");
        transaction.commit().await.expect("commit item batches");

        assert_eq!(created.len(), MAX_BIND_PARAMETERS + 1);
        assert_eq!(
            created.iter().map(|item| item.public_id.as_str()).collect::<Vec<_>>(),
            public_ids.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert!(created.iter().all(|item| item.id != item.public_id));
    }

    #[tokio::test]
    async fn repeated_public_ids_get_distinct_history_rows() {
        let pool = crate::storage::create_pool_with_schema(Some("sqlite://?mode=memory"))
            .await
            .expect("create in-memory database");
        let items = vec![row("item_repeated".to_owned()), row("item_repeated".to_owned())];
        let mut transaction = pool.begin().await.expect("begin transaction");
        let created = create_in_tx(&mut transaction, items, None)
            .await
            .expect("insert repeated public ids");
        transaction.commit().await.expect("commit history rows");

        assert_eq!(created.len(), 2);
        assert_ne!(created[0].id, created[1].id);
        assert_eq!(created[0].public_id, "item_repeated");
        assert_eq!(created[1].public_id, "item_repeated");
    }

    #[tokio::test]
    async fn conversation_item_batches_keep_contiguous_sequences() {
        let pool = crate::storage::create_pool_with_schema(Some("sqlite://?mode=memory"))
            .await
            .expect("create in-memory database");
        let conversation_id = "conv_batch";
        let item_count = MAX_ITEMS_PER_INSERT + 1;
        let items = (0..item_count)
            .map(|index| row(format!("conversation_item_{index}")))
            .collect::<Vec<_>>();
        let mut transaction = pool.begin().await.expect("begin transaction");
        conversation::create_in_tx(&mut transaction, conversation_id, None, None)
            .await
            .expect("create conversation");
        let created = create_in_tx(&mut transaction, items, Some(conversation_id))
            .await
            .expect("insert conversation item batches");
        transaction.commit().await.expect("commit item batches");
        let stored = conversation::get_with_items(&pool, conversation_id)
            .await
            .expect("load conversation item batches")
            .expect("conversation exists")
            .items;
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
}
