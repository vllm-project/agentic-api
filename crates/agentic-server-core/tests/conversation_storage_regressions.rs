//! Storage-level conversation and item regressions using the existing tables directly.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agentic_core::storage::models::item as item_model;
use agentic_core::storage::{ConversationStore, InOutItem, ResponseMetadata, ResponseStore, create_pool_with_schema};
use agentic_core::types::conversations::ItemOrder;
use serde_json::json;

static COUNTER: AtomicU64 = AtomicU64::new(0);

async fn test_store() -> ConversationStore {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let url = format!("sqlite:file:conversation_storage_regression_{id}?mode=memory&cache=shared");
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    ConversationStore::new(pool)
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
        .create_with_metadata_and_items(Some("default_tenant"), None, vec![])
        .await
        .unwrap();
    store
        .persist(
            &conv.conversation_id,
            "resp_review",
            None,
            vec![regression_message("from response persistence")],
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
    assert_eq!(store.rehydrate(&conv.conversation_id).await.unwrap().len(), 1);
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
        .create_with_metadata_and_items(Some("default_tenant"), None, vec![regression_message("preserve me")])
        .await
        .unwrap();
    let items = store
        .list_items("default_tenant", &conv.conversation_id, 100, None, ItemOrder::Asc)
        .await
        .unwrap();
    store.delete("default_tenant", &conv.conversation_id).await.unwrap();
    let preserved = item_model::get_items(store.pool().unwrap(), &[items[0].id.clone()])
        .await
        .unwrap();
    assert_eq!(preserved.len(), 1);
}

#[tokio::test]
async fn regression_cursor_follows_sequence() {
    let store = test_store().await;
    let conv = store
        .create_with_metadata_and_items(Some("default_tenant"), None, vec![])
        .await
        .unwrap();
    let data = String::try_from(&regression_message("hello")).unwrap();
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
    assert_eq!(page[0].id, later_id);
    let next = store
        .list_items(
            "default_tenant",
            &conv.conversation_id,
            1,
            Some(&page[0].id),
            ItemOrder::Asc,
        )
        .await
        .unwrap();
    assert_eq!(next.len(), 1, "second item skipped despite having the next sequence");
}

#[tokio::test]
async fn regression_delete_invalidates_version() {
    let store = test_store().await;
    let conv = store
        .create_with_metadata_and_items(
            Some("default_tenant"),
            None,
            vec![regression_message("first"), regression_message("last")],
        )
        .await
        .unwrap();
    let snapshot = store.rehydrate_snapshot(&conv.conversation_id).await.unwrap();
    let items = store
        .list_items("default_tenant", &conv.conversation_id, 100, None, ItemOrder::Asc)
        .await
        .unwrap();
    store
        .delete_item("default_tenant", &conv.conversation_id, &items[0].id)
        .await
        .unwrap();
    let result = store
        .persist_if_version(
            &conv.conversation_id,
            snapshot.version,
            "resp_stale",
            None,
            vec![regression_message("based on deleted history")],
            &ResponseMetadata::default(),
        )
        .await;
    assert!(
        matches!(
            result,
            Err(agentic_core::storage::StorageError::ConversationConflict { .. })
        ),
        "{result:?}"
    );
}

#[tokio::test]
async fn regression_concurrent_item_creation() {
    let store = test_store().await;
    let pool = Arc::new(store.pool().unwrap().clone());
    let conv = store
        .create_with_metadata_and_items(Some("default_tenant"), None, vec![])
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
    let mut tx = pool.begin().await?;
    agentic_core::storage::models::conversation::lock_in_tx(&mut tx, id).await?;
    // Typed stores own item insertion; raw rows keep legacy NULL provenance.
    for (item_id, data) in items {
        let sequence: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), -1) + 1 FROM items WHERE conversation_id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query(
            "INSERT INTO items (id, data, created_at, conversation_id, seq, tenant_id) \
             SELECT $1, $2, $3, $4, $5, COALESCE(tenant_id, 'default_tenant') FROM conversations WHERE id = $4",
        )
        .bind(item_id)
        .bind(data)
        .bind(0_i64)
        .bind(id)
        .bind(sequence)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
async fn deletion_preserves_stored_response_history_and_invalidates_empty_snapshot() {
    let store = test_store().await;
    let conv = store
        .create_with_metadata_and_items(Some("default_tenant"), None, vec![])
        .await
        .unwrap();
    let empty = store.rehydrate_snapshot(&conv.conversation_id).await.unwrap();
    let inputs = vec![regression_message("keep response history")];
    store
        .persist(
            &conv.conversation_id,
            "resp_kept",
            None,
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
        .delete_item("default_tenant", &conv.conversation_id, &rows[0].id)
        .await
        .unwrap();
    let result = store
        .persist_if_version(
            &conv.conversation_id,
            empty.version,
            "resp_stale_empty",
            None,
            vec![],
            &ResponseMetadata::default(),
        )
        .await;
    assert!(matches!(
        result,
        Err(agentic_core::storage::StorageError::ConversationConflict { .. })
    ));
    let response_store = ResponseStore::new(Arc::new(store.pool().unwrap().clone()));
    assert_eq!(response_store.rehydrate("resp_kept").await.unwrap(), inputs);
    store
        .persist(
            &conv.conversation_id,
            "resp_kept_two",
            None,
            inputs.clone(),
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
    store.delete("default_tenant", &conv.conversation_id).await.unwrap();
    assert_eq!(response_store.rehydrate("resp_kept_two").await.unwrap(), inputs);
}

#[tokio::test]
async fn response_checkpoint_preserves_manual_items_after_conversation_deletion() {
    let store = test_store().await;
    let conv = store
        .create_with_metadata_and_items(Some("default_tenant"), None, vec![])
        .await
        .unwrap();
    let first = regression_message("SAPPHIRE");
    let manual = regression_message("ORCHID");
    let second = regression_message("OK");
    store
        .persist(
            &conv.conversation_id,
            "resp_before_manual",
            None,
            vec![first.clone()],
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
    let added = store
        .create_items("default_tenant", &conv.conversation_id, vec![manual.clone()])
        .await
        .unwrap();
    store
        .persist(
            &conv.conversation_id,
            "resp_after_manual",
            None,
            vec![second.clone()],
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
    store
        .delete_item("default_tenant", &conv.conversation_id, &added[0].id)
        .await
        .unwrap();

    let response_store = ResponseStore::new(Arc::new(store.pool().unwrap().clone()));
    assert_eq!(
        response_store.rehydrate("resp_before_manual").await.unwrap(),
        vec![first.clone()]
    );
    assert_eq!(
        response_store.rehydrate("resp_after_manual").await.unwrap(),
        vec![first.clone(), manual, second.clone()]
    );
    assert_eq!(
        store.rehydrate(&conv.conversation_id).await.unwrap(),
        vec![first, second]
    );
}

#[tokio::test]
async fn item_operations_reject_another_tenant_and_another_conversations_cursor() {
    let store = test_store().await;
    let conv = store
        .create_with_metadata_and_items(Some("tenant_a"), None, vec![regression_message("private")])
        .await
        .unwrap();
    let other = store
        .create_with_metadata_and_items(Some("tenant_a"), None, vec![])
        .await
        .unwrap();
    let items = store
        .list_items("tenant_a", &conv.conversation_id, 100, None, ItemOrder::Asc)
        .await
        .unwrap();
    let id = &items[0].id;
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
    assert_eq!(store.rehydrate(&conv.conversation_id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn regression_invalid_item_id_rejects_entire_batch_before_writing() {
    use agentic_core::storage::StorageError;

    let store = test_store().await;
    let conversation = store
        .create_with_metadata_and_items(Some("default_tenant"), None, vec![])
        .await
        .unwrap();
    let invalid = InOutItem::Input(
        serde_json::from_value(json!({
            "type":"message", "id":"item_wrongprefix", "role":"user", "content":"invalid"
        }))
        .unwrap(),
    );
    let error = store
        .create_items(
            "default_tenant",
            &conversation.conversation_id,
            vec![regression_message("valid"), invalid.clone()],
        )
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::InvalidItemId { param, .. } if param == "items[1].id"));
    assert!(store.rehydrate(&conversation.conversation_id).await.unwrap().is_empty());
    let error = store
        .create_with_metadata_and_items(Some("default_tenant"), None, vec![invalid])
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::InvalidItemId { param, .. } if param == "items[0].id"));
}

fn message_with_id(id: &str, text: &str) -> InOutItem {
    InOutItem::Input(serde_json::from_value(json!({"type":"message", "id":id, "role":"user", "content":text})).unwrap())
}

#[tokio::test]
async fn regression_database_rejects_duplicate_items_and_rolls_back_all_batches() {
    use agentic_core::storage::StorageError;

    let store = test_store().await;
    let conversation = store
        .create_with_metadata_and_items(
            Some("default_tenant"),
            None,
            vec![message_with_id("msg_existing", "original")],
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
        let rows = store
            .list_items(
                "default_tenant",
                &conversation.conversation_id,
                300,
                None,
                ItemOrder::Asc,
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "failed insertion must leave the history unchanged");
        assert_eq!(rows[0].id, "msg_existing");
        assert_eq!(rows[0].as_inout().unwrap(), message_with_id("msg_existing", "original"));
    }
    let created = store
        .create_items(
            "default_tenant",
            &conversation.conversation_id,
            vec![message_with_id("msg_new_0", "retry")],
        )
        .await
        .unwrap();
    assert_eq!(created[0].seq, Some(1));
}

#[tokio::test]
async fn regression_duplicate_initial_items_roll_back_conversation_creation() {
    let store = test_store().await;
    let error = store
        .create_with_metadata_and_items(
            Some("default_tenant"),
            None,
            vec![
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
        .create_with_metadata_and_items(Some("default_tenant"), None, vec![])
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
    let snapshot = store.rehydrate_snapshot(&conversation.conversation_id).await.unwrap();
    assert_eq!(snapshot.items, vec![created[0].as_inout().unwrap()]);
    assert_eq!(snapshot.version.last_sequence, Some(0));
    assert_eq!(snapshot.version.revision, 1);
}

#[path = "conversations/item_references.rs"]
mod item_references;
