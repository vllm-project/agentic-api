//! Independent item occurrences in stored response history.

use super::super::pool::{DbResult, DbTransaction};
use crate::utils::common::{utcnow_str, uuid7_str};

// Each row uses three parameters, plus one timestamp shared by the batch.
const MAX_ITEMS_PER_INSERT: usize = (999 - 1) / 3;

/// Store each occurrence under a fresh row ID, with its public ID kept separately.
/// Return row IDs in input order so repeated public IDs retain every position.
pub(crate) async fn create_in_tx(tx: &mut DbTransaction<'_>, items: &[(String, String)]) -> DbResult<Vec<String>> {
    let now = utcnow_str();
    let row_ids: Vec<_> = items.iter().map(|_| uuid7_str("item_")).collect();
    for (batch, ids) in items
        .chunks(MAX_ITEMS_PER_INSERT)
        .zip(row_ids.chunks(MAX_ITEMS_PER_INSERT))
    {
        let values = (0..batch.len())
            .map(|index| {
                let first = index * 3 + 2;
                format!("(${}, ${}, ${})", first, first + 1, first + 2)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "WITH incoming (id, public_id, data) AS (VALUES {values}) \
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
