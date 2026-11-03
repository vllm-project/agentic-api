//! LLM API response stored in the database.

use sqlx::AnyConnection;

use super::super::pool::{DbPool, DbResult, DbTransaction};
use super::item::Item;
use crate::utils::common::utcnow_str;

/// Written into `history_item_ids`, which the chain history store no longer reads. It is not an
/// array, so a binary from before the store fails closed instead of reading empty history.
pub(crate) const NO_HISTORY_LIST: &str = "{}";

/// Bind parameters per statement, matching the item batches.
const MAX_BIND_PARAMETERS: usize = 999;

/// Where a response's history begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryBase {
    /// The previous response's history, then this response's own items.
    Chain,
    /// The ordered `response_history_refs` rows, such as a response stored before the chain
    /// history store.
    List,
    /// The conversation's items as of `conversation_revision`.
    Conversation,
}

impl HistoryBase {
    const fn code(self) -> i64 {
        match self {
            Self::Chain => 0,
            Self::List => 1,
            Self::Conversation => 2,
        }
    }

    fn from_code(code: i64) -> Option<Self> {
        match code {
            0 => Some(Self::Chain),
            1 => Some(Self::List),
            2 => Some(Self::Conversation),
            _ => None,
        }
    }
}

/// LLM API response stored in the database.
///
/// Maps to the `responses` table and represents a single API response
/// with its metadata and history chain.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Response {
    /// Unique response identifier.
    pub id: String,

    /// Optional conversation this response belongs to.
    pub conversation_id: Option<String>,

    /// Optional reference to previous response for chaining.
    pub previous_response_id: Option<String>,

    /// History item IDs as a JSON array string, read only to convert rows stored before the
    /// chain history store.
    pub history_item_ids: Option<String>,

    /// Response metadata as JSON object string.
    pub metadata: Option<String>,

    /// Creation timestamp as Unix timestamp in seconds.
    pub created_at: i64,

    /// Where the history begins; see [`HistoryBase`].
    pub history_base: i64,

    /// Conversation revision committed by this turn, for conversation-based history.
    pub conversation_revision: Option<i64>,
}

/// A response row to insert.
#[derive(Debug, Clone, Copy)]
pub struct NewResponse<'a> {
    pub id: &'a str,
    pub tenant_id: &'a str,
    pub conversation_id: Option<&'a str>,
    pub conversation_revision: Option<i64>,
    pub previous_response_id: Option<&'a str>,
    pub history_base: HistoryBase,
    pub metadata: Option<&'a str>,
}

/// Create a response in a transaction and return it.
///
/// Returns `None` without writing when `previous_response_id` names no stored response.
///
/// # Errors
/// Returns `DbResult::Err` if the database insertion fails.
pub async fn create_in_tx(tx: &mut DbTransaction<'_>, row: &NewResponse<'_>) -> DbResult<Option<Response>> {
    sqlx::query_as::<_, Response>(
        "INSERT INTO responses \
         (id, tenant_id, conversation_id, conversation_revision, previous_response_id, history_base, \
          history_item_ids, metadata, created_at) \
         SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9 \
         WHERE CAST($5 AS TEXT) IS NULL OR EXISTS (SELECT 1 FROM responses parent WHERE parent.id = $5) \
         RETURNING *",
    )
    .bind(row.id)
    .bind(row.tenant_id)
    .bind(row.conversation_id)
    .bind(row.conversation_revision)
    .bind(row.previous_response_id)
    .bind(row.history_base.code())
    .bind(NO_HISTORY_LIST)
    .bind(row.metadata)
    .bind(utcnow_str())
    .fetch_optional(&mut **tx)
    .await
}

