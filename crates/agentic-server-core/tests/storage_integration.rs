mod support;

use agentic_core::config::SqliteConfig;
use agentic_core::storage::ResponseMetadata;
use agentic_core::storage::{
    ConversationData, ConversationStore, ResponseStore, StoreResult, create_pool_with_schema,
    create_pool_with_schema_and_sqlite_config,
};
use agentic_core::storage::{ConversationVersion, StorageError};
use agentic_core::types::conversations::ConversationMetadata;
use agentic_core::types::event::MessageStatus;
use agentic_core::types::io::InOutItem;
use agentic_core::types::io::{InputItem, InputMessage, InputMessageContent, OutputItem, OutputMessage};
use std::sync::Arc;

use support::persist_turn;
use support::setup_pool;

/// Create an empty conversation owned by the default tenant.
async fn create_conversation(store: &ConversationStore) -> StoreResult<ConversationData> {
    store.create("default_tenant", &ConversationMetadata::new(), &[]).await
}

fn create_input_item(text: &str) -> InOutItem {
    InOutItem::Input(InputItem::Message(InputMessage {
        role: "user".to_string(),
        content: InputMessageContent::Text(text.to_string()),
        ..Default::default()
    }))
}

fn create_output_item(id: &str) -> InOutItem {
    InOutItem::Output(OutputItem::Message(OutputMessage::new(id, MessageStatus::Completed)))
}

#[tokio::test]
async fn test_conversation_store_create_and_get() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let created = create_conversation(&store).await.expect("create failed");
    assert!(created.conversation_id.starts_with("conv_"));

    let retrieved = store.get(&created.conversation_id).await.expect("get failed");

    assert_eq!(retrieved.conversation_id, created.conversation_id);
}

#[tokio::test]
async fn test_conversation_store_persist_and_rehydrate() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let conversation = create_conversation(&store).await.expect("create failed");
    let conv_id = &conversation.conversation_id;

    let items = vec![create_input_item("hello"), create_output_item("msg_1")];

    let metadata = ResponseMetadata::default();

    persist_turn(&store, conv_id, "resp_1", items, &metadata)
        .await
        .expect("persist failed");

    let rehydrated = store.rehydrate(conv_id).await.expect("rehydrate failed");

    assert_eq!(rehydrated.items.len(), 2);
}

#[tokio::test]
async fn conversation_snapshot_reports_empty_and_last_response() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);
    let conversation = create_conversation(&store).await?;

    let snapshot = store.rehydrate(&conversation.conversation_id).await?;
    assert!(snapshot.items.is_empty());
    assert_eq!(snapshot.version, ConversationVersion::default());

    persist_turn(
        &store,
        &conversation.conversation_id,
        "resp_1",
        vec![create_input_item("hello"), create_output_item("msg_1")],
        &ResponseMetadata::default(),
    )
    .await?;

    let snapshot = store.rehydrate(&conversation.conversation_id).await?;
    assert_eq!(snapshot.items.len(), 2);
    assert_eq!(
        snapshot.version,
        ConversationVersion {
            response_id: Some("resp_1".to_owned()),
            revision: 1,
            last_sequence: Some(1),
        }
    );
    assert_eq!(
        store.rehydrate(&conversation.conversation_id).await?.items,
        snapshot.items
    );

    Ok(())
}

#[tokio::test]
async fn conversation_snapshot_rejects_an_undecodable_final_row() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = create_conversation(&store).await?;
    let stored_item = create_input_item("hello");

    persist_turn(
        &store,
        &conversation.conversation_id,
        "resp_1",
        vec![stored_item.clone()],
        &ResponseMetadata::default(),
    )
    .await?;
    sqlx::query(
        "INSERT INTO items (id, public_id, metadata, data, created_at, conversation_id, seq) \
         VALUES ($1, $1, $2, $3, $4, $5, $6)",
    )
    .bind("item_undecodable")
    .bind(r#"{"kind":"input"}"#)
    .bind("not valid JSON")
    .bind(0_i64)
    .bind(&conversation.conversation_id)
    .bind(1_i64)
    .execute(pool.as_ref())
    .await?;

    let error = store.rehydrate(&conversation.conversation_id).await.unwrap_err();
    assert!(matches!(error, StorageError::InvalidHistoryItem { ref item_id } if item_id == "item_undecodable"));
    assert!(!error.to_string().contains("not valid JSON"));

    Ok(())
}

