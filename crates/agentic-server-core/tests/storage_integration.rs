mod support;

use agentic_core::config::SqliteConfig;
use agentic_core::storage::ResponseMetadata;
use agentic_core::storage::{
    ConversationStore, ResponseStore, create_pool_with_schema, create_pool_with_schema_and_sqlite_config,
};
use agentic_core::storage::{ConversationVersion, InOutItem, StorageError};
use agentic_core::types::event::MessageStatus;
use agentic_core::types::io::{InputItem, InputMessage, InputMessageContent, OutputItem, OutputMessage};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Barrier;

use support::setup_pool;

fn create_input_item(text: &str) -> InOutItem {
    InOutItem::Input(InputItem::Message(InputMessage {
        id: None,
        role: "user".to_string(),
        status: None,
        content: InputMessageContent::Text(text.to_string()),
    }))
}

fn create_output_item(id: &str) -> InOutItem {
    InOutItem::Output(OutputItem::Message(OutputMessage::new(id, MessageStatus::Completed)))
}

#[tokio::test]
async fn test_conversation_store_create_and_get() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let created = store.create().await.expect("create failed");
    assert!(created.conversation_id.starts_with("conv_"));

    let retrieved = store.get(&created.conversation_id).await.expect("get failed");

    assert_eq!(retrieved.conversation_id, created.conversation_id);
}

#[tokio::test]
async fn tenant_scoped_conversations_cannot_be_read_or_mutated_by_another_tenant() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);
    let conversation = store
        .create_with_items_for_tenant(Some("tenant_a"), None, vec![create_input_item("private")])
        .await
        .expect("create tenant conversation");

    assert!(
        store
            .get_for_tenant(&conversation.conversation_id, Some("tenant_b"))
            .await
            .is_err()
    );
    assert!(
        store
            .append_items_for_tenant(
                &conversation.conversation_id,
                Some("tenant_b"),
                vec![create_input_item("not allowed")],
            )
            .await
            .is_err()
    );

    let page = store
        .list_items_for_tenant(&conversation.conversation_id, Some("tenant_a"), None, 20, false)
        .await
        .expect("list tenant conversation items");
    assert_eq!(page.data.len(), 1);
}

