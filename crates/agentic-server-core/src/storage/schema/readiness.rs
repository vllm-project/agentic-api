//! Persistence probes use the same item insert path as ordinary writes.

use super::{DatabaseBackend, DbPool, DbResult};

pub(crate) async fn verify_persistence_writable(pool: &DbPool) -> DbResult<()> {
    let mut transaction = pool.begin().await?;
    let probe_result = async {
        let suffix = uuid::Uuid::now_v7().simple();
        let conversation_id = format!("conv_readiness_{suffix}");
        let item_id = format!("item_readiness_{suffix}");
        let response_id = format!("resp_readiness_{suffix}");
        let created_at = crate::utils::common::utcnow_str();
        sqlx::query("INSERT INTO conversations (id, created_at) VALUES ($1, $2)")
            .bind(&conversation_id)
            .bind(created_at)
            .execute(&mut *transaction)
            .await?;
        crate::storage::models::conversation::lock_in_tx(&mut transaction, &conversation_id).await?;
        crate::storage::models::item::create_in_tx(
            &mut transaction,
            vec![crate::storage::models::item::InsertItem::unmarked(
                item_id.clone(),
                "{}".to_owned(),
            )],
            Some(&conversation_id),
        )
        .await
        .map_err(|error| match error {
            crate::storage::StorageError::Database(error) => error,
            other => sqlx::Error::Configuration(Box::new(other)),
        })?;
        crate::storage::models::response::create_in_tx(
            &mut transaction,
            &response_id,
            Some(&conversation_id),
            None,
            Some(&format!("[\"{item_id}\"]")),
            Some("{}"),
        )
        .await?;
        crate::storage::models::conversation::set_latest_response_in_tx(
            &mut transaction,
            &conversation_id,
            &response_id,
        )
        .await?;
        Ok(())
    }
    .await;
    match probe_result {
        Ok(()) => transaction.rollback().await,
        Err(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
    }
}

pub(crate) async fn verify_persistence_ready(pool: &DbPool) -> DbResult<()> {
    let mut connection = pool.acquire().await?;
    match DatabaseBackend::from_connection(&connection) {
        DatabaseBackend::Postgres => {
            let ready: bool = sqlx::query_scalar(
                "WITH required(table_name, privilege) AS ( \
                     VALUES \
                         ('conversations', 'SELECT'), \
                         ('conversations', 'INSERT'), \
                         ('conversations', 'UPDATE'), \
                         ('items', 'SELECT'), \
                         ('items', 'INSERT'), \
                         ('responses', 'SELECT'), \
                         ('responses', 'INSERT') \
                 ) \
                 SELECT current_setting('transaction_read_only') = 'off' \
                    AND COUNT(table_relation.oid) = 7 \
                    AND COALESCE(BOOL_AND( \
                        has_table_privilege(current_user, table_relation.oid, required.privilege) \
                    ), false) \
                 FROM required \
                 LEFT JOIN pg_class table_relation \
                   ON table_relation.relname = required.table_name \
                  AND table_relation.relkind IN ('r', 'p') \
                  AND pg_table_is_visible(table_relation.oid)",
            )
            .fetch_one(&mut *connection)
            .await?;
            if !ready {
                return Err(sqlx::Error::Configuration(
                    "PostgreSQL persistence tables are unavailable, read-only, or missing required privileges".into(),
                ));
            }
            sqlx::query("SELECT reasoning_provenance FROM items LIMIT 0")
                .execute(&mut *connection)
                .await?;
        }
        DatabaseBackend::Sqlite => {
            let query_only: i64 = sqlx::query_scalar("PRAGMA query_only")
                .fetch_one(&mut *connection)
                .await?;
            if query_only != 0 {
                return Err(sqlx::Error::Configuration("SQLite persistence is read-only".into()));
            }
            for statement in [
                "SELECT id FROM conversations LIMIT 0",
                "SELECT id, reasoning_provenance FROM items LIMIT 0",
                "SELECT id FROM responses LIMIT 0",
            ] {
                sqlx::query(statement).execute(&mut *connection).await?;
            }
        }
        DatabaseBackend::Other => {
            sqlx::query("SELECT 1").execute(&mut *connection).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn readiness_requires_provenance_column_without_backfilling_legacy_state() {
        let pool = crate::storage::create_pool(Some("sqlite://?mode=memory"))
            .await
            .unwrap();
        let mut migrations = sqlx::migrate!("./migrations");
        migrations.migrations = migrations
            .iter()
            .filter(|migration| migration.version < 7)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        migrations.run(pool.as_ref()).await.unwrap();
        assert!(verify_persistence_ready(pool.as_ref()).await.is_err());
        crate::storage::SchemaManager::new(pool.as_ref())
            .run_migrations()
            .await
            .unwrap();
        verify_persistence_ready(pool.as_ref()).await.unwrap();
        verify_persistence_writable(pool.as_ref()).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
        assert_eq!(count, 0, "the write probe rolls back");
    }
}