#[tokio::test]
async fn conversation_snapshot_rejects_items_without_a_sequence() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = create_conversation(&store).await?;

    persist_turn(
        &store,
        &conversation.conversation_id,
        "resp_1",
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
        .rehydrate(&conversation.conversation_id)
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
async fn item_only_version_records_a_turn_on_the_next_persist() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = create_conversation(&store).await?;
    store
        .create_items(
            "default_tenant",
            &conversation.conversation_id,
            vec![create_input_item("item")],
        )
        .await?;

    let item_only = store.rehydrate(&conversation.conversation_id).await?;
    // Adding items advances the revision but records no turn.
    assert_eq!(
        item_only.version,
        ConversationVersion {
            last_sequence: Some(0),
            response_id: None,
            revision: 1,
        }
    );

    store
        .persist(
            &conversation.conversation_id,
            item_only.version,
            "resp_after_items",
            Vec::new(),
            None,
            &ResponseMetadata::default(),
        )
        .await?;
    let recorded = store.rehydrate(&conversation.conversation_id).await?;
    assert_eq!(
        recorded.version,
        ConversationVersion {
            response_id: Some("resp_after_items".to_owned()),
            revision: 2,
            last_sequence: Some(0),
        }
    );

    Ok(())
}

#[tokio::test]
async fn conversation_version_empty_checked_persist_succeeds() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);
    let conversation = create_conversation(&store).await?;
    let items = vec![create_input_item("first input"), create_output_item("msg_first")];

    store
        .persist(
            &conversation.conversation_id,
            ConversationVersion::default(),
            "resp_first",
            items.clone(),
            None,
            &ResponseMetadata::default(),
        )
        .await?;

    let snapshot = store.rehydrate(&conversation.conversation_id).await?;
    assert_eq!(snapshot.items, items);
    assert_eq!(
        snapshot.version,
        ConversationVersion {
            response_id: Some("resp_first".to_owned()),
            revision: 1,
            last_sequence: Some(1),
        }
    );

    Ok(())
}

#[tokio::test]
async fn zero_item_turn_advances_version_and_retains_exact_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let response_store = ResponseStore::new(pool);
    let conversation = create_conversation(&store).await?;
    let empty = store.rehydrate(&conversation.conversation_id).await?;
    let first_metadata = ResponseMetadata {
        multi_agent_tree: None,
        model: "first-zero-item-turn".to_owned(),
        ..ResponseMetadata::default()
    };

    store
        .persist(
            &conversation.conversation_id,
            empty.version,
            "resp_zero_items_first",
            Vec::new(),
            None,
            &first_metadata,
        )
        .await?;

    let first = store.rehydrate(&conversation.conversation_id).await?;
    assert!(first.items.is_empty());
    assert_eq!(
        first.version,
        ConversationVersion {
            response_id: Some("resp_zero_items_first".to_owned()),
            revision: 1,
            last_sequence: None,
        }
    );
    assert_eq!(
        first.turn_metadata.as_ref().map(|metadata| metadata.model.as_str()),
        Some("first-zero-item-turn")
    );

    store
        .persist(
            &conversation.conversation_id,
            first.version.clone(),
            "resp_zero_items_second",
            Vec::new(),
            None,
            &ResponseMetadata {
                multi_agent_tree: None,
                model: "second-zero-item-turn".to_owned(),
                ..ResponseMetadata::default()
            },
        )
        .await?;

    let second = store.rehydrate(&conversation.conversation_id).await?;
    assert!(second.items.is_empty());
    assert_ne!(second.version, first.version);
    assert_eq!(
        first.turn_metadata.as_ref().map(|metadata| metadata.model.as_str()),
        Some("first-zero-item-turn")
    );
    assert_eq!(
        second.turn_metadata.as_ref().map(|metadata| metadata.model.as_str()),
        Some("second-zero-item-turn")
    );

    let error = store
        .persist(
            &conversation.conversation_id,
            first.version,
            "resp_zero_items_stale",
            Vec::new(),
            None,
            &ResponseMetadata::default(),
        )
        .await
        .expect_err("an earlier zero-item turn must be stale");
    assert!(error.is_conversation_conflict());
    assert!(response_store.get("resp_zero_items_stale").await.is_err());

    Ok(())
}

