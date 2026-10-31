//! Reused public item IDs must retain independent history positions and snapshots.

use super::{message_with_id, test_store};
use agentic_core::storage::{ResponseMetadata, ResponseStore, StorageError};
use agentic_core::types::io::ItemOrder;
use std::sync::Arc;

/// A response history row stores its content under a `item_…` row ID and keeps the public ID in
/// `items.public_id`. Adding that public ID to a conversation must resolve the stored row.
#[tokio::test]
async fn response_history_public_ids_stay_independent_across_conversation_batches() {
    let store = test_store().await;
    let responses = ResponseStore::new(Arc::new(store.pool().unwrap().clone()));
    let original = message_with_id("msg_history", "stored content");
    responses
        .persist(
            "resp_history",
            None,
            vec![original.clone()],
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
    let target = store
        .create_with_metadata_and_items(Some("default_tenant"), None, vec![])
        .await
        .unwrap();
    // Span SQL batches and repeat one public ID in a single request.
    let refs = vec![message_with_id("msg_history", "submitted replacement"); 200];
    let added = store
        .create_items("default_tenant", &target.conversation_id, refs)
        .await
        .unwrap();
    assert_eq!(added.len(), 200);
    // Each occurrence is its own list position; all reference the stored item.
    assert_eq!(
        added
            .iter()
            .map(|item| item.seq)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        200
    );
    for (index, item) in added.iter().enumerate() {
        assert_eq!(item.public_id(), "msg_history");
        assert_eq!(item.seq, Some(i64::try_from(index).unwrap()));
        assert_eq!(item.as_inout().unwrap(), original);
    }
    assert_eq!(
        store.rehydrate(&target.conversation_id).await.unwrap(),
        vec![original; 200]
    );
    let listed = store
        .list_items("default_tenant", &target.conversation_id, 300, None, ItemOrder::Asc)
        .await
        .unwrap();
    assert_eq!(
        listed.iter().map(|item| &item.id).collect::<Vec<_>>(),
        added.iter().map(|item| &item.id).collect::<Vec<_>>()
    );
    assert_eq!(responses.rehydrate("resp_history").await.unwrap().len(), 1);
}

#[tokio::test]
async fn repeated_response_history_public_ids_keep_every_occurrence_and_order() {
    let store = test_store().await;
    let responses = ResponseStore::new(Arc::new(store.pool().unwrap().clone()));
    let first = message_with_id("msg_first", "first content");
    let second = message_with_id("msg_second", "second content");
    responses
        .persist(
            "resp_two_items",
            None,
            vec![first.clone(), second.clone()],
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
    let target = store
        .create_with_metadata_and_items(Some("default_tenant"), None, vec![])
        .await
        .unwrap();
    let added = store
        .create_items(
            "default_tenant",
            &target.conversation_id,
            vec![
                message_with_id("msg_first", "replacement one"),
                message_with_id("msg_second", "replacement two"),
                message_with_id("msg_first", "replacement three"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        added.iter().map(agentic_core::DbItem::public_id).collect::<Vec<_>>(),
        vec!["msg_first", "msg_second", "msg_first"]
    );
    assert_eq!(
        store.rehydrate(&target.conversation_id).await.unwrap(),
        vec![first.clone(), second.clone(), first.clone()]
    );
    let retrieved = store
        .retrieve_item("default_tenant", &target.conversation_id, "msg_first")
        .await
        .unwrap();
    assert_eq!(retrieved.public_id(), "msg_first");
    assert_eq!(retrieved.as_inout(), Some(first.clone()));
    store
        .delete_item("default_tenant", &target.conversation_id, "msg_first")
        .await
        .unwrap();
    assert_eq!(
        store.rehydrate(&target.conversation_id).await.unwrap(),
        vec![second.clone()]
    );
    assert_eq!(
        responses.rehydrate("resp_two_items").await.unwrap(),
        vec![first, second]
    );
}

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
    // Each occurrence is its own list position; all reference the stored item.
    assert_eq!(
        added
            .iter()
            .map(|item| item.seq)
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
    let snapshot = ResponseStore::new(Arc::new(store.pool().unwrap().clone()))
        .get_items("resp_references")
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
    // Another tenant's item is invisible, so the ID creates this tenant's own item.
    store
        .create_items(
            "other",
            &target.conversation_id,
            vec![message_with_id("msg_private", "replacement")],
        )
        .await
        .unwrap();
    assert_eq!(
        store.rehydrate(&target.conversation_id).await.unwrap(),
        vec![message_with_id("msg_private", "replacement")]
    );
}

/// Several stored rows can share one public ID. Resolving that public ID must still add exactly one
/// list entry per submitted occurrence.
#[tokio::test]
async fn public_id_matching_many_rows_inserts_one_row_per_occurrence() {
    let store = test_store().await;
    let source = store
        .create_with_metadata_and_items(
            Some("tenant"),
            None,
            vec![message_with_id("msg_many", "original content")],
        )
        .await
        .unwrap();
    let first_target = store
        .create_with_metadata_and_items(Some("tenant"), None, vec![])
        .await
        .unwrap();
    store
        .create_items(
            "tenant",
            &first_target.conversation_id,
            vec![message_with_id("msg_many", "ignored replacement")],
        )
        .await
        .unwrap();
    let second_target = store
        .create_with_metadata_and_items(Some("tenant"), None, vec![])
        .await
        .unwrap();
    let added = store
        .create_items(
            "tenant",
            &second_target.conversation_id,
            vec![
                message_with_id("msg_many", "ignored replacement"),
                message_with_id("msg_many", "ignored replacement"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(added.len(), 2);
    assert_ne!(added[0].seq, added[1].seq);
    for item in &added {
        assert_eq!(item.public_id(), "msg_many");
        assert_eq!(
            item.as_inout().unwrap(),
            message_with_id("msg_many", "original content")
        );
    }
    assert_eq!(
        store.rehydrate(&second_target.conversation_id).await.unwrap(),
        vec![
            message_with_id("msg_many", "original content"),
            message_with_id("msg_many", "original content")
        ]
    );
    assert_eq!(store.rehydrate(&source.conversation_id).await.unwrap().len(), 1);
}
