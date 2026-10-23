//! Response CRUD uses existing history rows and foreign-key behavior.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agentic_core::storage::models::{conversation as conversation_model, response as response_model};
use agentic_core::storage::{
    ConversationStore, DbPool, ResponseMetadata, ResponseStore, StorageError, create_pool_with_schema,
};
use agentic_core::types::io::InOutItem;
use serde_json::json;

static COUNTER: AtomicU64 = AtomicU64::new(0);

async fn pool() -> Arc<DbPool> {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let url = format!("sqlite:file:response_deletion_{id}?mode=memory&cache=shared");
    create_pool_with_schema(Some(&url)).await.unwrap()
}

fn message(text: &str) -> InOutItem {
    InOutItem::Input(serde_json::from_value(json!({"type": "message", "role": "user", "content": text})).unwrap())
}

fn answer(id: &str) -> InOutItem {
    InOutItem::Output(
        serde_json::from_value(
            json!({"type": "message", "id": id, "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": "answer", "annotations": []}]}),
        )
        .unwrap(),
    )
}

async fn item_count(pool: &DbPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn turn(store: &ResponseStore, id: &str, previous: Option<&str>) {
    store
        .persist(
            id,
            previous,
            vec![message(id), answer(&format!("msg_{id}"))],
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn deleting_a_response_removes_the_resource_and_keeps_history_rows() {
    let pool = pool().await;
    let store = ResponseStore::new(Arc::clone(&pool));
    turn(&store, "resp_only", None).await;
    assert_eq!(item_count(&pool).await, 2);

    store.delete("resp_only").await.unwrap();
    assert_eq!(item_count(&pool).await, 2);
    assert!(store.get("resp_only").await.unwrap_err().is_not_found());
    assert!(store.get_items("resp_only").await.unwrap_err().is_not_found());
    assert!(store.delete("resp_only").await.unwrap_err().is_not_found());
}

#[tokio::test]
async fn response_deletion_uses_the_callers_transaction() {
    let pool = pool().await;
    let store = ResponseStore::new(Arc::clone(&pool));
    turn(&store, "resp_parent", None).await;
    turn(&store, "resp_child", Some("resp_parent")).await;
    let original = store.get("resp_child").await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    assert!(response_model::delete_in_tx(&mut tx, "resp_parent").await.unwrap());
    assert!(
        response_model::get_in_tx(&mut tx, "resp_parent")
            .await
            .unwrap()
            .is_none()
    );
    let child = response_model::get_in_tx(&mut tx, "resp_child").await.unwrap().unwrap();
    assert!(child.previous_response_id.is_none());
    tx.rollback().await.unwrap();

    assert!(store.get("resp_parent").await.is_ok());
    let restored = store.get("resp_child").await.unwrap();
    assert_eq!(restored.previous_response_id, original.previous_response_id);
    assert_eq!(restored.history_item_ids, original.history_item_ids);
    assert_eq!(item_count(&pool).await, 4);
}

#[tokio::test]
async fn deleting_a_parent_removes_its_context_and_preserves_child_history_order() {
    let pool = pool().await;
    let store = ResponseStore::new(Arc::clone(&pool));
    // `resp_a` and `resp_b` are siblings: both inherit the parent's two rows.
    turn(&store, "resp_parent", None).await;
    turn(&store, "resp_a", Some("resp_parent")).await;
    turn(&store, "resp_b", Some("resp_parent")).await;
    assert_eq!(item_count(&pool).await, 6);

    // OpenAI removes the deleted parent's context from descendant listings and continuations.
    store.delete("resp_parent").await.unwrap();
    assert_eq!(item_count(&pool).await, 6);
    // Deletion only removes response records, preserving all shared item rows.
    store.delete("resp_a").await.unwrap();
    assert_eq!(item_count(&pool).await, 6);
    let rows = store.get_items("resp_b").await.unwrap();
    let ids = store.get("resp_b").await.unwrap().history_item_ids;
    assert_eq!(
        rows.iter().map(|row| &row.id).collect::<Vec<_>>(),
        ids.iter().collect::<Vec<_>>()
    );
    let history = store.rehydrate("resp_b").await.unwrap();
    assert_eq!(history, vec![message("resp_b"), answer("msg_resp_b")]);
    store.delete("resp_b").await.unwrap();
    assert_eq!(item_count(&pool).await, 6);
}

#[tokio::test]
async fn deleting_a_conversation_latest_response_clears_its_pointer_and_keeps_items() {
    let pool = pool().await;
    let conversations = ConversationStore::new(Arc::clone(&pool));
    let responses = ResponseStore::new(Arc::clone(&pool));
    let conversation = conversations
        .create_with_metadata_and_items(Some("default_tenant"), None, vec![])
        .await
        .unwrap();
    let id = conversation.conversation_id.as_str();
    for (response, text) in [("resp_first", "first"), ("resp_second", "second")] {
        conversations
            .persist(
                id,
                response,
                None,
                vec![message(text), answer(&format!("msg_{response}"))],
                &ResponseMetadata::default(),
            )
            .await
            .unwrap();
    }
    let latest = |pool: Arc<DbPool>| async move {
        conversation_model::get(&pool, id)
            .await
            .unwrap()
            .unwrap()
            .latest_response_id
    };
    assert_eq!(latest(Arc::clone(&pool)).await.as_deref(), Some("resp_second"));

    responses.delete("resp_second").await.unwrap();
    assert_eq!(
        latest(Arc::clone(&pool)).await.as_deref(),
        None,
        "the existing foreign key clears the deleted pointer"
    );
    assert_eq!(
        item_count(&pool).await,
        4,
        "conversation items stay with the conversation"
    );

    responses.delete("resp_first").await.unwrap();
    assert_eq!(latest(Arc::clone(&pool)).await, None);
    assert_eq!(item_count(&pool).await, 4);
}

#[tokio::test]
async fn item_lookup_distinguishes_missing_empty_and_invalid_history() {
    let pool = pool().await;
    let store = ResponseStore::new(Arc::clone(&pool));
    assert!(store.get_items("resp_missing").await.unwrap_err().is_not_found());
    store
        .persist("resp_empty", None, vec![], &ResponseMetadata::default())
        .await
        .unwrap();
    assert!(store.get_items("resp_empty").await.unwrap().is_empty());
    sqlx::query("UPDATE responses SET history_item_ids = NULL WHERE id = 'resp_empty'")
        .execute(pool.as_ref())
        .await
        .unwrap();
    assert!(store.get_items("resp_empty").await.unwrap().is_empty());
    for invalid in ["not-json", "null", "{}"] {
        sqlx::query("UPDATE responses SET history_item_ids = $1 WHERE id = 'resp_empty'")
            .bind(invalid)
            .execute(pool.as_ref())
            .await
            .unwrap();
        assert!(matches!(
            store.get_items("resp_empty").await,
            Err(StorageError::InvalidResponseHistory { .. })
        ));
    }
    sqlx::query("UPDATE responses SET history_item_ids = '[\"item_missing\"]' WHERE id = 'resp_empty'")
        .execute(pool.as_ref())
        .await
        .unwrap();
    assert!(matches!(
        store.get_items("resp_empty").await,
        Err(StorageError::InvalidHistoryItem { .. })
    ));
}