#[tokio::test]
async fn conversation_version_stale_checked_persist_rolls_back_items_and_response()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let response_store = ResponseStore::new(Arc::clone(&pool));
    let conversation = create_conversation(&store).await?;
    let snapshot = store.rehydrate(&conversation.conversation_id).await?;
    let competing_items = vec![
        create_input_item("competing input"),
        create_output_item("msg_competing"),
    ];
    persist_turn(
        &store,
        &conversation.conversation_id,
        "resp_competing",
        competing_items.clone(),
        &ResponseMetadata::default(),
    )
    .await?;
    let rejected_items = vec![create_input_item("stale input"), create_output_item("msg_stale")];

    let error = store
        .persist(
            &conversation.conversation_id,
            snapshot.version,
            "resp_stale",
            rejected_items,
            None,
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
    assert_eq!(
        store.rehydrate(&conversation.conversation_id).await?.items,
        competing_items
    );
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
    let conversation = create_conversation(&store).await?;
    let version = store.rehydrate(&conversation.conversation_id).await?.version;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let writer_one = {
        let store = ConversationStore::new(Arc::clone(&pool));
        let conversation_id = conversation.conversation_id.clone();
        let barrier = Arc::clone(&barrier);
        let version = version.clone();
        tokio::spawn(async move {
            let items = vec![create_input_item("writer one"), create_output_item("msg_writer_one")];
            barrier.wait().await;
            let result = store
                .persist(
                    &conversation_id,
                    version,
                    "resp_writer_one",
                    items.clone(),
                    None,
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
                .persist(
                    &conversation_id,
                    version,
                    "resp_writer_two",
                    items.clone(),
                    None,
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
    assert_eq!(
        store.rehydrate(&conversation.conversation_id).await?.items,
        winner_items
    );

    Ok(())
}

#[tokio::test]
async fn conversation_version_is_scoped_per_conversation() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);
    let first = create_conversation(&store).await?;
    let second = create_conversation(&store).await?;
    let first_items = vec![create_input_item("first conversation")];
    persist_turn(
        &store,
        &first.conversation_id,
        "resp_first_conversation",
        first_items.clone(),
        &ResponseMetadata::default(),
    )
    .await?;
    let first_snapshot = store.rehydrate(&first.conversation_id).await?;

    store
        .persist(
            &second.conversation_id,
            ConversationVersion::default(),
            "resp_second_conversation",
            vec![create_input_item("second conversation")],
            None,
            &ResponseMetadata::default(),
        )
        .await?;

    let first_after = store.rehydrate(&first.conversation_id).await?;
    assert_eq!(first_after.items, first_items);
    assert_eq!(first_after.version, first_snapshot.version);

    Ok(())
}

#[tokio::test]
async fn test_conversation_store_multiple_turns() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let conversation = create_conversation(&store).await.expect("create failed");
    let conv_id = &conversation.conversation_id;

    let metadata = ResponseMetadata::default();

    // First turn
    persist_turn(&store, conv_id, "resp_1", vec![create_input_item("turn 1")], &metadata)
        .await
        .expect("first persist failed");

    // Second turn
    persist_turn(&store, conv_id, "resp_2", vec![create_input_item("turn 2")], &metadata)
        .await
        .expect("second persist failed");

    let rehydrated = store.rehydrate(conv_id).await.expect("rehydrate failed");

    assert_eq!(rehydrated.items.len(), 2);
}

#[tokio::test]
async fn test_response_store_persist_and_rehydrate() {
    let pool = setup_pool().await;
    let store = ResponseStore::new(pool);

    let items = vec![create_input_item("query"), create_output_item("out_1")];

    let metadata = ResponseMetadata::default();

    store
        .persist("resp_1", None, items, None, &metadata)
        .await
        .expect("persist failed");

    let rehydrated = store.rehydrate("resp_1").await.expect("rehydrate failed");

    assert_eq!(rehydrated.1.len(), 2);
}

#[tokio::test]
async fn test_response_store_get() {
    let pool = setup_pool().await;
    let store = ResponseStore::new(pool);

    let items = vec![create_input_item("test")];
    let metadata = ResponseMetadata::default();

    store
        .persist("resp_get_test", None, items, None, &metadata)
        .await
        .expect("persist failed");

    let response = store.get("resp_get_test").await.expect("get failed");

    assert_eq!(response.response_id, "resp_get_test");
    assert_eq!(response.history_item_ids.len(), 1);
}

#[tokio::test]
async fn response_rehydration_rejects_invalid_or_missing_history() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ResponseStore::new(Arc::clone(&pool));
    store
        .persist(
            "resp_corrupt",
            None,
            vec![create_input_item("query")],
            None,
            &ResponseMetadata::default(),
        )
        .await?;
    let item_id = store.get("resp_corrupt").await?.history_item_ids.remove(0);

    // Reasoning that even the legacy projection cannot read fails closed, not silently.
    let malformed = serde_json::json!({
        "type": "reasoning", "id": "rs_1", "content": [{"type": "reasoning_text", "text": 42}],
        "encrypted_content": {"ciphertext": "sensitive-state"}
    })
    .to_string();
    sqlx::query("UPDATE items SET data = $1 WHERE id = $2")
        .bind(malformed)
        .bind(&item_id)
        .execute(pool.as_ref())
        .await?;
    let error = store.rehydrate("resp_corrupt").await.unwrap_err();
    assert!(matches!(error, StorageError::InvalidHistoryItem { item_id: ref invalid } if invalid == &item_id));
    assert!(!error.to_string().contains("sensitive-state"));

    sqlx::query("DELETE FROM items WHERE id = $1")
        .bind(&item_id)
        .execute(pool.as_ref())
        .await?;
    let error = store.rehydrate("resp_corrupt").await.unwrap_err();
    assert!(matches!(error, StorageError::InvalidHistoryItem { item_id: missing } if missing == item_id));
    Ok(())
}

#[tokio::test]
async fn conversation_rehydration_rejects_invalid_reasoning() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = create_conversation(&store).await?;
    persist_turn(
        &store,
        &conversation.conversation_id,
        "resp_corrupt",
        vec![create_input_item("query")],
        &ResponseMetadata::default(),
    )
    .await?;
    let malformed = serde_json::json!({
        "type": "reasoning", "id": "rs_1", "summary": "sensitive-state"
    })
    .to_string();
    sqlx::query("UPDATE items SET data = $1 WHERE conversation_id = $2")
        .bind(malformed)
        .bind(&conversation.conversation_id)
        .execute(pool.as_ref())
        .await?;
    let error = store.rehydrate(&conversation.conversation_id).await.unwrap_err();
    assert!(matches!(error, StorageError::InvalidHistoryItem { .. }));
    assert!(!error.to_string().contains("sensitive-state"));
    Ok(())
}