#[tokio::test]
async fn conversation_item_crud_and_pagination_are_ordered_and_scoped() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);
    let conversation = store
        .create_with_items_for_tenant(
            Some("tenant_crud"),
            Some(&json!({"project": "storage"})),
            vec![create_input_item("first"), create_input_item("second")],
        )
        .await?;

    assert_eq!(conversation.metadata.as_deref(), Some(r#"{"project":"storage"}"#));
    let updated = store
        .update_metadata_for_tenant(
            &conversation.conversation_id,
            Some("tenant_crud"),
            Some(&json!({"project": "updated"})),
        )
        .await?;
    assert_eq!(updated.metadata.as_deref(), Some(r#"{"project":"updated"}"#));

    let appended = store
        .append_items_for_tenant(
            &conversation.conversation_id,
            Some("tenant_crud"),
            vec![create_input_item("third")],
        )
        .await?;
    assert_eq!(appended.len(), 1);

    let ascending = store
        .list_items_for_tenant(&conversation.conversation_id, Some("tenant_crud"), None, 1, false)
        .await?;
    assert_eq!(ascending.data.len(), 1);
    assert_eq!(ascending.data[0].sequence, 0);
    assert!(ascending.has_more);

    let after_first = store
        .list_items_for_tenant(
            &conversation.conversation_id,
            Some("tenant_crud"),
            Some(&ascending.data[0].id),
            1,
            false,
        )
        .await?;
    assert_eq!(after_first.data[0].sequence, 1);
    assert!(after_first.has_more);

    let descending = store
        .list_items_for_tenant(&conversation.conversation_id, Some("tenant_crud"), None, 1, true)
        .await?;
    assert_eq!(descending.data[0].sequence, 2);
    assert!(descending.has_more);

    let retrieved = store
        .get_item_for_tenant(&conversation.conversation_id, &appended[0].id, Some("tenant_crud"))
        .await?;
    assert_eq!(retrieved.id, appended[0].id);
    assert_eq!(retrieved.item["content"], "third");

    store
        .delete_item_for_tenant(&conversation.conversation_id, &appended[0].id, Some("tenant_crud"))
        .await?;
    assert!(
        store
            .get_item_for_tenant(&conversation.conversation_id, &appended[0].id, Some("tenant_crud"))
            .await
            .is_err()
    );
    assert!(
        store
            .get_item_for_tenant(&conversation.conversation_id, "item_missing", Some("tenant_crud"))
            .await
            .is_err()
    );

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn tenant_isolation_covers_conversation_item_and_response_paths() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = store
        .create_with_items_for_tenant(
            Some("tenant_a"),
            Some(&json!({"owner": "a"})),
            vec![create_input_item("private")],
        )
        .await?;
    let item = store
        .list_items_for_tenant(&conversation.conversation_id, Some("tenant_a"), None, 20, false)
        .await?
        .data
        .into_iter()
        .next()
        .expect("tenant item");

    assert!(
        store
            .get_or_create_for_tenant(&conversation.conversation_id, Some("tenant_b"))
            .await
            .is_err()
    );
    assert!(
        store
            .get_for_tenant(&conversation.conversation_id, Some("tenant_b"))
            .await
            .is_err()
    );
    assert!(
        store
            .update_metadata_for_tenant(
                &conversation.conversation_id,
                Some("tenant_b"),
                Some(&json!({"owner": "b"}))
            )
            .await
            .is_err()
    );
    assert!(
        store
            .delete_for_tenant(&conversation.conversation_id, Some("tenant_b"))
            .await
            .is_err()
    );
    assert!(
        store
            .append_items_for_tenant(
                &conversation.conversation_id,
                Some("tenant_b"),
                vec![create_input_item("denied")]
            )
            .await
            .is_err()
    );
    assert!(
        store
            .list_items_for_tenant(&conversation.conversation_id, Some("tenant_b"), None, 20, false)
            .await
            .is_err()
    );
    assert!(
        store
            .get_item_for_tenant(&conversation.conversation_id, &item.id, Some("tenant_b"))
            .await
            .is_err()
    );
    assert!(
        store
            .delete_item_for_tenant(&conversation.conversation_id, &item.id, Some("tenant_b"))
            .await
            .is_err()
    );
    assert!(
        store
            .rehydrate_snapshot_for_tenant(&conversation.conversation_id, Some("tenant_b"))
            .await
            .is_err()
    );
    assert!(
        store
            .persist_if_version_for_tenant(
                &conversation.conversation_id,
                Some("tenant_b"),
                ConversationVersion::LastSequence(0),
                "resp_tenant_b",
                None,
                vec![create_input_item("denied")],
                &ResponseMetadata::default(),
            )
            .await
            .is_err()
    );

    store
        .persist_if_version_for_tenant(
            &conversation.conversation_id,
            Some("tenant_a"),
            ConversationVersion::LastSequence(0),
            "resp_tenant_a",
            None,
            vec![create_input_item("allowed")],
            &ResponseMetadata::default(),
        )
        .await?;
    let response_store = ResponseStore::new(pool);
    assert!(
        response_store
            .get_for_tenant("resp_tenant_a", Some("tenant_b"))
            .await
            .is_err()
    );
    assert!(
        response_store
            .rehydrate_for_tenant("resp_tenant_a", Some("tenant_b"))
            .await
            .is_err()
    );
    assert_eq!(
        store
            .get_for_tenant(&conversation.conversation_id, Some("tenant_a"))
            .await?
            .metadata
            .as_deref(),
        Some(r#"{"owner":"a"}"#)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_concurrent_item_appends_allocate_contiguous_sequences() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = std::env::temp_dir().join(format!("append_{}.db", uuid::Uuid::now_v7()));
    let db_url = format!("sqlite://{}", db_path.display());
    let pool = create_pool_with_schema_and_sqlite_config(
        Some(&db_url),
        SqliteConfig {
            max_connections: 8,
            ..SqliteConfig::default()
        },
    )
    .await?;
    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = store.create().await?;
    let writer_count = 16_usize;
    let barrier = Arc::new(Barrier::new(writer_count));
    let mut tasks = Vec::with_capacity(writer_count);
    for index in 0..writer_count {
        let store = store.clone();
        let conversation_id = conversation.conversation_id.clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .append_items_for_tenant(
                    &conversation_id,
                    None,
                    vec![create_input_item(&format!("item {index}"))],
                )
                .await
        }));
    }
    for task in tasks {
        task.await??;
    }
    let page = store
        .list_items_for_tenant(&conversation.conversation_id, None, None, 100, false)
        .await?;
    assert_eq!(page.data.len(), writer_count);
    assert_eq!(
        page.data.iter().map(|item| item.sequence).collect::<Vec<_>>(),
        (0..i64::try_from(writer_count)?).collect::<Vec<_>>()
    );
    pool.close().await;
    let _ = std::fs::remove_file(db_path);
    Ok(())
}

#[tokio::test]
async fn deleting_a_conversation_preserves_its_items() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = store
        .create_with_items_for_tenant(Some("tenant_a"), None, vec![create_input_item("preserved")])
        .await?;

    let item_count_before = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items WHERE tenant_id = $1")
        .bind("tenant_a")
        .fetch_one(pool.as_ref())
        .await?;
    assert_eq!(item_count_before, 1);

    store
        .delete_for_tenant(&conversation.conversation_id, Some("tenant_a"))
        .await?;

    let item = sqlx::query_as::<_, (Option<String>, Option<i64>)>(
        "SELECT conversation_id, seq FROM items WHERE tenant_id = $1",
    )
    .bind("tenant_a")
    .fetch_one(pool.as_ref())
    .await?;
    assert_eq!(item, (None, None));
    Ok(())
}

#[tokio::test]
async fn test_conversation_store_persist_and_rehydrate() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let conversation = store.create().await.expect("create failed");
    let conv_id = &conversation.conversation_id;

    let items = vec![create_input_item("hello"), create_output_item("msg_1")];

    let metadata = ResponseMetadata::default();

    store
        .persist(conv_id, "resp_1", None, items, &metadata)
        .await
        .expect("persist failed");

    let rehydrated = store.rehydrate(conv_id).await.expect("rehydrate failed");

    assert_eq!(rehydrated.len(), 2);
}

