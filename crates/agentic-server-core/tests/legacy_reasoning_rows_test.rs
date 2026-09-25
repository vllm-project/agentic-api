//! Reasoning rows written before typed reasoning stay readable and continuable.

mod support;

use std::sync::Arc;

use agentic_core::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler};
use agentic_core::storage::{ConversationStore, InOutItem, ResponseMetadata, ResponseStore};
use agentic_core::types::ReasoningStatus;
use agentic_core::types::io::{InputItem, ReasoningOutput, ReasoningTextContent, ReasoningTextKind};

/// Persist one typed reasoning item, then rewrite its row to a shape an earlier release stored.
async fn stored_legacy_row(legacy: serde_json::Value) -> (Arc<agentic_core::storage::DbPool>, ResponseStore) {
    let pool = support::setup_pool().await;
    let store = ResponseStore::new(Arc::clone(&pool));
    let mut reasoning = ReasoningOutput::new("rs_legacy");
    reasoning.content.push(ReasoningTextContent::new("placeholder"));
    store
        .persist(
            "resp_legacy",
            None,
            vec![InOutItem::Input(InputItem::Reasoning(reasoning))],
            &ResponseMetadata {
                model: "Qwen/Qwen3-30B-A3B-FP8".into(),
                ..ResponseMetadata::default()
            },
        )
        .await
        .unwrap();
    let (data,): (String,) = sqlx::query_as("SELECT data FROM items")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    let mut data: serde_json::Value = serde_json::from_str(&data).unwrap();
    for (key, value) in legacy.as_object().unwrap() {
        data[key] = value.clone();
    }
    sqlx::query("UPDATE items SET data = $1")
        .bind(data.to_string())
        .execute(pool.as_ref())
        .await
        .unwrap();
    (pool, store)
}

fn only_reasoning(items: Vec<InOutItem>) -> ReasoningOutput {
    match InOutItem::into_input_items(items).pop() {
        Some(InputItem::Reasoning(reasoning)) => reasoning,
        other => panic!("expected one reasoning item, got {other:?}"),
    }
}

#[tokio::test]
async fn legacy_rows_keep_every_field_that_still_decodes() {
    let (_pool, store) = stored_legacy_row(serde_json::json!({
        "content": [{"type": "unexpected_provider_type", "text": "keep this"}],
        "summary": [{"type": "summary_text", "text": "kept summary"}, {"text": "untyped summary"}],
        "encrypted_content": {"ciphertext": "untyped state"},
        "status": "failed"
    }))
    .await;
    let reasoning = only_reasoning(store.rehydrate("resp_legacy").await.unwrap());
    assert_eq!(reasoning.content.len(), 1);
    assert_eq!(reasoning.content[0].type_, ReasoningTextKind::ReasoningText);
    assert_eq!(reasoning.content[0].text, "keep this");
    assert_eq!(reasoning.summary.len(), 1);
    assert_eq!(reasoning.summary[0].text, "kept summary");
    assert!(reasoning.encrypted_content.is_none());
    assert!(reasoning.status.is_none());

    let (_pool, store) = stored_legacy_row(serde_json::json!({
        "content": [{"type": "reasoning_text", "text": "plaintext"}],
        "summary": [{"text": "untyped summary"}],
        "encrypted_content": "opaque string state",
        "status": "completed"
    }))
    .await;
    let reasoning = only_reasoning(store.rehydrate("resp_legacy").await.unwrap());
    assert!(reasoning.summary.is_empty());
    assert_eq!(
        reasoning
            .encrypted_content
            .as_ref()
            .map(agentic_core::types::OpaqueReasoning::as_str),
        Some("opaque string state")
    );
    assert_eq!(reasoning.status, Some(ReasoningStatus::Completed));
}

#[tokio::test]
async fn legacy_rows_remain_continuable_on_the_default_path() {
    let cassette = support::load_cassette(&format!(
        "{}/tests/cassettes/reasoning/responses/reasoning-single-Qwen-Qwen3-30B-A3B-FP8-nonstreaming.yaml",
        env!("CARGO_MANIFEST_DIR")
    ));
    for legacy in [
        serde_json::json!({"content": [{"type": "unexpected_provider_type", "text": "plaintext for vLLM"}]}),
        serde_json::json!({
            "content": [{"type": "reasoning_text", "text": "plaintext for vLLM"}],
            "encrypted_content": {"ciphertext": "untyped state"}
        }),
        serde_json::json!({"content": [{"type": "reasoning_text", "text": "plaintext for vLLM"}], "status": "failed"}),
    ] {
        let (pool, _store) = stored_legacy_row(legacy.clone()).await;
        let server = support::MockServer::start_deque(vec![support::MockResponse::from_turn(&cassette.turns[0])]).await;
        let exec_ctx = Arc::new(ExecutionContext::new(
            ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
            ResponseHandler::new(ResponseStore::new(Arc::clone(&pool))),
            Arc::new(reqwest::Client::new()),
            server.url().to_string(),
        ));
        let followup = support::make_request("continue", false, false, Some("resp_legacy".into()), None);
        let response = support::unwrap_blocking(ExecuteRequest::new(followup, exec_ctx).run().await.unwrap());
        assert_eq!(response.status, "completed", "{legacy}");
        let sent = server.request_bodies().await;
        let replayed = &sent[0]["input"][0];
        assert_eq!(replayed["type"], "reasoning", "{legacy}");
        assert_eq!(replayed["content"][0]["type"], "reasoning_text", "{legacy}");
        assert_eq!(replayed["content"][0]["text"], "plaintext for vLLM", "{legacy}");
        assert!(!sent[0].to_string().contains("untyped state"), "{legacy}");
    }
}