#[tokio::test]
async fn test_response_store_with_previous_response() {
    let pool = setup_pool().await;
    let store = ResponseStore::new(pool);

    let metadata = ResponseMetadata::default();

    store
        .persist("resp_1", None, vec![create_input_item("first")], None, &metadata)
        .await
        .expect("persist first failed");

    store
        .persist(
            "resp_2",
            Some("resp_1"),
            vec![create_output_item("out_2")],
            None,
            &metadata,
        )
        .await
        .expect("persist second failed");

    let response = store.get("resp_2").await.expect("get failed");

    assert_eq!(response.previous_response_id, Some("resp_1".to_string()));
    assert_eq!(response.history_item_ids.len(), 2);

    let rehydrated = store.rehydrate("resp_2").await.expect("rehydrate failed");
    assert_eq!(rehydrated.1.len(), 2);
}

// Edge case tests

#[tokio::test]
async fn test_conversation_persist_empty_items() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let conversation = create_conversation(&store).await.expect("create failed");
    let conv_id = &conversation.conversation_id;

    let metadata = ResponseMetadata::default();

    // Persist with empty item list
    persist_turn(&store, conv_id, "resp_empty", vec![], &metadata)
        .await
        .expect("persist empty items failed");

    let rehydrated = store.rehydrate(conv_id).await.expect("rehydrate failed");

    assert!(rehydrated.items.is_empty());
}