#[tokio::test]
async fn conversation_snapshot_reports_empty_and_last_sequence() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);
    let conversation = store.create().await?;

    let snapshot = store.rehydrate_snapshot(&conversation.conversation_id).await?;
    assert!(snapshot.items.is_empty());
    assert_eq!(snapshot.version, ConversationVersion::Empty);

    store
        .persist(
            &conversation.conversation_id,
            "resp_1",
            None,
            vec![create_input_item("hello"), create_output_item("msg_1")],
            &ResponseMetadata::default(),
        )
        .await?;

    let snapshot = store.rehydrate_snapshot(&conversation.conversation_id).await?;
    assert_eq!(snapshot.items.len(), 2);
    assert_eq!(snapshot.version, ConversationVersion::LastSequence(1));
    assert_eq!(store.rehydrate(&conversation.conversation_id).await?, snapshot.items);

    Ok(())
}

#[tokio::test]
async fn conversation_snapshot_version_includes_an_undecodable_final_row() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = store.create().await?;
    let stored_item = create_input_item("hello");

    store
        .persist(
            &conversation.conversation_id,
            "resp_1",
            None,
            vec![stored_item.clone()],
            &ResponseMetadata::default(),
        )
        .await?;
    sqlx::query("INSERT INTO items (id, data, created_at, conversation_id, seq) VALUES ($1, $2, $3, $4, $5)")
        .bind("item_undecodable")
        .bind("not valid JSON")
        .bind(0_i64)
        .bind(&conversation.conversation_id)
        .bind(1_i64)
        .execute(pool.as_ref())
        .await?;

    let snapshot = store.rehydrate_snapshot(&conversation.conversation_id).await?;

    assert_eq!(snapshot.items, vec![stored_item]);
    assert_eq!(snapshot.version, ConversationVersion::LastSequence(1));

    Ok(())
}

#[tokio::test]
async fn conversation_snapshot_rejects_items_without_a_sequence() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = store.create().await?;

    store
        .persist(
            &conversation.conversation_id,
            "resp_1",
            None,
            vec![create_input_item("hello")],
            &ResponseMetadata::default(),
        )
        .await?;

    let item_id: String = sqlx::query_scalar("SELECT id FROM items WHERE conversation_id = $1")
        .bind(&conversation.conversation_id)
        .fetch_one(pool.as_ref())
        .await?;
    sqlx::query("UPDATE items SET seq = NULL WHERE id = $1")
        .bind(&item_id)
        .execute(pool.as_ref())
        .await?;

    let error = store
        .rehydrate_snapshot(&conversation.conversation_id)
        .await
        .expect_err("snapshot must reject an item without a sequence");
    assert!(matches!(
        error,
        StorageError::InvalidConversationSequence {
            conversation_id,
            item_id: invalid_item_id,
        } if conversation_id == conversation.conversation_id && invalid_item_id == item_id
    ));

    Ok(())
}

