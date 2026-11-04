mod support;

use agentic_core::config::SqliteConfig;
use agentic_core::storage::ResponseCheckpoint;
use agentic_core::storage::{
    ConversationData, ConversationStore, ResponseStore, StoreResult, create_pool_with_schema,
    create_pool_with_schema_and_sqlite_config,
};
use agentic_core::storage::{ResponseData, ResponseMetadata, StorageError};
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

    let retrieved = store
        .retrieve("default_tenant", &created.conversation_id)
        .await
        .expect("retrieve failed");

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
    assert!(snapshot.checkpoint.response_id.is_none());

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
    assert!(snapshot.checkpoint.response_id.is_some());
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
async fn items_added_before_a_turn_precede_it() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let conversation = create_conversation(&store).await?;
    let added = create_input_item("item");
    store
        .create_items("default_tenant", &conversation.conversation_id, vec![added.clone()])
        .await?;

    let item_only = store.rehydrate(&conversation.conversation_id).await?;
    // Adding items records no turn.
    assert!(item_only.checkpoint.response_id.is_none());

    let turn = create_input_item("turn");
    persist_turn(
        &store,
        &conversation.conversation_id,
        "resp_after_items",
        vec![turn.clone()],
        &ResponseMetadata::default(),
    )
    .await?;
    let recorded = store.rehydrate(&conversation.conversation_id).await?;
    assert_eq!(recorded.items, vec![added, turn]);
    assert!(recorded.checkpoint.response_id.is_some());

    Ok(())
}

#[tokio::test]
async fn first_turn_stores_its_items() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);
    let conversation = create_conversation(&store).await?;
    let items = vec![create_input_item("first input"), create_output_item("msg_first")];

    store
        .persist(
            &conversation.conversation_id,
            &ResponseCheckpoint::default(),
            "resp_first",
            items.clone(),
            None,
            &ResponseMetadata::default(),
        )
        .await?;

    let snapshot = store.rehydrate(&conversation.conversation_id).await?;
    assert_eq!(snapshot.items, items);

    Ok(())
}

#[tokio::test]
async fn zero_item_turns_record_their_exact_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);
    let conversation = create_conversation(&store).await?;

    for model in ["first-zero-item-turn", "second-zero-item-turn"] {
        let metadata = ResponseMetadata {
            multi_agent_tree: None,
            model: model.to_owned(),
            ..ResponseMetadata::default()
        };
        persist_turn(
            &store,
            &conversation.conversation_id,
            &format!("resp_{model}"),
            Vec::new(),
            &metadata,
        )
        .await?;

        let snapshot = store.rehydrate(&conversation.conversation_id).await?;
        assert!(snapshot.items.is_empty());
        assert_eq!(Some(snapshot.checkpoint.metadata.model.as_str()), Some(model));
    }

    Ok(())
}

#[tokio::test]
async fn turn_appends_after_a_turn_stored_while_it_ran() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(Arc::clone(&pool));
    let response_store = ResponseStore::new(Arc::clone(&pool));
    let conversation = create_conversation(&store).await?;
    // Both turns start from the empty conversation; OpenAI stores each after whatever is there when it finishes.
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
    let later_items = vec![create_input_item("later input"), create_output_item("msg_later")];

    persist_turn(
        &store,
        &conversation.conversation_id,
        "resp_later",
        later_items.clone(),
        &ResponseMetadata::default(),
    )
    .await?;

    assert_eq!(
        store.rehydrate(&conversation.conversation_id).await?.items,
        [competing_items, later_items].concat()
    );
    response_store.get("resp_later").await?;

    Ok(())
}

#[tokio::test]
async fn a_turn_changes_only_its_own_conversation() -> Result<(), Box<dyn std::error::Error>> {
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
    store
        .persist(
            &second.conversation_id,
            &ResponseCheckpoint::default(),
            "resp_second_conversation",
            vec![create_input_item("second conversation")],
            None,
            &ResponseMetadata::default(),
        )
        .await?;

    let first_after = store.rehydrate(&first.conversation_id).await?;
    assert_eq!(first_after.items, first_items);

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

    let response = store
        .get("resp_get_test")
        .await
        .and_then(ResponseData::try_from)
        .expect("get failed");

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
    let item_id = ResponseData::try_from(store.get("resp_corrupt").await?)?
        .history_item_ids
        .remove(0);

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
            Some(
                &store
                    .get("resp_1")
                    .await
                    .and_then(ResponseData::try_from)
                    .expect("parent")
                    .into(),
            ),
            vec![create_output_item("out_2")],
            None,
            &metadata,
        )
        .await
        .expect("persist second failed");

    let response = store
        .get("resp_2")
        .await
        .and_then(ResponseData::try_from)
        .expect("get failed");

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
            Some(&ResponseCheckpoint {
                response_id: Some("resp_2".to_owned()),
                durable: true,
                ..ResponseCheckpoint::default()
            }),
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

    let spawn_turn = |response_id: &'static str, text: &'static str| {
        let store = ConversationStore::new(pool.clone());
        let conv_id = conv_id.clone();
        tokio::spawn(async move {
            store
                .persist(
                    &conv_id,
                    &ResponseCheckpoint::default(),
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

    // As on OpenAI, both turns commit, each appended after the other's items in commit order.
    assert!(results.iter().all(Result::is_ok));
    let rehydrated = store.rehydrate(&conv_id).await.expect("rehydrate failed");
    assert_eq!(rehydrated.items.len(), 2);
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
async fn test_conversation_store_rehydrate_nonexistent() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let result = store.rehydrate("nonexistent_conv").await;
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
async fn test_response_store_get_after_persist() {
    let pool = setup_pool().await;
    let store = ResponseStore::new(pool);

    let items = vec![create_input_item("query"), create_output_item("out_1")];
    let metadata = ResponseMetadata::default();

    store
        .persist("resp_stored", None, items.clone(), None, &metadata)
        .await
        .expect("persist failed");

    let retrieved = store
        .get("resp_stored")
        .await
        .and_then(ResponseData::try_from)
        .expect("response should be found");

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
    let metadata = snapshot.checkpoint.metadata;
    assert_eq!(metadata.model, "latest-model");
    assert_eq!(metadata.tool_search_loaded_tools.as_deref().map(<[_]>::len), Some(1));
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