#[tokio::test]
async fn test_conversation_rehydrate_after_multiple_varying_turns() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let conversation = create_conversation(&store).await.expect("create failed");
    let conv_id = &conversation.conversation_id;

    let metadata = ResponseMetadata::default();

    // Turn 1: 1 item
    persist_turn(&store, conv_id, "resp_1", vec![create_input_item("turn1")], &metadata)
        .await
        .expect("turn 1 failed");

    // Turn 2: 3 items
    persist_turn(
        &store,
        conv_id,
        "resp_2",
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
    persist_turn(
        &store,
        conv_id,
        "resp_3",
        vec![create_input_item("turn3"), create_output_item("out3")],
        &metadata,
    )
    .await
    .expect("turn 3 failed");

    let rehydrated = store.rehydrate(conv_id).await.expect("rehydrate failed");

    assert_eq!(rehydrated.items.len(), 6);
}

#[tokio::test]
async fn test_response_store_chaining_respects_foreign_key() {
    let pool = setup_pool().await;
    let store = ResponseStore::new(pool);

    let metadata = ResponseMetadata::default();

    // Create resp_1
    store
        .persist("resp_1", None, vec![create_input_item("first")], None, &metadata)
        .await
        .expect("resp_1 persist failed");

    // Try to create resp_3 with resp_2 as previous (resp_2 doesn't exist)
    // This should fail due to foreign key constraint
    let result = store
        .persist(
            "resp_3",
            Some("resp_2"),
            vec![create_output_item("out3")],
            None,
            &metadata,
        )
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

    let conversation = create_conversation(&store).await.expect("create failed");
    let conv_id = conversation.conversation_id.clone();
    // Both turns were rehydrated from the same conversation version.
    let version = store.rehydrate(&conv_id).await.expect("rehydrate failed").version;

    let spawn_turn = |response_id: &'static str, text: &'static str| {
        let store = ConversationStore::new(pool.clone());
        let conv_id = conv_id.clone();
        let version = version.clone();
        tokio::spawn(async move {
            store
                .persist(
                    &conv_id,
                    version,
                    response_id,
                    vec![create_input_item(text)],
                    None,
                    &ResponseMetadata::default(),
                )
                .await
        })
    };
    let (first, second) = (spawn_turn("resp_t1", "thread1"), spawn_turn("resp_t2", "thread2"));
    let results = [
        first.await.expect("turn task panicked"),
        second.await.expect("turn task panicked"),
    ];

    // Exactly one turn commits; the other is rejected as stale.
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .any(|result| matches!(result, Err(StorageError::ConversationConflict { .. })))
    );
    let rehydrated = store.rehydrate(&conv_id).await.expect("rehydrate failed");
    assert_eq!(rehydrated.items.len(), 1);
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
    let conversation = create_conversation(&writer_store_a)
        .await
        .expect("create conversation failed");
    let conv_id = conversation.conversation_id;
    let barrier = Arc::new(tokio::sync::Barrier::new(10));

    let spawn_writer = |writer_idx: usize, writer_store: ConversationStore| {
        let writer_conv_id = conv_id.clone();
        let writer_barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            writer_barrier.wait().await;
            for idx in 0..50 {
                writer_store
                    .create_items(
                        "default_tenant",
                        &writer_conv_id,
                        vec![create_input_item(&format!("writer {writer_idx} item {idx}"))],
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
    assert_eq!(final_items.items.len(), 100);

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
    let conversation = create_conversation(&store).await.expect("create conversation failed");
    let conv_id = conversation.conversation_id;
    let barrier = Arc::new(tokio::sync::Barrier::new(10));

    let spawn_writer = |writer_idx: usize| {
        let writer_store = store.clone();
        let writer_conv_id = conv_id.clone();
        let writer_barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            writer_barrier.wait().await;
            for idx in 0..50 {
                writer_store
                    .create_items(
                        "default_tenant",
                        &writer_conv_id,
                        vec![create_input_item(&format!("same pool writer {writer_idx} item {idx}"))],
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
    assert_eq!(final_items.items.len(), 100);

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
    let result = persist_turn(
        &store,
        "nonexistent_conv",
        "resp_1",
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

    let result = create_conversation(&store).await;
    assert!(result.is_err(), "expected error from disabled store");

    let err = result.unwrap_err();
    assert!(err.is_not_configured());
}

#[tokio::test]
async fn test_response_store_disabled() {
    let store = ResponseStore::disabled();

    let metadata = ResponseMetadata::default();
    let result = store
        .persist("resp_1", None, vec![create_input_item("test")], None, &metadata)
        .await;

    assert!(result.is_err(), "expected error from disabled store");

    let err = result.unwrap_err();
    assert!(err.is_not_configured());
}

#[tokio::test]
async fn test_conversation_store_get_after_create() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let created = create_conversation(&store).await.expect("create failed");

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
        .persist("resp_stored", None, items.clone(), None, &metadata)
        .await
        .expect("persist failed");

    let retrieved = store.get("resp_stored").await.expect("response should be found");

    assert_eq!(retrieved.response_id, "resp_stored");
    assert_eq!(retrieved.history_item_ids.len(), 2);
}

#[tokio::test]
async fn test_tool_search_conversation_metadata_matches_snapshot_version() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = create_conversation(&store).await.expect("create conversation");
    persist_turn(
        &store,
        &conversation.conversation_id,
        "resp_tool_search_initial",
        vec![create_input_item("initial")],
        &ResponseMetadata {
            model: "initial-model".to_owned(),
            ..ResponseMetadata::default()
        },
    )
    .await
    .expect("persist initial turn");

    let loaded_tool: agentic_core::types::tools::ResponsesTool = serde_json::from_value(serde_json::json!({
        "type": "function",
        "name": "get_weather",
        "description": "Get weather",
        "parameters": {"type": "object"},
        "defer_loading": true
    }))
    .expect("valid loaded function");
    let latest = ResponseMetadata {
        model: "latest-model".to_owned(),
        effective_tools: Some(vec![loaded_tool.clone()]),
        tool_search_loaded_tools: Some(vec![loaded_tool]),
        ..ResponseMetadata::default()
    };
    persist_turn(
        &store,
        &conversation.conversation_id,
        "resp_tool_search_latest",
        vec![create_input_item("latest")],
        &latest,
    )
    .await
    .expect("persist latest turn");

    // Simulate replicas whose clocks and process-local UUID order disagree with committed conversation-item sequence.
    sqlx::query("UPDATE responses SET created_at = $1 WHERE id = $2")
        .bind(9_999_i64)
        .bind("resp_tool_search_initial")
        .execute(pool.as_ref())
        .await
        .expect("make older turn appear newer by wall clock");
    sqlx::query("UPDATE responses SET created_at = $1 WHERE id = $2")
        .bind(1_i64)
        .bind("resp_tool_search_latest")
        .execute(pool.as_ref())
        .await
        .expect("make latest turn appear older by wall clock");

    let latest_item_id: String =
        sqlx::query_scalar("SELECT id FROM items WHERE conversation_id = $1 ORDER BY seq DESC LIMIT 1")
            .bind(&conversation.conversation_id)
            .fetch_one(pool.as_ref())
            .await
            .expect("latest conversation item");
    let branch = ResponseMetadata {
        model: "branch-model".to_owned(),
        ..ResponseMetadata::default()
    };
    sqlx::query(
        "INSERT INTO responses (id, previous_response_id, history_item_ids, metadata, created_at) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("resp_tool_search_branch")
    .bind("resp_tool_search_latest")
    .bind(serde_json::to_string(&vec![latest_item_id]).expect("branch history JSON"))
    .bind(String::try_from(&branch).expect("branch metadata JSON"))
    .bind(20_000_i64)
    .execute(pool.as_ref())
    .await
    .expect("seed a newer response branched from the latest turn");

    let snapshot = store
        .rehydrate(&conversation.conversation_id)
        .await
        .expect("rehydrate typed snapshot");
    persist_turn(
        &store,
        &conversation.conversation_id,
        "resp_after_snapshot",
        vec![create_input_item("after snapshot")],
        &ResponseMetadata {
            model: "after-snapshot-model".to_owned(),
            ..ResponseMetadata::default()
        },
    )
    .await
    .expect("persist a newer conversation turn");
    // The snapshot keeps the metadata of the turn it was rehydrated at.
    let metadata = snapshot
        .turn_metadata
        .expect("response metadata accompanies conversation items");
    assert_eq!(metadata.model, "latest-model");
    assert_eq!(metadata.tool_search_loaded_tools.as_deref().map(<[_]>::len), Some(1));
}

#[tokio::test]
async fn test_tool_search_conversation_conflict_does_not_persist_stale_loaded_state() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);
    let conversation = create_conversation(&store).await.expect("create conversation");
    let snapshot = store
        .rehydrate(&conversation.conversation_id)
        .await
        .expect("capture empty version");
    let winning_tool: agentic_core::types::tools::ResponsesTool = serde_json::from_value(serde_json::json!({
        "type": "function",
        "name": "winning_tool",
        "parameters": {"type": "object"},
        "defer_loading": true
    }))
    .expect("winning tool");
    let stale_tool: agentic_core::types::tools::ResponsesTool = serde_json::from_value(serde_json::json!({
        "type": "function",
        "name": "stale_tool",
        "parameters": {"type": "object"},
        "defer_loading": true
    }))
    .expect("stale tool");
    let winning = ResponseMetadata {
        multi_agent_tree: None,
        model: "winner".to_owned(),
        effective_tools: Some(vec![winning_tool.clone()]),
        tool_search_loaded_tools: Some(vec![winning_tool]),
        ..ResponseMetadata::default()
    };
    let stale = ResponseMetadata {
        multi_agent_tree: None,
        model: "stale".to_owned(),
        effective_tools: Some(vec![stale_tool.clone()]),
        tool_search_loaded_tools: Some(vec![stale_tool]),
        ..ResponseMetadata::default()
    };

    store
        .persist(
            &conversation.conversation_id,
            snapshot.version.clone(),
            "resp_tool_search_winner",
            vec![create_input_item("winner")],
            None,
            &winning,
        )
        .await
        .expect("winning turn persists");
    let error = store
        .persist(
            &conversation.conversation_id,
            snapshot.version,
            "resp_tool_search_stale",
            vec![create_input_item("stale")],
            None,
            &stale,
        )
        .await
        .expect_err("stale turn conflicts");
    assert!(matches!(error, StorageError::ConversationConflict { .. }));

    let snapshot = store
        .rehydrate(&conversation.conversation_id)
        .await
        .expect("rehydrate winning state");
    let latest = snapshot.turn_metadata.expect("winning metadata");
    assert_eq!(latest.model, "winner");
    let serialized = serde_json::to_value(latest.tool_search_loaded_tools).unwrap();
    assert_eq!(serialized[0]["name"], "winning_tool");
    assert!(!serialized.to_string().contains("stale_tool"));
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