#[tokio::test]
async fn conversation_version_empty_checked_persist_succeeds() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);
    let conversation = store.create().await?;
    let items = vec![create_input_item("first input"), create_output_item("msg_first")];

    store
        .persist_if_version(
            &conversation.conversation_id,
            ConversationVersion::Empty,
            "resp_first",
            None,
            items.clone(),
            &ResponseMetadata::default(),
        )
        .await?;

    let snapshot = store.rehydrate_snapshot(&conversation.conversation_id).await?;
    assert_eq!(snapshot.items, items);
    assert_eq!(snapshot.version, ConversationVersion::LastSequence(1));

    Ok(())
}

#[tokio::test]
async fn conversation_version_stale_checked_persist_rolls_back_items_and_response()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let response_store = ResponseStore::new(Arc::clone(&pool));
    let conversation = store.create().await?;
    let snapshot = store.rehydrate_snapshot(&conversation.conversation_id).await?;
    let competing_items = vec![
        create_input_item("competing input"),
        create_output_item("msg_competing"),
    ];
    store
        .persist(
            &conversation.conversation_id,
            "resp_competing",
            None,
            competing_items.clone(),
            &ResponseMetadata::default(),
        )
        .await?;
    let rejected_items = vec![create_input_item("stale input"), create_output_item("msg_stale")];

    let error = store
        .persist_if_version(
            &conversation.conversation_id,
            snapshot.version,
            "resp_stale",
            None,
            rejected_items,
            &ResponseMetadata::default(),
        )
        .await
        .expect_err("a stale conversation version must be rejected");

    assert!(error.is_conversation_conflict());
    assert!(matches!(
        error,
        StorageError::ConversationConflict { conversation_id }
            if conversation_id == conversation.conversation_id
    ));
    assert_eq!(store.rehydrate(&conversation.conversation_id).await?, competing_items);
    let response_error = response_store
        .get("resp_stale")
        .await
        .expect_err("the rejected response must not be stored");
    assert!(response_error.is_not_found());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversation_version_racing_checked_persists_allow_exactly_one_winner()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = store.create().await?;
    let version = store.rehydrate_snapshot(&conversation.conversation_id).await?.version;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let writer_one = {
        let store = ConversationStore::new(Arc::clone(&pool));
        let conversation_id = conversation.conversation_id.clone();
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            let items = vec![create_input_item("writer one"), create_output_item("msg_writer_one")];
            barrier.wait().await;
            let result = store
                .persist_if_version(
                    &conversation_id,
                    version,
                    "resp_writer_one",
                    None,
                    items.clone(),
                    &ResponseMetadata::default(),
                )
                .await;
            (result, items)
        })
    };
    let writer_two = {
        let store = ConversationStore::new(pool);
        let conversation_id = conversation.conversation_id.clone();
        tokio::spawn(async move {
            let items = vec![create_input_item("writer two"), create_output_item("msg_writer_two")];
            barrier.wait().await;
            let result = store
                .persist_if_version(
                    &conversation_id,
                    version,
                    "resp_writer_two",
                    None,
                    items.clone(),
                    &ResponseMetadata::default(),
                )
                .await;
            (result, items)
        })
    };

    let (writer_one_result, writer_one_items) = writer_one.await?;
    let (writer_two_result, writer_two_items) = writer_two.await?;

    assert_eq!(
        usize::from(writer_one_result.is_ok()) + usize::from(writer_two_result.is_ok()),
        1
    );
    assert_eq!(
        usize::from(
            writer_one_result
                .as_ref()
                .is_err_and(StorageError::is_conversation_conflict)
        ) + usize::from(
            writer_two_result
                .as_ref()
                .is_err_and(StorageError::is_conversation_conflict)
        ),
        1
    );
    let winner_items = if writer_one_result.is_ok() {
        writer_one_items
    } else {
        writer_two_items
    };
    assert_eq!(store.rehydrate(&conversation.conversation_id).await?, winner_items);

    Ok(())
}

