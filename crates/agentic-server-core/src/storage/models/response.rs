//! LLM API response stored in the database.

use std::collections::HashSet;

use super::super::pool::{DbPool, DbResult, DbTransaction};
use crate::utils::common::{serialize_to_string, utcnow_str};

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
    metadata: Option<&str>,
) -> DbResult<Response> {
    let now = utcnow_str();
    sqlx::query_as::<_, Response>(
        "INSERT INTO responses \
         (id, conversation_id, previous_response_id, history_item_ids, metadata, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(id)
    .bind(conversation_id)
    .bind(previous_response_id)
    .bind(history_item_ids)
    .bind(metadata)
    .bind(now)
    .fetch_one(&mut **tx)
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

/// Delete a response and remove its inherited context from response-chain descendants.
/// Conversation snapshots and shared item rows are retained.
/// The caller owns the transaction and commits or rolls back all changes together.
///
/// # Errors
/// Returns an error if history is malformed or the transaction fails.
pub async fn delete_in_tx(tx: &mut DbTransaction<'_>, id: &str) -> DbResult<bool> {
    let Some(root) = get_in_tx(tx, id).await? else {
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
        for child in children {
            if child.conversation_id.is_none() {
                let mut ids = child
                    .history_item_ids_vec()
                    .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
                ids.retain(|id| !inherited.contains(id));
                let json = serialize_to_string(&ids).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
                sqlx::query("UPDATE responses SET history_item_ids = $1 WHERE id = $2")
                    .bind(json)
                    .bind(&child.id)
                    .execute(&mut **tx)
                    .await?;
            }
            parents.push(child.id);
        }
    }
    sqlx::query("DELETE FROM responses WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

/// Read the ordered history item IDs without loading response metadata.
/// A legacy NULL history is empty; a missing response returns `None`.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn get_item_ids(pool: &DbPool, id: &str) -> DbResult<Option<String>> {
    sqlx::query_scalar("SELECT COALESCE(history_item_ids, '[]') FROM responses WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
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
    /// Deserialize history references, treating only SQL NULL as legacy empty history.
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

    #[test]
    fn test_response_history_ids_empty() {
        let response = Response {
            id: "test".to_string(),
            conversation_id: None,
            previous_response_id: None,
            history_item_ids: None,
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
            metadata: None,
            created_at: 1_704_067_200,
        };

        let ids = response.history_item_ids_vec().expect("valid history references");
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "item_1");
    }

    #[test]
    fn test_response_metadata_deserialize() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct TestMeta {
            model: String,
        }

        let response = Response {
            id: "resp_1".to_string(),
            conversation_id: None,
            previous_response_id: None,
            history_item_ids: None,
            metadata: Some(r#"{"model":"gpt-4"}"#.to_string()),
            created_at: 1_704_067_200,
        };

        let meta: Option<TestMeta> = response.metadata_as().expect("valid metadata");
        assert!(meta.is_some());
    }
}
