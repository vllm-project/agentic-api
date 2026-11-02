//! Reasoning stored before typed reasoning stays readable and continuable. Shapes
//! earlier releases could not have stored still fail closed.

mod support;

use agentic_core::types::conversations::ConversationMetadata;
use std::sync::Arc;

use agentic_core::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ExecutorError, ResponseHandler};
use agentic_core::storage::{ConversationStore, ResponseMetadata, ResponseStore, StorageError};
use agentic_core::types::ReasoningStatus;
use agentic_core::types::io::InOutItem;
use agentic_core::types::io::{InputItem, OutputItem, ReasoningOutput, ReasoningTextContent, ReasoningTextKind};
use agentic_core::types::request_response::RequestPayload;
use serde_json::{Value, json};

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

fn recording(name: &str) -> support::Cassette {
    support::load_cassette(&format!(
        "{}/tests/cassettes/reasoning/responses/{name}-nonstreaming.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))
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
    let reasoning = only_reasoning(store.rehydrate("resp_legacy").await.unwrap().1);
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
        "status": "completed",
        "agent": {"agent_name": "/root/worker"}
    }))
    .await;
    let reasoning = only_reasoning(store.rehydrate("resp_legacy").await.unwrap().1);
    assert!(reasoning.summary.is_empty());
    assert_eq!(
        reasoning
            .encrypted_content
            .as_ref()
            .map(agentic_core::types::OpaqueReasoning::as_str),
        Some("opaque string state")
    );
    assert_eq!(reasoning.status, Some(ReasoningStatus::Completed));
    assert_eq!(
        reasoning.agent.as_ref().map(|agent| agent.agent_name.as_str()),
        Some("/root/worker")
    );
}

