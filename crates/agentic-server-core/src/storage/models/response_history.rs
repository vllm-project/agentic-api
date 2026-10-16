//! Independent item occurrences in stored response history.

use super::super::pool::{DbResult, DbTransaction};
use crate::utils::common::{utcnow_str, uuid7_str};

// Every row binds a row ID, a public ID, and data, plus one timestamp shared by the batch.
const ITEM_COLUMN_COUNT: usize = 3;
const MAX_BIND_PARAMETERS: usize = 999;
const MAX_ITEMS_PER_INSERT: usize = (MAX_BIND_PARAMETERS - 1) / ITEM_COLUMN_COUNT;

/// Create response history rows, one per occurrence.
///
/// Each occurrence receives a fresh row ID while its public ID is stored separately, so a public ID
/// can repeat across rows and across responses. Returns the row IDs in input order.
///
/// # Errors
/// Returns an error if an insert fails.
pub(crate) async fn create_in_tx(tx: &mut DbTransaction<'_>, items: &[(String, String)]) -> DbResult<Vec<String>> {
    let now = utcnow_str();
    let row_ids = items.iter().map(|_| uuid7_str("item_")).collect::<Vec<_>>();
    for (batch, ids) in items
        .chunks(MAX_ITEMS_PER_INSERT)
        .zip(row_ids.chunks(MAX_ITEMS_PER_INSERT))
    {
        let values_clause = (0..batch.len())
            .map(|index| {
                let first = index * ITEM_COLUMN_COUNT + 2;
                format!("(${first}, ${}, ${})", first + 1, first + 2)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "WITH incoming (id, public_id, data) AS (VALUES {values_clause}) \
             INSERT INTO items (id, public_id, data, created_at, tenant_id) \
             SELECT id, public_id, data, $1, 'default_tenant' FROM incoming"
        );
        let mut query = sqlx::query(&sql).bind(now);
        for ((public_id, data), id) in batch.iter().zip(ids) {
            query = query.bind(id).bind(public_id).bind(data);
        }
        query.execute(&mut **tx).await?;
    }
    Ok(row_ids)
}
