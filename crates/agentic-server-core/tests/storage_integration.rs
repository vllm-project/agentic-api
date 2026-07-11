mod support;

use agentic_core::config::SqliteConfig;
use agentic_core::storage::InOutItem;
use agentic_core::storage::ResponseMetadata;
use agentic_core::storage::{
    ConversationStore, ResponseStore, create_pool_with_schema, create_pool_with_schema_and_sqlite_config,
};
use agentic_core::types::event::MessageStatus;
use agentic_core::types::io::{InputItem, InputMessage, InputMessageContent, OutputItem, OutputMessage};
use std::sync::Arc;

use support::setup_pool;

fn create_input_item(text: &str) -> InOutItem {
    InOutItem::Input(InputItem::Message(InputMessage {
        role: "user".to_string(),
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

    assert!(
        result.is_err(),
        "expected error when persisting to non-existent conversation"
    );
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
