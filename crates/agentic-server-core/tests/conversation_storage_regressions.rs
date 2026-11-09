//! Storage-level conversation and item regressions using the existing tables directly.

mod support;

use agentic_core::types::conversations::ConversationMetadata;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agentic_core::executor::{ConversationHandler, ExecutorError};
use agentic_core::storage::models::{
    conversation as conversation_model, item as item_model, response as response_model,
};
use agentic_core::storage::{
    ConversationStore, Item, ResponseData, ResponseMetadata, ResponseStore, create_pool_with_schema,
};
use agentic_core::types::io::InOutItem;
use agentic_core::types::io::ItemOrder;
use serde_json::json;
use support::persist_turn;

static COUNTER: AtomicU64 = AtomicU64::new(0);

async fn test_store() -> ConversationStore {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let url = format!("sqlite:file:conversation_storage_regression_{id}?mode=memory&cache=shared");
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    ConversationStore::new(pool)
}

/// Stored rows of a conversation in sequence order, for checks below the public item API.
async fn conversation_rows(store: &ConversationStore, conversation_id: &str) -> Vec<Item> {
    conversation_model::get_with_items(store.pool().unwrap(), conversation_id)
        .await
        .unwrap()
        .expect("conversation exists")
        .items
}

/// Stored history rows of a response in order, for checks below the public item API.
async fn response_rows(store: &ConversationStore, response_id: &str) -> Vec<Item> {
    let response = response_model::get(store.pool().unwrap(), response_id)
        .await
        .unwrap()
        .expect("response exists");
    let ids = response.history_item_ids_vec().unwrap();
    let mut rows = item_model::get_items(store.pool().unwrap(), &ids).await.unwrap();
    rows.sort_by_key(|row| ids.iter().position(|id| id == &row.id));
    rows
}

/// Whether an item row with this storage ID still exists.
async fn item_row_exists(store: &ConversationStore, row_id: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items WHERE id = $1")
        .bind(row_id)
        .fetch_one(store.pool().unwrap())
        .await
        .unwrap()
        == 1
}

fn regression_message(text: &str) -> InOutItem {
    InOutItem::Input(
        serde_json::from_value(json!({
            "type": "message", "role": "user", "content": text
        }))
        .unwrap(),
    )
}

#[tokio::test]
async fn regression_response_items_are_visible() {
    let store = test_store().await;
    let conv = store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .unwrap();
    persist_turn(
        &store,
        &conv.conversation_id,
        "resp_review",
        vec![regression_message("from response persistence")],
        &ResponseMetadata::default(),
    )
    .await
    .unwrap();
    assert_eq!(store.rehydrate(&conv.conversation_id).await.unwrap().items.len(), 1);
    let rows = store
        .list_items("default_tenant", &conv.conversation_id, 100, None, ItemOrder::Asc)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn regression_delete_preserves_items() {
    let store = test_store().await;
    let conv = store
        .create(
            "default_tenant",
            &ConversationMetadata::new(),
            &[regression_message("preserve me")],
        )
        .await
        .unwrap();
    let rows = conversation_rows(&store, &conv.conversation_id).await;
    store.delete("default_tenant", &conv.conversation_id).await.unwrap();
    assert!(item_row_exists(&store, &rows[0].id).await);
}

#[tokio::test]
async fn regression_cursor_follows_sequence() {
    let store = test_store().await;
    let conv = store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .unwrap();
    let data = serde_json::to_string(&regression_message("hello")).unwrap();
    let earlier_id = agentic_core::utils::common::uuid7_str("item_");
    let later_id = agentic_core::utils::common::uuid7_str("item_");
    assert!(earlier_id < later_id);
    // IDs are allocated before awaiting storage, so creation order can differ from insertion order.
    insert_with_ids(
        store.pool().unwrap(),
        &conv.conversation_id,
        vec![(later_id.clone(), data.clone())],
    )
    .await
    .unwrap();
    insert_with_ids(
        store.pool().unwrap(),
        &conv.conversation_id,
        vec![(earlier_id.clone(), data)],
    )
    .await
    .unwrap();
    let page = store
        .list_items("default_tenant", &conv.conversation_id, 1, None, ItemOrder::Asc)
        .await
        .unwrap();
    assert_eq!(page[0].id(), Some(later_id.as_str()));
    let next = store
        .list_items("default_tenant", &conv.conversation_id, 1, page[0].id(), ItemOrder::Asc)
        .await
        .unwrap();
    assert_eq!(next.len(), 1, "second item skipped despite having the next sequence");
}

#[tokio::test]
async fn regression_turn_after_an_item_deletion_appends() {
    let store = test_store().await;
    let conv = store
        .create(
            "default_tenant",
            &ConversationMetadata::new(),
            &[regression_message("first"), regression_message("last")],
        )
        .await
        .unwrap();
    let items = store
        .list_items("default_tenant", &conv.conversation_id, 100, None, ItemOrder::Asc)
        .await
        .unwrap();
    store
        .delete_item("default_tenant", &conv.conversation_id, items[0].id().unwrap())
        .await
        .unwrap();
    // OpenAI stores a turn that ran while an item was deleted after the remaining items.
    persist_turn(
        &store,
        &conv.conversation_id,
        "resp_after_deletion",
        vec![regression_message("based on deleted history")],
        &ResponseMetadata::default(),
    )
    .await
    .unwrap();
    let snapshot = store.rehydrate(&conv.conversation_id).await.unwrap();
    assert_eq!(snapshot.items.len(), 2);
    assert_eq!(snapshot.items[1], regression_message("based on deleted history"));
}

#[tokio::test]
async fn regression_concurrent_item_creation() {
    let store = test_store().await;
    let pool = Arc::new(store.pool().unwrap().clone());
    let conv = store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(10));
    let mut tasks = Vec::new();
    for idx in 0..10 {
        let pool = Arc::clone(&pool);
        let barrier = Arc::clone(&barrier);
        let id = conv.conversation_id.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            ConversationStore::new(pool)
                .create_items("default_tenant", &id, vec![regression_message(&format!("hello {idx}"))])
                .await
        }));
    }
    let results = futures::future::join_all(tasks).await;
    let errors: Vec<_> = results.into_iter().filter_map(|r| r.unwrap().err()).collect();
    assert!(errors.is_empty(), "{errors:?}");
}