#[tokio::test]
async fn conversation_version_is_scoped_per_conversation() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);
    let first = store.create().await?;
    let second = store.create().await?;
    let first_items = vec![create_input_item("first conversation")];
    store
        .persist(
            &first.conversation_id,
            "resp_first_conversation",
            None,
            first_items.clone(),
            &ResponseMetadata::default(),
        )
        .await?;
    let first_snapshot = store.rehydrate_snapshot(&first.conversation_id).await?;

    store
        .persist_if_version(
            &second.conversation_id,
            ConversationVersion::Empty,
            "resp_second_conversation",
            None,
            vec![create_input_item("second conversation")],
            &ResponseMetadata::default(),
        )
        .await?;

    let first_after = store.rehydrate_snapshot(&first.conversation_id).await?;
    assert_eq!(first_after.items, first_items);
    assert_eq!(first_after.version, first_snapshot.version);

    Ok(())
}

#[tokio::test]
async fn test_conversation_store_multiple_turns() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let conversation = store.create().await.expect("create failed");
    let conv_id = &conversation.conversation_id;

    let metadata = ResponseMetadata::default();

    // First turn
    store
        .persist(conv_id, "resp_1", None, vec![create_input_item("turn 1")], &metadata)
        .await
        .expect("first persist failed");

    // Second turn
    store
        .persist(
            conv_id,
            "resp_2",
            Some("resp_1"),
            vec![create_input_item("turn 2")],
            &metadata,
        )
        .await
        .expect("second persist failed");

    let rehydrated = store.rehydrate(conv_id).await.expect("rehydrate failed");

    assert_eq!(rehydrated.len(), 2);
}

#[tokio::test]
async fn test_response_store_persist_and_rehydrate() {
    let pool = setup_pool().await;
    let store = ResponseStore::new(pool);

    let items = vec![create_input_item("query"), create_output_item("out_1")];

    let metadata = ResponseMetadata::default();

    store
        .persist("resp_1", None, items, &metadata)
        .await
        .expect("persist failed");

    let rehydrated = store.rehydrate("resp_1").await.expect("rehydrate failed");

    assert_eq!(rehydrated.len(), 2);
}

#[tokio::test]
async fn test_response_store_get() {
    let pool = setup_pool().await;
    let store = ResponseStore::new(pool);

    let items = vec![create_input_item("test")];
    let metadata = ResponseMetadata::default();

    store
        .persist("resp_get_test", None, items, &metadata)
        .await
        .expect("persist failed");

    let response = store.get("resp_get_test").await.expect("get failed");

    assert_eq!(response.response_id, "resp_get_test");
    assert_eq!(response.history_item_ids.len(), 1);
}

#[tokio::test]
async fn test_response_store_with_previous_response() {
    let pool = setup_pool().await;
    let store = ResponseStore::new(pool);

    let metadata = ResponseMetadata::default();

    store
        .persist("resp_1", None, vec![create_input_item("first")], &metadata)
        .await
        .expect("persist first failed");

    store
        .persist("resp_2", Some("resp_1"), vec![create_output_item("out_2")], &metadata)
        .await
        .expect("persist second failed");

    let response = store.get("resp_2").await.expect("get failed");

    assert_eq!(response.previous_response_id, Some("resp_1".to_string()));
    assert_eq!(response.history_item_ids.len(), 2);

    let rehydrated = store.rehydrate("resp_2").await.expect("rehydrate failed");
    assert_eq!(rehydrated.len(), 2);
}

#[tokio::test]
async fn response_store_allocates_unique_ids_for_repeated_protocol_items() {
    let pool = setup_pool().await;
    let store = ResponseStore::new(pool);
    let metadata = ResponseMetadata::default();
    let repeated_item = create_output_item("fc_search");

    store
        .persist(
            "resp_repeated_protocol_ids",
            None,
            vec![repeated_item.clone(), repeated_item.clone(), repeated_item],
            &metadata,
        )
        .await
        .expect("repeated protocol IDs must not collide in storage");

    let response = store
        .get("resp_repeated_protocol_ids")
        .await
        .expect("response should be stored");
    let unique_ids: std::collections::HashSet<&String> = response.history_item_ids.iter().collect();
    assert_eq!(response.history_item_ids.len(), 3);
    assert_eq!(unique_ids.len(), 3);

    let rehydrated = store
        .rehydrate("resp_repeated_protocol_ids")
        .await
        .expect("response should rehydrate");
    assert_eq!(rehydrated.len(), 3);
    assert!(rehydrated.iter().all(|item| item == &create_output_item("fc_search")));
}

