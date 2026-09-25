//! Upgrade a real pre-provenance schema; legacy rows must never gain inferred trust.

use agentic_core::storage::{Item, SchemaManager, create_pool};

#[tokio::test]
async fn sqlite_upgrade_preserves_legacy_data_and_repeated_startup() {
    let pool = create_pool(Some("sqlite://?mode=memory")).await.unwrap();
    let mut migrations = sqlx::migrate!("./migrations");
    // Apply the exact embedded legacy migrations with their SQLx checksums.
    migrations.migrations = migrations
        .iter()
        .filter(|migration| migration.version < 7)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    migrations.run(pool.as_ref()).await.unwrap();
    let original = r#"{"type":"reasoning","id":"rs_legacy","encrypted_content":"opaque unchanged","replay_provenance":{"version":"1","source":{"origin":"client_submitted"}}}"#;
    sqlx::query("INSERT INTO items (id, data, created_at) VALUES ($1, $2, $3)")
        .bind("item_legacy")
        .bind(original)
        .bind(1_i64)
        .execute(pool.as_ref())
        .await
        .unwrap();
    let legacy = r#"{"type":"reasoning","id":"rs_old","content":[{"type":"legacy_text","text":"keep this"}],"summary":[{"type":"other","text":"old"}],"encrypted_content":{"provider":"old"},"status":"legacy_status"}"#;
    sqlx::query("INSERT INTO items (id, data, created_at) VALUES ($1, $2, $3)")
        .bind("item_old_shape")
        .bind(legacy)
        .bind(2_i64)
        .execute(pool.as_ref())
        .await
        .unwrap();
    for _ in 0..2 {
        SchemaManager::new(&pool).run_migrations().await.unwrap();
        let item: Item = sqlx::query_as("SELECT * FROM items WHERE id = $1")
            .bind("item_legacy")
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
        assert_eq!(item.data, original);
        assert!(item.reasoning_provenance.is_none());
        assert!(
            item.as_inout().unwrap().reasoning_provenance().is_none(),
            "JSON cannot backfill provenance"
        );
        let old_shape: Item = sqlx::query_as("SELECT * FROM items WHERE id = $1")
            .bind("item_old_shape")
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
        let item = old_shape.as_inout().expect("legacy reasoning remains readable");
        assert!(item.reasoning_provenance().is_none());
        let serialized = match item {
            agentic_core::storage::InOutItem::Output(output) => serde_json::to_value(output).unwrap(),
            agentic_core::storage::InOutItem::Input(input) => serde_json::to_value(input).unwrap(),
        };
        assert_eq!(serialized["content"][0]["text"], "keep this");
        assert_eq!(serialized["content"][0]["type"], "reasoning_text");
        let continuation = agentic_core::storage::InOutItem::into_input_items(vec![
            old_shape.as_inout().expect("legacy continuation item"),
        ]);
        assert_eq!(continuation.len(), 1);
        assert!(matches!(continuation[0], agentic_core::types::InputItem::Reasoning(_)));
    }
    let versions: Vec<i64> = sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(versions, vec![1, 2, 3, 4, 5, 6, 7]);
}
