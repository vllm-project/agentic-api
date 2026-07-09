mod support;

use agentic_core::storage::InOutItem;
use agentic_core::storage::ResponseMetadata;
use agentic_core::storage::{ConversationStore, ResponseStore};
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
