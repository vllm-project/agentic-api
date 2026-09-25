//! Reused public item IDs must retain independent history positions and snapshots.

use super::{message_with_id, test_store};
use agentic_core::storage::{ResponseMetadata, StorageError};
use agentic_core::types::conversations::ItemOrder;

#[tokio::test]
async fn reused_item_occurrences_preserve_content_order_and_response_snapshot() {
    let store = test_store().await;
    let source = store
        .create_with_metadata_and_items(
            Some("tenant"),
            None,
            vec![message_with_id("msg_original", "original content")],
        )
        .await
        .unwrap();
    let target = store
        .create_with_metadata_and_items(Some("tenant"), None, vec![])
        .await
        .unwrap();
    // Span SQL batches to exercise the boundary between existing history and
    // repeated references arriving together in one request.
    let refs = vec![message_with_id("msg_original", "ignored replacement"); 200];
    let added = store
        .create_items("tenant", &target.conversation_id, refs)
        .await
        .unwrap();
    assert_eq!(added.len(), 200);
    assert_eq!(
        added
            .iter()
            .map(|item| &item.id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        200
    );
    for (index, item) in added.iter().enumerate() {
        assert_eq!(item.public_id(), "msg_original");
        assert_eq!(item.seq, Some(i64::try_from(index).unwrap()));
        assert_eq!(
            item.as_inout().unwrap(),
            message_with_id("msg_original", "original content")
        );
    }
    let listed = store
        .list_items("tenant", &target.conversation_id, 300, None, ItemOrder::Asc)
        .await
        .unwrap();
    assert_eq!(
        listed.iter().map(|item| &item.id).collect::<Vec<_>>(),
        added.iter().map(|item| &item.id).collect::<Vec<_>>()
    );
    let retrieved = store
        .retrieve_item("tenant", &target.conversation_id, "msg_original")
        .await
        .unwrap();
    assert_eq!(retrieved.public_id(), "msg_original");
    assert_eq!(retrieved.as_inout(), added[0].as_inout());
    let error = store
        .create_items(
            "tenant",
            &target.conversation_id,
            vec![message_with_id("msg_original", "again")],
        )
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::ItemAlreadyInConversation));
    store
        .persist(
            &target.conversation_id,
            "resp_references",
            None,
            vec![],
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
    let before = store.rehydrate(&target.conversation_id).await.unwrap();
    store
        .delete_item("tenant", &target.conversation_id, "msg_original")
        .await
        .unwrap();
    assert!(store.rehydrate(&target.conversation_id).await.unwrap().is_empty());
    assert_eq!(
        store.rehydrate(&source.conversation_id).await.unwrap(),
        vec![message_with_id("msg_original", "original content")]
    );
    let response = agentic_core::storage::models::response::get(store.pool().unwrap(), "resp_references")
        .await
        .unwrap()
        .unwrap();
    let ids = response.history_item_ids_vec().unwrap();
    let snapshot = agentic_core::storage::models::item::get_items(store.pool().unwrap(), &ids)
        .await
        .unwrap();
    assert_eq!(snapshot.len(), before.len());
    assert!(snapshot.iter().all(|item| item.as_inout().as_ref() == before.first()));
}

#[tokio::test]
async fn reused_item_cannot_read_another_tenants_content() {
    let store = test_store().await;
    store
        .create_with_metadata_and_items(Some("owner"), None, vec![message_with_id("msg_private", "private")])
        .await
        .unwrap();
    let target = store
        .create_with_metadata_and_items(Some("other"), None, vec![])
        .await
        .unwrap();
    assert!(
        store
            .create_items(
                "other",
                &target.conversation_id,
                vec![message_with_id("msg_private", "replacement")]
            )
            .await
            .is_err()
    );
    assert!(store.rehydrate(&target.conversation_id).await.unwrap().is_empty());
}
