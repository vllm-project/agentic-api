//! One-time conversion of pre-migration history lists into explicit history lists.
//!
//! Migration 0008 gives every stored response an explicit history list. Its ordered `history_item_ids`
//! JSON array is expanded here, because SQLite and PostgreSQL expand JSON arrays with different
//! functions. Converted rows hold the non-array marker, so later startups find nothing to do.

use sqlx::{AnyConnection, Connection};

use crate::storage::models::response::{self, NO_HISTORY_LIST};

type DbResult<T> = Result<T, sqlx::Error>;

const BATCH: i64 = 200;

/// Convert every remaining history list. Malformed lists stop startup with the response named.
pub(super) async fn convert_history_lists(connection: &mut AnyConnection) -> DbResult<()> {
    loop {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, history_item_ids FROM responses \
             WHERE history_item_ids IS NOT NULL AND history_item_ids <> $1 ORDER BY id LIMIT $2",
        )
        .bind(NO_HISTORY_LIST)
        .bind(BATCH)
        .fetch_all(&mut *connection)
        .await?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = connection.begin().await?;
        for (response_id, list) in rows {
            let item_ids: Vec<String> = serde_json::from_str(&list).map_err(|_| {
                sqlx::Error::Protocol(format!(
                    "response '{response_id}' has a malformed history_item_ids list"
                ))
            })?;
            response::insert_history_refs_in_tx(&mut tx, &response_id, &item_ids).await?;
            sqlx::query("UPDATE responses SET history_item_ids = $1 WHERE id = $2")
                .bind(NO_HISTORY_LIST)
                .bind(&response_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
}
