//! Durable provenance is separate from item JSON, versioned and fail-closed.

mod support;

use std::sync::Arc;

use agentic_core::storage::{ConversationStore, InOutItem, ResponseMetadata, ResponseStore, StorageError};
use agentic_core::types::OpaqueReasoning;
use agentic_core::types::io::{InputItem, OutputItem, ReasoningOutput};
use agentic_core::types::reasoning_replay::{MAX_REASONING_PROVENANCE_BYTES, ReasoningProvenance};

const OPAQUE: &str = "  opaque/bytes+=\nnot-for-diagnostics";

fn item(id: &str, input: bool, provenance: Option<ReasoningProvenance>) -> InOutItem {
    let mut reasoning = ReasoningOutput::new(id);
    reasoning.encrypted_content = Some(OpaqueReasoning::try_from(OPAQUE.to_owned()).unwrap());
    reasoning.replay_provenance = provenance;
    if input {
        InOutItem::Input(InputItem::Reasoning(reasoning))
    } else {
        InOutItem::Output(OutputItem::Reasoning(reasoning))
    }
}

fn provider_provenance() -> ReasoningProvenance {
    serde_json::from_value(serde_json::json!({
        "version": "1", "source": {"origin": "upstream", "policy": "vllm_plaintext", "identity": ([7; 32].to_vec())}
    }))
    .unwrap()
}

#[tokio::test]
async fn both_stores_preserve_per_item_provenance_and_opaque_bytes_across_batches_and_branches() {
    let pool = support::setup_pool().await;
    let conversations = ConversationStore::new(Arc::clone(&pool));
    let responses = ResponseStore::new(Arc::clone(&pool));
    let conversation = conversations.create().await.unwrap();
    let metadata = ResponseMetadata::default();
    // More than one portable insert batch, mixing input/output and legacy origins.
    let batch = |prefix: &str| {
        (0..170)
            .map(|index| {
                item(
                    &format!("{prefix}_{index}"),
                    index % 2 == 0,
                    match index % 3 {
                        0 => None,
                        1 => Some(ReasoningProvenance::client_submitted()),
                        _ => Some(provider_provenance()),
                    },
                )
            })
            .collect::<Vec<_>>()
    };
    let items = batch("rs");
    conversations
        .persist(
            &conversation.conversation_id,
            "resp_root",
            None,
            items.clone(),
            &metadata,
        )
        .await
        .unwrap();
    assert_eq!(
        conversations.rehydrate(&conversation.conversation_id).await.unwrap(),
        items
    );
    assert_eq!(responses.rehydrate("resp_root").await.unwrap(), items);
    for child in ["resp_left", "resp_right"] {
        responses
            .persist(child, Some("resp_root"), Vec::new(), &metadata)
            .await
            .unwrap();
        assert_eq!(responses.rehydrate(child).await.unwrap(), items);
    }
    // Also exercise inserts without a conversation, not only shared parent rows.
    // Stored rows keep each item's public ID, so this batch needs its own IDs.
    let standalone = batch("rs_standalone");
    responses
        .persist("resp_standalone", None, standalone.clone(), &metadata)
        .await
        .unwrap();
    assert_eq!(responses.rehydrate("resp_standalone").await.unwrap(), standalone);
    let rows: Vec<(String, Option<String>)> = sqlx::query_as("SELECT data, reasoning_provenance FROM items")
        .fetch_all(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(rows.len(), 340, "branches reuse parent rows");
    for (data, provenance) in rows {
        assert!(!data.contains("replay_provenance"));
        assert!(!data.contains("\"source\""));
        if let Some(provenance) = provenance {
            assert!(provenance.len() <= MAX_REASONING_PROVENANCE_BYTES);
            assert!(!provenance.contains(OPAQUE));
        }
    }
}

#[tokio::test]
async fn malformed_provenance_fails_closed_in_both_stores_with_redacted_errors() {
    let pool = support::setup_pool().await;
    let conversations = ConversationStore::new(Arc::clone(&pool));
    let responses = ResponseStore::new(Arc::clone(&pool));
    let conversation = conversations.create().await.unwrap();
    conversations
        .persist(
            &conversation.conversation_id,
            "resp_root",
            None,
            vec![item("rs_1", false, None)],
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
    let oversized = " ".repeat(MAX_REASONING_PROVENANCE_BYTES + 1);
    for invalid in [
        "null",
        "{}",
        "[]",
        "not JSON",
        "{\"version\":\"2\"}",
        OPAQUE,
        &oversized,
    ] {
        sqlx::query("UPDATE items SET reasoning_provenance = $1")
            .bind(invalid)
            .execute(pool.as_ref())
            .await
            .unwrap();
        for error in [
            responses.rehydrate("resp_root").await.unwrap_err(),
            conversations
                .rehydrate(&conversation.conversation_id)
                .await
                .unwrap_err(),
        ] {
            assert!(matches!(error, StorageError::InvalidHistoryItem { .. }));
            assert!(!format!("{error:?}").contains(OPAQUE));
            assert!(!error.to_string().contains(OPAQUE));
            assert!(std::error::Error::source(&error).is_none());
        }
    }
    sqlx::query("UPDATE items SET reasoning_provenance = NULL")
        .execute(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(
        responses.rehydrate("resp_root").await.unwrap(),
        vec![item("rs_1", false, None)]
    );
    // A valid provenance envelope attached to the wrong item kind is corrupt too.
    sqlx::query("UPDATE items SET data = $1, reasoning_provenance = $2")
        .bind(r#"{"type":"message","role":"user","content":"hello"}"#)
        .bind(serde_json::to_string(&ReasoningProvenance::client_submitted()).unwrap())
        .execute(pool.as_ref())
        .await
        .unwrap();
    assert!(matches!(
        responses.rehydrate("resp_root").await,
        Err(StorageError::InvalidHistoryItem { .. })
    ));
    assert!(matches!(
        conversations.rehydrate(&conversation.conversation_id).await,
        Err(StorageError::InvalidHistoryItem { .. })
    ));
}