/// Get a response by ID.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get(pool: &DbPool, id: &str) -> DbResult<Option<Response>> {
    sqlx::query_as::<_, Response>("SELECT * FROM responses WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Read a response row inside the caller's transaction.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn get_in_tx(tx: &mut DbTransaction<'_>, id: &str) -> DbResult<Option<Response>> {
    sqlx::query_as::<_, Response>("SELECT * FROM responses WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
}

#[derive(sqlx::FromRow)]
struct HistoryRow {
    depth: i64,
    id: Option<String>,
    reference_id: Option<String>,
    public_id: Option<String>,
    data: Option<String>,
    created_at: Option<i64>,
    conversation_id: Option<String>,
    seq: Option<i64>,
    tenant_id: Option<String>,
}

/// Walk `previous_response_id` while the history base is a chain, then read the base it ends at.
/// The first row marks the target itself, so a missing response is distinguishable from an empty
/// history. Its NULLs are typed because PostgreSQL resolves each `UNION ALL` column left to right.
const HISTORY_SQL: &str = "\
WITH RECURSIVE walk (id, previous, base, conversation_id, revision, depth) AS ( \
    SELECT id, previous_response_id, history_base, conversation_id, conversation_revision, 0 \
    FROM responses WHERE id = $1 \
    UNION ALL \
    SELECT r.id, r.previous_response_id, r.history_base, r.conversation_id, r.conversation_revision, walk.depth + 1 \
    FROM walk JOIN responses r ON r.id = walk.previous \
    WHERE walk.base = 0 \
) \
SELECT -1 AS depth, 0 AS ord, CAST(NULL AS TEXT) AS id, CAST(NULL AS TEXT) AS reference_id, \
       CAST(NULL AS TEXT) AS public_id, CAST(NULL AS TEXT) AS data, CAST(NULL AS BIGINT) AS created_at, \
       CAST(NULL AS TEXT) AS conversation_id, CAST(NULL AS BIGINT) AS seq, CAST(NULL AS TEXT) AS tenant_id \
FROM walk WHERE walk.depth = 0 \
UNION ALL \
SELECT walk.depth, items.position, items.id, items.reference_id, items.public_id, items.data, \
       items.created_at, NULL, NULL, items.tenant_id \
FROM walk JOIN items ON items.response_id = walk.id \
WHERE walk.base = 0 \
UNION ALL \
SELECT walk.depth, refs.position, items.id, items.reference_id, items.public_id, items.data, \
       items.created_at, items.conversation_id, items.seq, items.tenant_id \
FROM walk JOIN response_history_refs refs ON refs.response_id = walk.id \
JOIN items ON items.id = refs.item_id \
WHERE walk.base = 1 \
UNION ALL \
SELECT walk.depth, entries.seq, items.id, items.reference_id, entries.public_id, items.data, \
       items.created_at, entries.conversation_id, entries.seq, items.tenant_id \
FROM walk JOIN conversation_entries entries ON entries.conversation_id = walk.conversation_id \
    AND entries.added_revision <= walk.revision \
    AND (entries.removed_revision IS NULL OR entries.removed_revision > walk.revision) \
JOIN items ON items.id = entries.item_id \
WHERE walk.base = 2 \
ORDER BY 1 DESC, 2";

/// Load a response's full ordered history in one query, or `None` if the response does not exist.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn history(connection: &mut AnyConnection, id: &str) -> DbResult<Option<Vec<Item>>> {
    let rows = sqlx::query_as::<_, HistoryRow>(HISTORY_SQL)
        .bind(id)
        .fetch_all(&mut *connection)
        .await?;
    if !rows.iter().any(|row| row.depth == -1) {
        return Ok(None);
    }
    Ok(Some(
        rows.into_iter()
            .filter_map(|row| {
                Some(Item {
                    id: row.id?,
                    reference_id: row.reference_id,
                    public_id: row.public_id,
                    data: row.data?,
                    created_at: row.created_at?,
                    conversation_id: row.conversation_id,
                    seq: row.seq,
                    tenant_id: row.tenant_id,
                })
            })
            .collect(),
    ))
}