// Edge case tests

#[tokio::test]
async fn test_conversation_persist_empty_items() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let conversation = store.create().await.expect("create failed");
    let conv_id = &conversation.conversation_id;

    let metadata = ResponseMetadata::default();

    // Persist with empty item list
    store
        .persist(conv_id, "resp_empty", None, vec![], &metadata)
        .await
        .expect("persist empty items failed");

    let rehydrated = store.rehydrate(conv_id).await.expect("rehydrate failed");

    assert!(rehydrated.is_empty());
}

#[tokio::test]
async fn test_conversation_rehydrate_after_multiple_varying_turns() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let conversation = store.create().await.expect("create failed");
    let conv_id = &conversation.conversation_id;

    let metadata = ResponseMetadata::default();

    // Turn 1: 1 item
    store
        .persist(conv_id, "resp_1", None, vec![create_input_item("turn1")], &metadata)
        .await
        .expect("turn 1 failed");

    // Turn 2: 3 items
    store
        .persist(
            conv_id,
            "resp_2",
            Some("resp_1"),
            vec![
                create_input_item("turn2a"),
                create_output_item("out2"),
                create_input_item("turn2b"),
            ],
            &metadata,
        )
        .await
        .expect("turn 2 failed");

    // Turn 3: 2 items
    store
        .persist(
            conv_id,
            "resp_3",
            Some("resp_2"),
            vec![create_input_item("turn3"), create_output_item("out3")],
            &metadata,
        )
        .await
        .expect("turn 3 failed");

    let rehydrated = store.rehydrate(conv_id).await.expect("rehydrate failed");

    assert_eq!(rehydrated.len(), 6);
}

