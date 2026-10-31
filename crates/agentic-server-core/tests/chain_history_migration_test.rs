//! Upgrading a database written by migrations 0001-0007 to the chain history store.

use std::borrow::Cow;

use agentic_core::storage::{
    ConversationStore, DbPool, ResponseMetadata, ResponseStore, create_pool, create_pool_with_schema,
};
use agentic_core::types::io::InOutItem;
use serde_json::json;

fn message(id: &str) -> InOutItem {
    InOutItem::Input(
        serde_json::from_value(json!({"type": "message", "id": id, "role": "user", "content": id})).unwrap(),
    )
}

fn public_ids(items: &[InOutItem]) -> Vec<&str> {
    items.iter().map(|item| item.id().unwrap()).collect()
}

/// Apply only the migrations that existed before the chain history store.
async fn migrate_to_0007(pool: &DbPool) {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.migrations = Cow::Owned(
        migrator
            .migrations
            .iter()
            .filter(|migration| migration.version <= 7)
            .cloned()
            .collect(),
    );
    migrator.run(pool).await.expect("apply migrations 0001-0007");
}

async fn insert_item(pool: &DbPool, id: &str, public_id: Option<&str>, conversation: Option<(&str, i64)>) {
    let data = String::try_from(&message(public_id.unwrap_or(id))).unwrap();
    sqlx::query(
        "INSERT INTO items (id, public_id, data, created_at, conversation_id, seq, tenant_id) \
         VALUES ($1, $2, $3, $4, $5, $6, 'default_tenant')",
    )
    .bind(id)
    .bind(public_id)
    .bind(data)
    .bind(1_704_067_200_i64)
    .bind(conversation.map(|(conversation_id, _)| conversation_id))
    .bind(conversation.map(|(_, seq)| seq))
    .execute(pool)
    .await
    .expect("seed item");
}

