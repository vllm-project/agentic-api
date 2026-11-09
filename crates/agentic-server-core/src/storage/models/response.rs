//! LLM API response stored in the database.

use std::collections::HashSet;

use super::super::pool::{DbResult, DbTransaction};
use crate::storage::types::response::encode_history;
use crate::utils::common::utcnow_str;

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

    /// History item IDs as JSON array string.
    pub history_item_ids: Option<String>,

    /// Response metadata as JSON object string.
    pub metadata: Option<String>,

    /// The response exactly as returned, as JSON text; absent for responses that cannot be retrieved.
    pub snapshot: Option<String>,

    /// Creation timestamp as Unix timestamp in seconds.
    pub created_at: i64,
}

/// Create a response in a transaction and return it.
///
/// # Errors
/// Returns `DbResult::Err` if the database insertion fails.
pub async fn create_in_tx(
    tx: &mut DbTransaction<'_>,
    id: &str,
    conversation_id: Option<&str>,
    previous_response_id: Option<&str>,
    history_item_ids: Option<&str>,
    snapshot: Option<&str>,
    metadata: Option<&str>,
) -> DbResult<Response> {
    let now = utcnow_str();
    sqlx::query_as::<_, Response>(
        "INSERT INTO responses \
         (id, conversation_id, previous_response_id, history_item_ids, snapshot, metadata, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *",
    )
    .bind(id)
    .bind(conversation_id)
    .bind(previous_response_id)
    .bind(history_item_ids)
    .bind(snapshot)
    .bind(metadata)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
}

/// Get a response by ID through a pool or the caller's transaction.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get<'e, E>(executor: E, id: &str) -> DbResult<Option<Response>>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    sqlx::query_as::<_, Response>("SELECT * FROM responses WHERE id = $1")
        .bind(id)
        .fetch_optional(executor)
        .await
}

/// Delete a response and remove its inherited context from response-chain descendants.
/// Conversation snapshots and shared item rows are retained.
/// The caller owns the transaction and commits or rolls back all changes together.
///
/// # Errors
/// Returns an error if history is malformed or the transaction fails.
pub async fn delete_in_tx(tx: &mut DbTransaction<'_>, id: &str) -> DbResult<bool> {
    let Some(root) = get(&mut **tx, id).await? else {
        return Ok(false);
    };
    let inherited: HashSet<String> = root
        .history_item_ids_vec()
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))?
        .into_iter()
        .collect();
    let mut parents = vec![root.id];
    while let Some(parent) = parents.pop() {
        let children =
            sqlx::query_as::<_, Response>("SELECT * FROM responses WHERE previous_response_id = $1 ORDER BY id")
                .bind(parent)
                .fetch_all(&mut **tx)
                .await?;
        // Children continue by `previous_response_id`, so none is a conversation turn.
        for child in children {
            let mut ids = child
                .history_item_ids_vec()
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
            ids.retain(|id| !inherited.contains(id));
            let json = encode_history(&ids).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
            sqlx::query("UPDATE responses SET history_item_ids = $1 WHERE id = $2")
                .bind(json)
                .bind(&child.id)
                .execute(&mut **tx)
                .await?;
            parents.push(child.id);
        }
    }
    sqlx::query("DELETE FROM responses WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_history_ids_empty() {
        let response = Response {
            id: "test".to_string(),
            conversation_id: None,
            previous_response_id: None,
            history_item_ids: None,
            snapshot: None,
            metadata: None,
            created_at: 1_704_067_200,
        };

        let ids = response.history_item_ids_vec().expect("legacy empty history");
        assert!(ids.is_empty());
    }

    #[test]
    fn test_response_history_ids_valid() {
        let response = Response {
            id: "test".to_string(),
            conversation_id: None,
            previous_response_id: None,
            history_item_ids: Some(r#"["item_1", "item_2"]"#.to_string()),
            snapshot: None,
            metadata: None,
            created_at: 1_704_067_200,
        };

        let ids = response.history_item_ids_vec().expect("valid history references");
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "item_1");
    }
}