#[tokio::test]
async fn test_response_store_chaining_respects_foreign_key() {
    let pool = setup_pool().await;
    let store = ResponseStore::new(pool);

    let metadata = ResponseMetadata::default();

    // Create resp_1
    store
        .persist("resp_1", None, vec![create_input_item("first")], &metadata)
        .await
        .expect("resp_1 persist failed");

    // Try to create resp_3 with resp_2 as previous (resp_2 doesn't exist)
    // This should fail due to foreign key constraint
    let result = store
        .persist("resp_3", Some("resp_2"), vec![create_output_item("out3")], &metadata)
        .await;

    assert!(
        result.is_err(),
        "expected error when previous_response_id references non-existent response"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_conversation_concurrent_turns() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool.clone());

    let conversation = store.create().await.expect("create failed");
    let conv_id = conversation.conversation_id.clone();

    let metadata_1 = Arc::new(ResponseMetadata::default());
    let metadata_2 = metadata_1.clone();

    // Spawn two concurrent persist operations
    let conv_id_1 = conv_id.clone();
    let store_1 = ConversationStore::new(pool.clone());
    let handle1 = tokio::spawn(async move {
        store_1
            .persist(
                &conv_id_1,
                "resp_t1",
                None,
                vec![create_input_item("thread1")],
                metadata_1.as_ref(),
            )
            .await
    });

    let conv_id_2 = conv_id.clone();
    let store_2 = ConversationStore::new(pool);
    let handle2 = tokio::spawn(async move {
        store_2
            .persist(
                &conv_id_2,
                "resp_t2",
                None,
                vec![create_input_item("thread2")],
                metadata_2.as_ref(),
            )
            .await
    });

    let result1 = handle1.await;
    let result2 = handle2.await;

    assert!(result1.is_ok() && result1.unwrap().is_ok());
    assert!(result2.is_ok() && result2.unwrap().is_ok());

    let rehydrated = store.rehydrate(&conv_id).await.expect("rehydrate failed");
    assert_eq!(rehydrated.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_sqlite_multi_pool_mixed_read_write_concurrency() {
    let db_path = std::env::temp_dir().join(format!("mixed_rw_{}.db", uuid::Uuid::now_v7()));
    let db_url = format!("sqlite://{}", db_path.display());

    let writer_pool_a = create_pool_with_schema(Some(&db_url))
        .await
        .expect("failed to create writer pool a");
    let writer_pool_b = create_pool_with_schema(Some(&db_url))
        .await
        .expect("failed to create writer pool b");
    let reader_pool = create_pool_with_schema(Some(&db_url))
        .await
        .expect("failed to create reader pool");

    let writer_store_a = ConversationStore::new(Arc::clone(&writer_pool_a));
    let writer_store_b = ConversationStore::new(writer_pool_b);
    let reader_store = ConversationStore::new(reader_pool);
    let conversation = writer_store_a.create().await.expect("create conversation failed");
    let conv_id = conversation.conversation_id;
    let metadata = Arc::new(ResponseMetadata::default());
    let barrier = Arc::new(tokio::sync::Barrier::new(10));

    let spawn_writer = |writer_idx: usize, writer_store: ConversationStore| {
        let writer_conv_id = conv_id.clone();
        let writer_metadata = Arc::clone(&metadata);
        let writer_barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            writer_barrier.wait().await;
            for idx in 0..50 {
                writer_store
                    .persist(
                        &writer_conv_id,
                        &format!("resp_lock_writer_{writer_idx}_{idx}"),
                        None,
                        vec![create_input_item(&format!("writer {writer_idx} item {idx}"))],
                        writer_metadata.as_ref(),
                    )
                    .await
                    .map_err(|err| format!("writer {writer_idx} write {idx} failed: {err:?}"))?;
                tokio::task::yield_now().await;
            }
            Ok::<(), String>(())
        })
    };
    let writers = vec![spawn_writer(0, writer_store_a.clone()), spawn_writer(1, writer_store_b)];

    let mut readers = Vec::new();
    for reader_idx in 0..8 {
        let reader_store = reader_store.clone();
        let reader_conv_id = conv_id.clone();
        let reader_barrier = Arc::clone(&barrier);
        readers.push(tokio::spawn(async move {
            reader_barrier.wait().await;
            for iter in 0..100 {
                reader_store
                    .rehydrate(&reader_conv_id)
                    .await
                    .map_err(|err| format!("reader {reader_idx} iteration {iter} failed: {err:?}"))?;
                tokio::task::yield_now().await;
            }
            Ok::<(), String>(())
        }));
    }

    for writer in writers {
        writer.await.expect("writer task panicked").expect("writer task failed");
    }
    for reader in readers {
        reader.await.expect("reader task panicked").expect("reader task failed");
    }

    let final_items = ConversationStore::new(Arc::clone(&writer_pool_a))
        .rehydrate(&conv_id)
        .await
        .expect("final rehydrate failed");
    assert_eq!(final_items.len(), 100);

    let seqs: Vec<i64> = sqlx::query_scalar("SELECT seq FROM items WHERE conversation_id = ? ORDER BY seq ASC")
        .bind(&conv_id)
        .fetch_all(writer_pool_a.as_ref())
        .await
        .expect("sequence query failed");
    assert_eq!(seqs, (0..100).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_sqlite_same_pool_mixed_read_write_concurrency() {
    let db_path = std::env::temp_dir().join(format!("same_pool_mixed_rw_{}.db", uuid::Uuid::now_v7()));
    let db_url = format!("sqlite://{}", db_path.display());
    let sqlite_config = SqliteConfig {
        max_connections: 4,
        ..SqliteConfig::default()
    };
    let pool = create_pool_with_schema_and_sqlite_config(Some(&db_url), sqlite_config)
        .await
        .expect("failed to create pool");
    assert_eq!(pool.options().get_max_connections(), 4);

    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = store.create().await.expect("create conversation failed");
    let conv_id = conversation.conversation_id;
    let metadata = Arc::new(ResponseMetadata::default());
    let barrier = Arc::new(tokio::sync::Barrier::new(10));

    let spawn_writer = |writer_idx: usize| {
        let writer_store = store.clone();
        let writer_conv_id = conv_id.clone();
        let writer_metadata = Arc::clone(&metadata);
        let writer_barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            writer_barrier.wait().await;
            for idx in 0..50 {
                writer_store
                    .persist(
                        &writer_conv_id,
                        &format!("resp_same_pool_writer_{writer_idx}_{idx}"),
                        None,
                        vec![create_input_item(&format!("same pool writer {writer_idx} item {idx}"))],
                        writer_metadata.as_ref(),
                    )
                    .await
                    .map_err(|err| format!("writer {writer_idx} write {idx} failed: {err:?}"))?;
                tokio::task::yield_now().await;
            }
            Ok::<(), String>(())
        })
    };
    let writers = vec![spawn_writer(0), spawn_writer(1)];

    let mut readers = Vec::new();
    for reader_idx in 0..8 {
        let reader_store = store.clone();
        let reader_conv_id = conv_id.clone();
        let reader_barrier = Arc::clone(&barrier);
        readers.push(tokio::spawn(async move {
            reader_barrier.wait().await;
            for iter in 0..100 {
                reader_store
                    .rehydrate(&reader_conv_id)
                    .await
                    .map_err(|err| format!("reader {reader_idx} iteration {iter} failed: {err:?}"))?;
                tokio::task::yield_now().await;
            }
            Ok::<(), String>(())
        }));
    }

    for writer in writers {
        writer.await.expect("writer task panicked").expect("writer task failed");
    }
    for reader in readers {
        reader.await.expect("reader task panicked").expect("reader task failed");
    }

    let final_items = store.rehydrate(&conv_id).await.expect("final rehydrate failed");
    assert_eq!(final_items.len(), 100);

    let seqs: Vec<i64> = sqlx::query_scalar("SELECT seq FROM items WHERE conversation_id = ? ORDER BY seq ASC")
        .bind(&conv_id)
        .fetch_all(pool.as_ref())
        .await
        .expect("sequence query failed");
    assert_eq!(seqs, (0..100).collect::<Vec<_>>());
}