async fn insert_with_ids(
    pool: &agentic_core::storage::DbPool,
    id: &str,
    items: Vec<(String, String)>,
) -> agentic_core::storage::StoreResult<()> {
    let items = items
        .into_iter()
        .map(|(id, data)| Item {
            public_id: id.clone(),
            id,
            metadata: r#"{"kind":"input"}"#.to_owned(),
            data,
            created_at: 0,
            conversation_id: None,
            seq: None,
            tenant_id: None,
        })
        .collect();
    let mut tx = pool.begin().await?;
    agentic_core::storage::models::conversation::lock_in_tx(&mut tx, id).await?;
    item_model::create_in_tx(&mut tx, items, Some(id)).await?;
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
async fn deletion_preserves_stored_response_history() {
    let store = test_store().await;
    let conv = store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .unwrap();
    let inputs = vec![regression_message("keep response history")];
    persist_turn(
        &store,
        &conv.conversation_id,
        "resp_kept",
        inputs.clone(),
        &ResponseMetadata::default(),
    )
    .await
    .unwrap();
    let rows = store
        .list_items("default_tenant", &conv.conversation_id, 100, None, ItemOrder::Asc)
        .await
        .unwrap();
    store
        .delete_item("default_tenant", &conv.conversation_id, rows[0].id().unwrap())
        .await
        .unwrap();
    let response_store = ResponseStore::new(Arc::new(store.pool().unwrap().clone()));
    assert_eq!(response_store.rehydrate("resp_kept").await.unwrap().1, inputs);
    persist_turn(
        &store,
        &conv.conversation_id,
        "resp_kept_two",
        inputs.clone(),
        &ResponseMetadata::default(),
    )
    .await
    .unwrap();
    store.delete("default_tenant", &conv.conversation_id).await.unwrap();
    assert_eq!(response_store.rehydrate("resp_kept_two").await.unwrap().1, inputs);
}

#[tokio::test]
async fn response_checkpoint_preserves_manual_items_after_conversation_deletion() {
    let store = test_store().await;
    let conv = store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .unwrap();
    let first = regression_message("SAPPHIRE");
    let manual = regression_message("ORCHID");
    let second = regression_message("OK");
    persist_turn(
        &store,
        &conv.conversation_id,
        "resp_before_manual",
        vec![first.clone()],
        &ResponseMetadata::default(),
    )
    .await
    .unwrap();
    let added = store
        .create_items("default_tenant", &conv.conversation_id, vec![manual.clone()])
        .await
        .unwrap();
    persist_turn(
        &store,
        &conv.conversation_id,
        "resp_after_manual",
        vec![second.clone()],
        &ResponseMetadata::default(),
    )
    .await
    .unwrap();
    store
        .delete_item("default_tenant", &conv.conversation_id, added[0].id().unwrap())
        .await
        .unwrap();

    let response_store = ResponseStore::new(Arc::new(store.pool().unwrap().clone()));
    assert_eq!(
        response_store.rehydrate("resp_before_manual").await.unwrap().1,
        vec![first.clone()]
    );
    assert_eq!(
        response_store.rehydrate("resp_after_manual").await.unwrap().1,
        vec![first.clone(), manual, second.clone()]
    );
    assert_eq!(
        store.rehydrate(&conv.conversation_id).await.unwrap().items,
        vec![first, second]
    );
}

#[tokio::test]
async fn item_operations_reject_another_tenant_and_another_conversations_cursor() {
    let store = test_store().await;
    let conv = store
        .create(
            "tenant_a",
            &ConversationMetadata::new(),
            &[regression_message("private")],
        )
        .await
        .unwrap();
    let other = store
        .create("tenant_a", &ConversationMetadata::new(), &[])
        .await
        .unwrap();
    let items = store
        .list_items("tenant_a", &conv.conversation_id, 100, None, ItemOrder::Asc)
        .await
        .unwrap();
    let id = items[0].id().unwrap();
    assert!(
        store
            .retrieve_item("tenant_b", &conv.conversation_id, id)
            .await
            .is_err()
    );
    assert!(store.delete_item("tenant_b", &conv.conversation_id, id).await.is_err());
    assert!(
        store
            .create_items("tenant_b", &conv.conversation_id, vec![regression_message("injected")])
            .await
            .is_err()
    );
    assert!(
        store
            .list_items("tenant_a", &other.conversation_id, 100, Some(id), ItemOrder::Asc)
            .await
            .is_err()
    );
    assert_eq!(store.rehydrate(&conv.conversation_id).await.unwrap().items.len(), 1);
}

#[tokio::test]
async fn regression_invalid_item_id_rejects_entire_batch_before_writing() {
    let store = test_store().await;
    let handler = ConversationHandler::new(store.clone());
    let conversation = handler
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .unwrap();
    let invalid = InOutItem::Input(
        serde_json::from_value(json!({
            "type":"message", "id":"item_wrongprefix", "role":"user", "content":"invalid"
        }))
        .unwrap(),
    );
    let error = handler
        .create_items(
            "default_tenant",
            &conversation.conversation_id,
            vec![regression_message("valid"), invalid.clone()],
        )
        .await
        .unwrap_err();
    assert!(matches!(&error, ExecutorError::ClientItem(error) if error.param() == "items[1].id"));
    assert!(
        store
            .rehydrate(&conversation.conversation_id)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let error = handler
        .create("default_tenant", &ConversationMetadata::new(), &[invalid])
        .await
        .unwrap_err();
    assert!(matches!(&error, ExecutorError::ClientItem(error) if error.param() == "items[0].id"));
}

fn message_with_id(id: &str, text: &str) -> InOutItem {
    InOutItem::Input(serde_json::from_value(json!({"type":"message", "id":id, "role":"user", "content":text})).unwrap())
}

#[tokio::test]
async fn regression_database_rejects_duplicate_items_and_rolls_back_all_batches() {
    use agentic_core::storage::StorageError;

    let store = test_store().await;
    let conversation = store
        .create(
            "default_tenant",
            &ConversationMetadata::new(),
            &[message_with_id("msg_existing", "original")],
        )
        .await
        .unwrap();
    for conflict in ["msg_new_0", "msg_existing"] {
        // More items than one INSERT permits: a conflict in the second SQL batch
        // must roll back the earlier successful batch as well.
        let mut items: Vec<_> = (0..250)
            .map(|index| message_with_id(&format!("msg_new_{index}"), "new"))
            .collect();
        items.push(message_with_id(conflict, "conflict"));
        let error = store
            .create_items("default_tenant", &conversation.conversation_id, items)
            .await
            .unwrap_err();
        if conflict == "msg_existing" {
            assert!(error.is_validation());
            assert!(matches!(error, StorageError::ItemAlreadyInConversation));
        } else {
            assert!(error.is_unique_violation());
        }
        let rows = conversation_rows(&store, &conversation.conversation_id).await;
        assert_eq!(rows.len(), 1, "failed insertion must leave the history unchanged");
        assert_eq!(rows[0].public_id, "msg_existing");
        assert_eq!(
            InOutItem::try_from(&rows[0]).unwrap(),
            message_with_id("msg_existing", "original")
        );
    }
    store
        .create_items(
            "default_tenant",
            &conversation.conversation_id,
            vec![message_with_id("msg_new_0", "retry")],
        )
        .await
        .unwrap();
    let rows = conversation_rows(&store, &conversation.conversation_id).await;
    assert_eq!(rows[1].public_id, "msg_new_0");
    assert_eq!(rows[1].seq, Some(1));
}

#[tokio::test]
async fn regression_duplicate_initial_items_roll_back_conversation_creation() {
    let store = test_store().await;
    let error = store
        .create(
            "default_tenant",
            &ConversationMetadata::new(),
            &[
                message_with_id("msg_duplicate", "first"),
                message_with_id("msg_duplicate", "second"),
            ],
        )
        .await
        .unwrap_err();
    assert!(error.is_unique_violation());
    for query in ["SELECT COUNT(*) FROM conversations", "SELECT COUNT(*) FROM items"] {
        let count: i64 = sqlx::query_scalar(query)
            .fetch_one(store.pool().unwrap())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[tokio::test]
async fn regression_concurrent_insert_checks_membership_under_the_conversation_lock() {
    use agentic_core::storage::StorageError;

    let store = test_store().await;
    let conversation = store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        store.create_items(
            "default_tenant",
            &conversation.conversation_id,
            vec![message_with_id("msg_race", "first")]
        ),
        store.create_items(
            "default_tenant",
            &conversation.conversation_id,
            vec![message_with_id("msg_race", "second")]
        ),
    );
    let (Ok(created), Err(error)) = (match (first, second) {
        (first @ Ok(_), second) => (first, second),
        (first, second) => (second, first),
    }) else {
        panic!("exactly one insert should succeed");
    };
    assert!(matches!(error, StorageError::ItemAlreadyInConversation));
    let snapshot = store.rehydrate(&conversation.conversation_id).await.unwrap();
    assert_eq!(
        snapshot.items.iter().map(InOutItem::id).collect::<Vec<_>>(),
        created.iter().map(InOutItem::id).collect::<Vec<_>>()
    );
}

#[path = "conversations/item_references.rs"]
mod item_references;

#[tokio::test]
async fn response_history_keeps_each_occurrence_of_a_public_id() {
    let store = test_store().await;
    let responses = ResponseStore::new(Arc::new(store.pool().unwrap().clone()));
    let original = message_with_id("msg_reused", "original");
    let updated = message_with_id("msg_reused", "updated");
    let metadata = ResponseMetadata::default();
    responses
        .persist("resp_first", None, vec![original.clone()], None, &metadata)
        .await
        .unwrap();

    let replay = vec![original.clone(), updated.clone(), original.clone()];
    responses
        .persist("resp_replay", None, replay.clone(), None, &metadata)
        .await
        .unwrap();
    assert_eq!(responses.rehydrate("resp_replay").await.unwrap().1, replay);
    assert_eq!(
        responses.rehydrate("resp_first").await.unwrap().1,
        vec![original.clone()]
    );

    responses
        .persist(
            "resp_next",
            Some(
                &responses
                    .get("resp_replay")
                    .await
                    .and_then(ResponseData::try_from)
                    .expect("parent")
                    .into(),
            ),
            vec![updated.clone()],
            None,
            &metadata,
        )
        .await
        .unwrap();
    let mut expected = replay;
    expected.push(updated);
    assert_eq!(responses.rehydrate("resp_next").await.unwrap().1, expected);
    let rows = response_rows(&store, "resp_next").await;
    assert_eq!(
        rows.iter()
            .map(|row| &row.id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4
    );
    assert!(rows.iter().all(|row| row.public_id == "msg_reused"));
}

#[tokio::test]
async fn response_history_does_not_reuse_another_tenants_item() {
    let store = test_store().await;
    let original = message_with_id("msg_shared", "private content");
    let conversation = store
        .create(
            "other_tenant",
            &ConversationMetadata::new(),
            std::slice::from_ref(&original),
        )
        .await
        .unwrap();
    let responses = ResponseStore::new(Arc::new(store.pool().unwrap().clone()));
    let supplied = message_with_id("msg_shared", "supplied content");
    responses
        .persist(
            "resp_shared",
            None,
            vec![supplied.clone()],
            None,
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
    assert_eq!(responses.rehydrate("resp_shared").await.unwrap().1, vec![supplied]);
    assert_eq!(
        store.rehydrate(&conversation.conversation_id).await.unwrap().items,
        vec![original]
    );
    let rows = response_rows(&store, "resp_shared").await;
    assert_eq!(rows[0].public_id, "msg_shared");
    assert_eq!(rows[0].tenant_id.as_deref(), Some("default_tenant"));
}

#[tokio::test]
async fn response_history_batches_keep_order_and_roll_back_together() {
    let store = test_store().await;
    let responses = ResponseStore::new(Arc::new(store.pool().unwrap().clone()));
    // More items than one INSERT permits, all sharing one public ID.
    let items: Vec<_> = (0..350)
        .map(|index| message_with_id("msg_repeated", &format!("position {index}")))
        .collect();
    let metadata = ResponseMetadata::default();
    responses
        .persist("resp_batches", None, items.clone(), None, &metadata)
        .await
        .unwrap();
    assert_eq!(responses.rehydrate("resp_batches").await.unwrap().1, items);

    let error = responses
        .persist("resp_batches", None, items, None, &metadata)
        .await
        .unwrap_err();
    assert!(error.is_unique_violation());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(store.pool().unwrap())
        .await
        .unwrap();
    assert_eq!(count, 350, "a failed response insert must roll back every item batch");
}
