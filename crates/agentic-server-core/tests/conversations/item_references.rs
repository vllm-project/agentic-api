//! Reused public item IDs must retain independent history positions and snapshots.

use super::{conversation_rows, message_with_id, persist_turn, response_rows, test_store};
use agentic_core::storage::{ResponseMetadata, ResponseStore, StorageError};
use agentic_core::types::conversations::ConversationMetadata;
use agentic_core::types::io::{InOutItem, ItemOrder};
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
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .unwrap();
    // Span SQL batches and repeat one public ID in a single request.
    let refs = vec![message_with_id("msg_history", "submitted replacement"); 200];
    let added = store
        .create_items("default_tenant", &target.conversation_id, refs)
        .await
        .unwrap();
    assert_eq!(added.len(), 200);
    assert!(added.iter().all(|item| item.id() == Some("msg_history")));
    let rows = conversation_rows(&store, &target.conversation_id).await;
    assert_eq!(
        rows.iter()
            .map(|row| &row.id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        200
    );
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.public_id(), "msg_history");
        assert_eq!(row.seq, Some(i64::try_from(index).unwrap()));
        assert_eq!(InOutItem::try_from(row).unwrap(), original);
    }
    assert_eq!(
        store.rehydrate(&target.conversation_id).await.unwrap().items,
        vec![original; 200]
    );
    let listed = store
        .list_items("default_tenant", &target.conversation_id, 300, None, ItemOrder::Asc)
        .await
        .unwrap();
    assert_eq!(
        listed.iter().map(InOutItem::id).collect::<Vec<_>>(),
        added.iter().map(InOutItem::id).collect::<Vec<_>>()
    );
    assert_eq!(responses.rehydrate("resp_history").await.unwrap().1.len(), 1);
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
        .create("default_tenant", &ConversationMetadata::new(), &[])
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
        added.iter().map(InOutItem::id).collect::<Vec<_>>(),
        vec![Some("msg_first"), Some("msg_second"), Some("msg_first")]
    );
    assert_eq!(
        store.rehydrate(&target.conversation_id).await.unwrap().items,
        vec![first.clone(), second.clone(), first.clone()]
    );
    let retrieved = store
        .retrieve_item("default_tenant", &target.conversation_id, "msg_first")
        .await
        .unwrap();
    assert_eq!(retrieved, added[0]);
    store
        .delete_item("default_tenant", &target.conversation_id, "msg_first")
        .await
        .unwrap();
    assert_eq!(
        store.rehydrate(&target.conversation_id).await.unwrap().items,
        vec![second.clone()]
    );
    assert_eq!(
        responses.rehydrate("resp_two_items").await.unwrap().1,
        vec![first, second]
    );
}

#[tokio::test]
async fn reused_item_occurrences_preserve_content_order_and_response_snapshot() {
    let store = test_store().await;
    let source = store
        .create(
            "tenant",
            &ConversationMetadata::new(),
            &[message_with_id("msg_original", "original content")],
        )
        .await
        .unwrap();
    let target = store.create("tenant", &ConversationMetadata::new(), &[]).await.unwrap();
    // Span SQL batches to exercise the boundary between existing history and
    // repeated references arriving together in one request.
    let refs = vec![message_with_id("msg_original", "ignored replacement"); 200];
    let added = store
        .create_items("tenant", &target.conversation_id, refs)
        .await
        .unwrap();
    assert_eq!(added.len(), 200);
    assert!(added.iter().all(|item| item.id() == Some("msg_original")));
    let rows = conversation_rows(&store, &target.conversation_id).await;
    assert_eq!(
        rows.iter()
            .map(|row| &row.id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        200
    );
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.public_id(), "msg_original");
        assert_eq!(row.seq, Some(i64::try_from(index).unwrap()));
        assert_eq!(
            InOutItem::try_from(row).unwrap(),
            message_with_id("msg_original", "original content")
        );
    }
    let listed = store
        .list_items("tenant", &target.conversation_id, 300, None, ItemOrder::Asc)
        .await
        .unwrap();
    assert_eq!(
        listed.iter().map(InOutItem::id).collect::<Vec<_>>(),
        added.iter().map(InOutItem::id).collect::<Vec<_>>()
    );
    let retrieved = store
        .retrieve_item("tenant", &target.conversation_id, "msg_original")
        .await
        .unwrap();
    assert_eq!(retrieved, added[0]);
    let error = store
        .create_items(
            "tenant",
            &target.conversation_id,
            vec![message_with_id("msg_original", "again")],
        )
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::ItemAlreadyInConversation));
    persist_turn(
        &store,
        &target.conversation_id,
        "resp_references",
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
    assert!(store.rehydrate(&target.conversation_id).await.unwrap().items.is_empty());
    assert_eq!(
        store.rehydrate(&source.conversation_id).await.unwrap().items,
        vec![message_with_id("msg_original", "original content")]
    );
    let snapshot = response_rows(&store, "resp_references").await;
    assert_eq!(snapshot.len(), before.items.len());
    assert!(
        snapshot
            .iter()
            .all(|row| InOutItem::try_from(row).ok().as_ref() == before.items.first())
    );
}

#[tokio::test]
async fn reused_item_cannot_read_another_tenants_content() {
    let store = test_store().await;
    store
        .create(
            "owner",
            &ConversationMetadata::new(),
            &[message_with_id("msg_private", "private")],
        )
        .await
        .unwrap();
    let target = store.create("other", &ConversationMetadata::new(), &[]).await.unwrap();
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
    assert!(store.rehydrate(&target.conversation_id).await.unwrap().items.is_empty());
}

/// Several stored rows can share one public ID: the original row and every reference to it.
/// Resolving that public ID must still insert exactly one row per submitted occurrence.
#[tokio::test]
async fn public_id_matching_many_rows_inserts_one_row_per_occurrence() {
    let store = test_store().await;
    let source = store
        .create(
            "tenant",
            &ConversationMetadata::new(),
            &[message_with_id("msg_many", "original content")],
        )
        .await
        .unwrap();
    let first_target = store.create("tenant", &ConversationMetadata::new(), &[]).await.unwrap();
    store
        .create_items(
            "tenant",
            &first_target.conversation_id,
            vec![message_with_id("msg_many", "ignored replacement")],
        )
        .await
        .unwrap();
    let second_target = store.create("tenant", &ConversationMetadata::new(), &[]).await.unwrap();
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
    let rows = conversation_rows(&store, &second_target.conversation_id).await;
    assert_ne!(rows[0].id, rows[1].id);
    for row in &rows {
        assert_eq!(row.public_id(), "msg_many");
        assert_eq!(
            InOutItem::try_from(row).unwrap(),
            message_with_id("msg_many", "original content")
        );
    }
    assert_eq!(
        store.rehydrate(&second_target.conversation_id).await.unwrap().items,
        vec![
            message_with_id("msg_many", "original content"),
            message_with_id("msg_many", "original content")
        ]
    );
    assert_eq!(store.rehydrate(&source.conversation_id).await.unwrap().items.len(), 1);
}
