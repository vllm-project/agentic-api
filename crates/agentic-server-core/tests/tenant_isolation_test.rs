mod support;

use agentic_core::storage::ConversationStore;
use serde_json::json;

fn metadata(value: serde_json::Value) -> agentic_core::types::conversations::ConversationMetadata {
    serde_json::from_value(value).unwrap()
}

use support::setup_pool;

#[tokio::test]
async fn test_conversation_tenant_isolation() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    // Create conversation for tenant A with metadata
    let conv_a = store
        .create_with_metadata_and_items(
            Some("tenant_a"),
            Some(metadata(json!({"user": "alice", "project": "test"}))),
            vec![],
        )
        .await
        .expect("create for tenant_a failed");

    assert!(conv_a.conversation_id.starts_with("conv_"));

    // Tenant A can retrieve it
    let retrieved = store
        .retrieve("tenant_a", &conv_a.conversation_id)
        .await
        .expect("tenant_a should retrieve own conversation");

    assert_eq!(retrieved.conversation_id, conv_a.conversation_id);

    // Tenant B CANNOT retrieve it (cross-tenant access blocked)
    let result = store.retrieve("tenant_b", &conv_a.conversation_id).await;
    assert!(result.is_err(), "tenant_b should NOT retrieve tenant_a's conversation");

    // Verify it's a NotFound error
    match result {
        Err(agentic_core::storage::StorageError::NotFound { .. }) => {
            // Expected
        }
        other => panic!("Expected NotFound error, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_conversation_metadata_update_with_tenant() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    // Create conversation
    let conv = store
        .create_with_metadata_and_items(Some("tenant_a"), Some(metadata(json!({"status": "draft"}))), vec![])
        .await
        .expect("create failed");

    // Update metadata
    let updated = store
        .update_metadata(
            "tenant_a",
            &conv.conversation_id,
            metadata(json!({"status": "active", "updated": "true"})),
        )
        .await
        .expect("update_metadata failed");

    assert_eq!(updated.conversation_id, conv.conversation_id);

    // Verify updated metadata
    let retrieved = store
        .retrieve("tenant_a", &conv.conversation_id)
        .await
        .expect("retrieve failed");

    assert_eq!(retrieved.conversation_id, conv.conversation_id);
}

#[tokio::test]
async fn test_conversation_delete_with_tenant_scoping() {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    // Create conversation for tenant A
    let conv = store
        .create_with_metadata_and_items(Some("tenant_a"), Some(metadata(json!({"temp": "true"}))), vec![])
        .await
        .expect("create failed");

    // Tenant B cannot delete tenant A's conversation
    let delete_result = store.delete("tenant_b", &conv.conversation_id).await;
    assert!(
        delete_result.is_err(),
        "tenant_b should NOT delete tenant_a's conversation"
    );

    // Verify conversation still exists for tenant A
    let still_exists = store.retrieve("tenant_a", &conv.conversation_id).await;
    assert!(
        still_exists.is_ok(),
        "conversation should still exist after failed delete"
    );

    // Tenant A can delete their own conversation
    let delete_result = store.delete("tenant_a", &conv.conversation_id).await;
    assert!(delete_result.is_ok(), "tenant_a should delete own conversation");

    // Verify conversation is gone
    let should_be_gone = store.retrieve("tenant_a", &conv.conversation_id).await;
    assert!(should_be_gone.is_err(), "conversation should be deleted");
}

#[tokio::test]
async fn test_conversation_create_with_initial_items() {
    use agentic_core::storage::InOutItem;
    use agentic_core::types::io::{InputItem, InputMessage, InputMessageContent};

    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);

    let initial_items = vec![InOutItem::Input(InputItem::Message(InputMessage {
        role: "user".to_string(),
        content: InputMessageContent::Text("Hello, world!".to_string()),
        ..Default::default()
    }))];

    // Create conversation with initial items
    let conv = store
        .create_with_metadata_and_items(
            Some("tenant_a"),
            Some(metadata(json!({"test": "initial_items"}))),
            initial_items,
        )
        .await
        .expect("create with items failed");

    assert!(conv.conversation_id.starts_with("conv_"));

    // Rehydrate and verify items are present
    let items = store.rehydrate(&conv.conversation_id).await.expect("rehydrate failed");

    assert_eq!(items.len(), 1, "should have 1 initial item");
}