#[tokio::test]
async fn legacy_rows_remain_continuable_on_the_default_path() {
    let cassette = recording("reasoning-single-Qwen-Qwen3-30B-A3B-FP8");
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

#[tokio::test]
async fn rows_earlier_releases_could_not_store_fail_closed() {
    for legacy in [
        json!({"content": [{"text": "part without a type"}]}),
        json!({"content": [{"type": 7, "text": "non-string type"}]}),
        json!({"content": "not an array"}),
        json!({"status": 5}),
        json!({"agent": "/root/worker"}),
    ] {
        let (_pool, store) = stored_legacy_row(legacy.clone()).await;
        let error = store.rehydrate("resp_legacy").await.unwrap_err();
        assert!(
            matches!(error, StorageError::InvalidHistoryItem { .. }),
            "{legacy}: {error:?}"
        );
    }
}

/// One stored response and the request that continues it.
struct StoredTurn {
    server: support::MockServer,
    pool: Arc<agentic_core::storage::DbPool>,
    exec_ctx: Arc<ExecutionContext>,
    response_id: String,
    /// Continues the stored response by `previous_response_id`, or by `conversation`.
    followup: RequestPayload,
}

/// Store one response, optionally as a conversation turn. The mock serves a second
/// recording for a continuation that reaches the model.
async fn stored_turn(in_conversation: bool) -> StoredTurn {
    let pool = support::setup_pool().await;
    let server = support::MockServer::start_deque(vec![
        support::MockResponse::from_turn(&recording("reasoning-single-Qwen-Qwen3-30B-A3B-FP8").turns[0]),
        support::MockResponse::from_turn(&recording("reasoning-single-openai-gpt-oss-20b").turns[0]),
    ])
    .await;
    let conversations = ConversationStore::new(Arc::clone(&pool));
    let conversation_id = if in_conversation {
        Some(
            conversations
                .create("default_tenant", &ConversationMetadata::new(), &[])
                .await
                .unwrap()
                .conversation_id,
        )
    } else {
        None
    };
    let exec_ctx = Arc::new(ExecutionContext::new(
        ConversationHandler::new(conversations),
        ResponseHandler::new(ResponseStore::new(Arc::clone(&pool))),
        Arc::new(reqwest::Client::new()),
        server.url().to_string(),
    ));
    let request = support::make_request("hello", true, false, None, conversation_id.clone());
    let stored = support::unwrap_blocking(ExecuteRequest::new(request, Arc::clone(&exec_ctx)).run().await.unwrap());
    let followup = match conversation_id {
        Some(conversation_id) => support::make_request("continue", false, false, None, Some(conversation_id)),
        None => support::make_request("continue", false, false, Some(stored.id.clone()), None),
    };
    StoredTurn {
        server,
        pool,
        exec_ctx,
        response_id: stored.id,
        followup,
    }
}

/// Rewrite every stored reasoning history row with the fields of `legacy`.
async fn rewrite_reasoning_rows(turn: &StoredTurn, legacy: &Value) {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT id, data FROM items")
        .fetch_all(turn.pool.as_ref())
        .await
        .unwrap();
    let mut rewritten = 0;
    for (id, data) in rows {
        let mut data: Value = serde_json::from_str(&data).unwrap();
        if data["type"] != "reasoning" {
            continue;
        }
        data.as_object_mut()
            .unwrap()
            .extend(legacy.as_object().unwrap().clone());
        sqlx::query("UPDATE items SET data = $1 WHERE id = $2")
            .bind(data.to_string())
            .bind(&id)
            .execute(turn.pool.as_ref())
            .await
            .unwrap();
        rewritten += 1;
    }
    assert!(rewritten > 0, "stored history has reasoning");
}

/// Store one response, optionally as a conversation turn, then rewrite the
/// reasoning in its stored snapshot with the fields of `legacy`.
async fn stored_legacy_snapshot(legacy: &Value, in_conversation: bool) -> StoredTurn {
    let turn = stored_turn(in_conversation).await;
    let (metadata,): (String,) = sqlx::query_as("SELECT metadata FROM responses WHERE id = $1")
        .bind(&turn.response_id)
        .fetch_one(turn.pool.as_ref())
        .await
        .unwrap();
    let mut metadata: Value = serde_json::from_str(&metadata).unwrap();
    let reasoning = metadata["response_snapshot"]["output"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|item| item["type"] == "reasoning")
        .expect("stored snapshot has reasoning");
    reasoning
        .as_object_mut()
        .unwrap()
        .extend(legacy.as_object().unwrap().clone());
    sqlx::query("UPDATE responses SET metadata = $1 WHERE id = $2")
        .bind(metadata.to_string())
        .bind(&turn.response_id)
        .execute(turn.pool.as_ref())
        .await
        .unwrap();
    turn
}

/// vLLM can't replay opaque state without plaintext, so such a continuation is
/// rejected before inference. When an earlier release stored that state in a shape
/// the typed schema drops, the continuation must still never reach the model.
#[tokio::test]
async fn opaque_only_reasoning_never_reaches_the_model() {
    for in_conversation in [false, true] {
        for state in [json!("string state"), json!({"ciphertext": "object state"})] {
            let turn = stored_turn(in_conversation).await;
            rewrite_reasoning_rows(&turn, &json!({"content": [], "encrypted_content": state})).await;
            let Err(error) = ExecuteRequest::new(turn.followup, Arc::clone(&turn.exec_ctx))
                .run()
                .await
            else {
                panic!("in_conversation={in_conversation} {state}: continuation reached the model");
            };
            let failed_closed = if state.is_string() {
                matches!(error, ExecutorError::InvalidRequest(_))
            } else {
                matches!(error, ExecutorError::Storage(StorageError::InvalidHistoryItem { .. }))
            };
            assert!(failed_closed, "in_conversation={in_conversation} {state}: {error:?}");
            assert_eq!(
                turn.server.request_bodies().await.len(),
                1,
                "only the stored turn reached the model"
            );
        }
    }
}

#[tokio::test]
async fn legacy_snapshots_remain_retrievable_and_continuable() {
    for in_conversation in [false, true] {
        let stored = stored_legacy_snapshot(
            &json!({
                "content": [{"type": "unexpected_provider_type", "text": "snapshot plaintext"}],
                "summary": [{"type": "summary_text", "text": "kept summary"}, {"text": "untyped summary"}],
                "encrypted_content": {"ciphertext": "untyped state"},
                "status": "failed"
            }),
            in_conversation,
        )
        .await;
        let retrieved = stored
            .exec_ctx
            .resp_handler
            .get_snapshot(&stored.response_id)
            .await
            .unwrap();
        let Some(OutputItem::Reasoning(reasoning)) = retrieved
            .output
            .iter()
            .find(|item| matches!(item, OutputItem::Reasoning(_)))
        else {
            panic!("expected reasoning in {:?}", retrieved.output);
        };
        assert_eq!(reasoning.content, vec![ReasoningTextContent::new("snapshot plaintext")]);
        assert_eq!(reasoning.summary.len(), 1);
        assert!(reasoning.encrypted_content.is_none());
        assert!(reasoning.status.is_none());

        let response = match ExecuteRequest::new(stored.followup, stored.exec_ctx).run().await {
            Ok(response) => support::unwrap_blocking(response),
            Err(error) => panic!("in_conversation={in_conversation}: {error:?}"),
        };
        assert_eq!(response.status, "completed", "in_conversation={in_conversation}");
    }
}

#[tokio::test]
async fn snapshots_earlier_releases_could_not_store_fail_closed() {
    let unreadable = [
        json!({"content": [{"text": "part without a type"}]}),
        // Its only replay state would be dropped.
        json!({"content": [], "encrypted_content": {"ciphertext": "object state"}}),
    ];
    for (in_conversation, legacy) in [false, true]
        .into_iter()
        .flat_map(|mode| unreadable.iter().map(move |legacy| (mode, legacy)))
    {
        let stored = stored_legacy_snapshot(legacy, in_conversation).await;
        let retrieved = stored
            .exec_ctx
            .resp_handler
            .get_snapshot(&stored.response_id)
            .await
            .unwrap_err();
        let Err(continued) = ExecuteRequest::new(stored.followup, stored.exec_ctx).run().await else {
            panic!("in_conversation={in_conversation} {legacy}: continuation read an unreadable snapshot");
        };
        for error in [retrieved, continued] {
            assert!(
                matches!(
                    error,
                    ExecutorError::Storage(StorageError::InvalidResponseMetadata { .. })
                ),
                "in_conversation={in_conversation} {legacy}: {error:?}"
            );
        }
    }
}