// Store-level error handling edge cases

#[tokio::test]
async fn test_conversation_store_get_nonexistent() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let result = store.get("nonexistent_conv").await;
    assert!(result.is_err(), "expected error for non-existent conversation");

    // Verify it's a not found error
    let err = result.unwrap_err();
    assert!(err.is_not_found());
}

#[tokio::test]
async fn test_conversation_store_persist_nonexistent_conversation() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let metadata = ResponseMetadata::default();

    // Try to persist to a non-existent conversation
    let result = store
        .persist(
            "nonexistent_conv",
            "resp_1",
            None,
            vec![create_input_item("test")],
            &metadata,
        )
        .await;

    let error = result.expect_err("persisting to a non-existent conversation should fail");
    assert!(error.is_not_found(), "expected not-found error, got {error}");
}

#[tokio::test]
async fn test_response_store_rehydrate_nonexistent() {
    let pool = setup_pool().await;
    let store = ResponseStore::new(pool);

    let result = store.rehydrate("nonexistent_resp").await;
    assert!(result.is_err(), "expected error for non-existent response");
}

#[tokio::test]
async fn test_conversation_store_disabled() {
    let store = ConversationStore::disabled();

    let result = store.create().await;
    assert!(result.is_err(), "expected error from disabled store");

    let err = result.unwrap_err();
    assert!(err.is_not_configured());
}

#[tokio::test]
async fn test_response_store_disabled() {
    let store = ResponseStore::disabled();

    let metadata = ResponseMetadata::default();
    let result = store
        .persist("resp_1", None, vec![create_input_item("test")], &metadata)
        .await;

    assert!(result.is_err(), "expected error from disabled store");

    let err = result.unwrap_err();
    assert!(err.is_not_configured());
}

#[tokio::test]
async fn test_conversation_store_get_after_create() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let created = store.create().await.expect("create failed");

    // Immediately try to get it
    let retrieved = store.get(&created.conversation_id).await.expect("get should succeed");

    assert_eq!(retrieved.conversation_id, created.conversation_id);
    assert_eq!(retrieved.created_at, created.created_at);
}

#[tokio::test]
async fn test_response_store_get_after_persist() {
    let pool = setup_pool().await;
    let store = ResponseStore::new(pool);

    let items = vec![create_input_item("query"), create_output_item("out_1")];
    let metadata = ResponseMetadata::default();

    store
        .persist("resp_stored", None, items.clone(), &metadata)
        .await
        .expect("persist failed");

    let retrieved = store.get("resp_stored").await.expect("response should be found");

    assert_eq!(retrieved.response_id, "resp_stored");
    assert_eq!(retrieved.history_item_ids.len(), 2);
}

#[tokio::test]
async fn test_conversation_get_or_create_same_id() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let conv_id = "test_conv_idempotent";

    let first = store.get_or_create(conv_id).await.expect("first get_or_create failed");

    let second = store.get_or_create(conv_id).await.expect("second get_or_create failed");

    // Should return the same conversation
    assert_eq!(first.conversation_id, second.conversation_id);
    assert_eq!(first.created_at, second.created_at);
}