async fn insert_response(pool: &DbPool, id: &str, conversation: Option<&str>, previous: Option<&str>, history: &str) {
    sqlx::query(
        "INSERT INTO responses (id, conversation_id, previous_response_id, history_item_ids, metadata, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(conversation)
    .bind(previous)
    .bind(history)
    .bind(String::try_from(&ResponseMetadata::default()).unwrap())
    .bind(1_704_067_200_i64)
    .execute(pool)
    .await
    .expect("seed response");
}

/// Data as the storage code before the chain history store wrote it.
async fn seed_0007_data(pool: &DbPool) {
    // A Responses chain: each response lists its full history.
    insert_item(pool, "item_a", Some("msg_a"), None).await;
    insert_item(pool, "item_b", Some("msg_b"), None).await;
    insert_response(pool, "resp_first", None, None, r#"["item_a","item_b"]"#).await;
    insert_item(pool, "item_c", Some("msg_c"), None).await;
    insert_response(
        pool,
        "resp_second",
        None,
        Some("resp_first"),
        r#"["item_a","item_b","item_c"]"#,
    )
    .await;

    // A conversation with a row from before public IDs, and an item deleted after the stored turn.
    sqlx::query(
        "INSERT INTO conversations (id, created_at, tenant_id, revision, latest_response_id) \
         VALUES ('conv_legacy', 1704067200, 'default_tenant', 2, NULL)",
    )
    .execute(pool)
    .await
    .expect("seed conversation");
    insert_item(pool, "msg_old", None, Some(("conv_legacy", 0))).await;
    insert_item(pool, "msg_kept", Some("msg_kept"), Some(("conv_legacy", 1))).await;
    insert_item(pool, "msg_deleted", Some("msg_deleted"), None).await;
    insert_item(pool, "msg_answer", Some("msg_answer"), Some(("conv_legacy", 2))).await;
    insert_response(
        pool,
        "resp_turn",
        Some("conv_legacy"),
        None,
        r#"["msg_old","msg_kept","msg_deleted","msg_answer"]"#,
    )
    .await;
    sqlx::query("UPDATE conversations SET latest_response_id = 'resp_turn' WHERE id = 'conv_legacy'")
        .execute(pool)
        .await
        .expect("seed latest turn");
}

async fn assert_upgrade_preserves_history(database_url: &str) {
    let legacy = create_pool(Some(database_url)).await.expect("open database");
    migrate_to_0007(&legacy).await;
    seed_0007_data(&legacy).await;
    legacy.close().await;

    let pool = create_pool_with_schema(Some(database_url))
        .await
        .expect("upgrade database");
    let responses = ResponseStore::new(pool.clone());
    let conversations = ConversationStore::new(pool.clone());

    // Every stored history reads back exactly.
    assert_eq!(
        public_ids(&responses.rehydrate("resp_first").await.unwrap()),
        ["msg_a", "msg_b"]
    );
    assert_eq!(
        public_ids(&responses.rehydrate("resp_second").await.unwrap()),
        ["msg_a", "msg_b", "msg_c"]
    );
    assert_eq!(
        public_ids(&responses.rehydrate("resp_turn").await.unwrap()),
        ["msg_old", "msg_kept", "msg_deleted", "msg_answer"]
    );
    let snapshot = conversations.rehydrate_snapshot("conv_legacy").await.unwrap();
    assert_eq!(public_ids(&snapshot.items), ["msg_old", "msg_kept", "msg_answer"]);
    assert_eq!(snapshot.version.response_id.as_deref(), Some("resp_turn"));

    // New turns build on converted data.
    responses
        .persist(
            "resp_third",
            Some("resp_second"),
            vec![message("msg_d")],
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        public_ids(&responses.rehydrate("resp_third").await.unwrap()),
        ["msg_a", "msg_b", "msg_c", "msg_d"]
    );
    conversations
        .persist_if_version(
            "conv_legacy",
            snapshot.version,
            "resp_next_turn",
            None,
            vec![message("msg_next")],
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
    let after = conversations.rehydrate_snapshot("conv_legacy").await.unwrap();
    assert_eq!(
        public_ids(&after.items),
        ["msg_old", "msg_kept", "msg_answer", "msg_next"]
    );
    assert_eq!(after.version.last_sequence, Some(3));

    // Deleting a converted ancestor removes its history from converted and new descendants.
    responses.delete("resp_first").await.unwrap();
    assert_eq!(
        public_ids(&responses.rehydrate("resp_second").await.unwrap()),
        ["msg_c"]
    );
    assert_eq!(
        public_ids(&responses.rehydrate("resp_third").await.unwrap()),
        ["msg_c", "msg_d"]
    );
    pool.close().await;

    // Starting again finds nothing left to convert.
    let reopened = create_pool_with_schema(Some(database_url))
        .await
        .expect("reopen upgraded database");
    assert_eq!(
        public_ids(
            &ResponseStore::new(reopened.clone())
                .rehydrate("resp_third")
                .await
                .unwrap()
        ),
        ["msg_c", "msg_d"]
    );
    reopened.close().await;
}

fn sqlite_url() -> String {
    let path = std::env::temp_dir().join(format!("chain_history_{}.db", uuid::Uuid::now_v7().simple()));
    format!("sqlite://{}?mode=rwc", path.display())
}

#[tokio::test]
async fn sqlite_upgrade_preserves_history() {
    assert_upgrade_preserves_history(&sqlite_url()).await;
}

#[tokio::test]
async fn malformed_history_list_stops_the_upgrade_and_names_the_response() {
    let url = sqlite_url();
    let legacy = create_pool(Some(&url)).await.expect("open database");
    migrate_to_0007(&legacy).await;
    insert_response(&legacy, "resp_corrupt", None, None, "not JSON").await;
    legacy.close().await;

    let error = create_pool_with_schema(Some(&url)).await.unwrap_err();
    assert!(error.to_string().contains("resp_corrupt"), "{error}");
}

#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL pointing to a PostgreSQL server where the test may create databases"]
async fn postgres_upgrade_preserves_history() {
    let server = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
    let name = format!("chain_history_{}", uuid::Uuid::now_v7().simple());
    let admin = create_pool(Some(&server)).await.expect("connect to PostgreSQL");
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(admin.as_ref())
        .await
        .expect("create isolated database");
    let (base, _) = server.rsplit_once('/').expect("database URL has a path");
    assert_upgrade_preserves_history(&format!("{base}/{name}")).await;
    sqlx::query(&format!("DROP DATABASE {name}"))
        .execute(admin.as_ref())
        .await
        .expect("drop isolated database");
}