/// Delete a response; descendants lose everything it had in its history.
///
/// Chain children lose the deleted response as their parent (`ON DELETE SET NULL`), which cuts
/// everything above them. Descendants with an explicit history list drop their references to the deleted history.
/// A descendant whose history is a conversation snapshot, and everything below it, keeps its
/// history. Conversation snapshots and item rows are retained.
/// The caller owns the transaction and commits or rolls back all changes together.
///
/// # Errors
/// Returns an error if the transaction fails.
pub async fn delete_in_tx(tx: &mut DbTransaction<'_>, id: &str) -> DbResult<bool> {
    let Some(inherited) = history(tx, id).await? else {
        return Ok(false);
    };
    let listed: Vec<String> = sqlx::query_scalar(
        "WITH RECURSIVE descendants (id, base, conversation_id) AS ( \
             SELECT id, history_base, conversation_id FROM responses \
             WHERE previous_response_id = $1 AND history_base <> 2 \
             UNION ALL \
             SELECT r.id, r.history_base, r.conversation_id \
             FROM descendants JOIN responses r ON r.previous_response_id = descendants.id \
             WHERE r.history_base <> 2 \
         ) SELECT id FROM descendants WHERE base = 1 AND conversation_id IS NULL",
    )
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    let inherited = inherited.into_iter().map(|item| item.id).collect::<Vec<_>>();
    for response_id in &listed {
        for batch in inherited.chunks(MAX_BIND_PARAMETERS - 1) {
            let placeholders = (2..batch.len() + 2)
                .map(|index| format!("${index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql =
                format!("DELETE FROM response_history_refs WHERE response_id = $1 AND item_id IN ({placeholders})");
            let mut query = sqlx::query(&sql).bind(response_id);
            for item_id in batch {
                query = query.bind(item_id);
            }
            query.execute(&mut **tx).await?;
        }
    }
    sqlx::query("DELETE FROM responses WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

/// Store a response's explicit ordered history.
///
/// # Errors
/// Returns an error if insertion fails.
pub async fn insert_history_refs_in_tx(
    tx: &mut DbTransaction<'_>,
    response_id: &str,
    item_ids: &[String],
) -> DbResult<()> {
    for (batch_index, batch) in item_ids.chunks(MAX_BIND_PARAMETERS / 3).enumerate() {
        let values = (0..batch.len())
            .map(|row| format!("(${}, ${}, ${})", 3 * row + 1, 3 * row + 2, 3 * row + 3))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("INSERT INTO response_history_refs (response_id, position, item_id) VALUES {values}");
        let mut query = sqlx::query(&sql);
        for (offset, item_id) in batch.iter().enumerate() {
            let position = batch_index * (MAX_BIND_PARAMETERS / 3) + offset;
            query = query
                .bind(response_id)
                .bind(i64::try_from(position).unwrap_or(i64::MAX))
                .bind(item_id);
        }
        query.execute(&mut **tx).await?;
    }
    Ok(())
}

/// Get the response that committed a conversation turn.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_conversation_turn(
    pool: &DbPool,
    conversation_id: &str,
    response_id: &str,
) -> DbResult<Option<Response>> {
    sqlx::query_as::<_, Response>(
        "SELECT * FROM responses \
         WHERE id = $1 AND conversation_id = $2",
    )
    .bind(response_id)
    .bind(conversation_id)
    .fetch_optional(pool)
    .await
}

impl Response {
    /// Where this response's history begins, or `None` for an unknown stored code.
    #[must_use]
    pub fn base(&self) -> Option<HistoryBase> {
        HistoryBase::from_code(self.history_base)
    }

    /// Deserialize a pre-migration history list, treating only SQL NULL as empty history.
    ///
    /// # Errors
    /// Returns an error for malformed JSON or a value other than an array of strings.
    pub fn history_item_ids_vec(&self) -> Result<Vec<String>, serde_json::Error> {
        self.history_item_ids
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map(Option::unwrap_or_default)
    }

    /// Deserialize metadata from JSON string to the given type.
    ///
    /// # Errors
    /// Returns an error if present metadata does not match the requested schema.
    pub fn metadata_as<T: serde::de::DeserializeOwned>(&self) -> Result<Option<T>, serde_json::Error> {
        self.metadata.as_deref().map(serde_json::from_str).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(history_item_ids: Option<&str>, metadata: Option<&str>) -> Response {
        Response {
            id: "test".to_string(),
            conversation_id: None,
            previous_response_id: None,
            history_item_ids: history_item_ids.map(str::to_owned),
            metadata: metadata.map(str::to_owned),
            created_at: 1_704_067_200,
            history_base: HistoryBase::List.code(),
            conversation_revision: None,
        }
    }

    #[test]
    fn test_response_history_ids_empty() {
        let ids = response(None, None)
            .history_item_ids_vec()
            .expect("legacy empty history");
        assert!(ids.is_empty());
    }

    #[test]
    fn test_response_history_ids_valid() {
        let ids = response(Some(r#"["item_1", "item_2"]"#), None)
            .history_item_ids_vec()
            .expect("valid history references");
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "item_1");
    }

    #[test]
    fn test_response_metadata_deserialize() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct TestMeta {
            model: String,
        }

        let meta: Option<TestMeta> = response(None, Some(r#"{"model":"gpt-4"}"#))
            .metadata_as()
            .expect("valid metadata");
        assert!(meta.is_some());
    }

    #[test]
    fn history_base_codes_round_trip() {
        for base in [HistoryBase::Chain, HistoryBase::List, HistoryBase::Conversation] {
            assert_eq!(HistoryBase::from_code(base.code()), Some(base));
        }
        assert_eq!(HistoryBase::from_code(3), None);
    }

    #[test]
    fn new_responses_store_a_list_older_binaries_reject() {
        assert!(serde_json::from_str::<Vec<String>>(NO_HISTORY_LIST).is_err());
    }
}
