//! Fail closed before replay when durable response context cannot be decoded.

mod support;

use std::sync::Arc;

use agentic_core::executor::{ConversationHandler, ExecutionContext, ResponseHandler, execute};
use agentic_core::storage::{ConversationStore, ConversationVersion, ResponseMetadata, ResponseStore, StorageError};
use support::{MockResponse, MockServer, load_cassette, make_request, setup_pool, unwrap_blocking};

const SECRET: &str = "opaque-state-must-not-appear-in-diagnostics";

fn assert_redacted(error: &StorageError) {
    assert!(!error.to_string().contains(SECRET));
    assert!(!format!("{error:?}").contains(SECRET));
    assert!(std::error::Error::source(error).is_none());
}

#[tokio::test]
async fn invalid_history_references_block_read_and_child_persistence() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ResponseStore::new(Arc::clone(&pool));
    let metadata = ResponseMetadata::default();
    store.persist("resp_parent", None, Vec::new(), &metadata).await?;

    for invalid in ["not JSON", "null", "{}", "[1]", "[null]", "[\"truncated\""] {
        sqlx::query("UPDATE responses SET history_item_ids = $1 WHERE id = $2")
            .bind(invalid)
            .bind("resp_parent")
            .execute(pool.as_ref())
            .await?;

        for error in [
            store.get("resp_parent").await.unwrap_err(),
            store.rehydrate("resp_parent").await.unwrap_err(),
            store
                .persist("resp_child", Some("resp_parent"), Vec::new(), &metadata)
                .await
                .unwrap_err(),
        ] {
            assert!(matches!(
                error,
                StorageError::InvalidResponseHistory { ref response_id } if response_id == "resp_parent"
            ));
            assert_redacted(&error);
        }
        assert!(store.get("resp_child").await.unwrap_err().is_not_found());
    }
    Ok(())
}

#[tokio::test]
async fn invalid_metadata_blocks_both_stores_without_echoing_secrets() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let conversations = ConversationStore::new(Arc::clone(&pool));
    let responses = ResponseStore::new(Arc::clone(&pool));
    let conversation = conversations.create().await?;
    conversations
        .persist(
            &conversation.conversation_id,
            "resp_parent",
            None,
            Vec::new(),
            &ResponseMetadata::default(),
        )
        .await?;
    let version = conversations
        .rehydrate_snapshot(&conversation.conversation_id)
        .await?
        .version;
    let secret_metadata = format!(r#"{{"model":"test","effective_tool_choice":"{SECRET}"}}"#);

    for invalid in ["not JSON", "null", "[]", "{}", r#"{"model":42}"#, &secret_metadata] {
        sqlx::query("UPDATE responses SET metadata = $1 WHERE id = $2")
            .bind(invalid)
            .bind("resp_parent")
            .execute(pool.as_ref())
            .await?;
        for error in [
            responses.get("resp_parent").await.unwrap_err(),
            responses.rehydrate("resp_parent").await.unwrap_err(),
            conversations
                .response_metadata_at_version(&conversation.conversation_id, &version)
                .await
                .unwrap_err(),
            responses
                .persist(
                    "resp_child",
                    Some("resp_parent"),
                    Vec::new(),
                    &ResponseMetadata::default(),
                )
                .await
                .unwrap_err(),
        ] {
            assert!(matches!(
                error,
                StorageError::InvalidResponseMetadata { ref response_id } if response_id == "resp_parent"
            ));
            assert_redacted(&error);
        }
        assert!(responses.get("resp_child").await.unwrap_err().is_not_found());
    }
    Ok(())
}

#[tokio::test]
async fn missing_or_foreign_captured_response_is_not_empty_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let store = ConversationStore::new(pool);
    let first = store.create().await?;
    let other = store.create().await?;
    store
        .persist(
            &first.conversation_id,
            "resp_first",
            None,
            Vec::new(),
            &ResponseMetadata::default(),
        )
        .await?;
    for response_id in ["resp_missing", "resp_first"] {
        let error = store
            .response_metadata_at_version(
                &other.conversation_id,
                &ConversationVersion {
                    response_id: Some(response_id.to_owned()),
                    last_sequence: None,
                    revision: 0,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(error, StorageError::InvalidResponseMetadata { .. }));
    }
    for version in [
        ConversationVersion::default(),
        ConversationVersion {
            last_sequence: Some(0),
            ..ConversationVersion::default()
        },
    ] {
        assert!(
            store
                .response_metadata_at_version(&first.conversation_id, &version)
                .await?
                .is_none()
        );
    }
    Ok(())
}

#[tokio::test]
async fn sql_null_legacy_fields_remain_readable() -> Result<(), Box<dyn std::error::Error>> {
    let pool = setup_pool().await;
    let conversations = ConversationStore::new(Arc::clone(&pool));
    let responses = ResponseStore::new(Arc::clone(&pool));
    let conversation = conversations.create().await?;
    conversations
        .persist(
            &conversation.conversation_id,
            "resp_legacy",
            None,
            Vec::new(),
            &ResponseMetadata::default(),
        )
        .await?;
    sqlx::query("UPDATE responses SET history_item_ids = NULL, metadata = NULL WHERE id = $1")
        .bind("resp_legacy")
        .execute(pool.as_ref())
        .await?;
    let response = responses.get("resp_legacy").await?;
    assert!(response.history_item_ids.is_empty());
    assert!(response.metadata.model.is_empty());
    assert!(responses.rehydrate("resp_legacy").await?.is_empty());
    let version = conversations
        .rehydrate_snapshot(&conversation.conversation_id)
        .await?
        .version;
    assert!(
        conversations
            .response_metadata_at_version(&conversation.conversation_id, &version)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn corrupt_continuation_fails_before_inference_in_json_and_sse() -> Result<(), Box<dyn std::error::Error>> {
    let cassette = load_cassette(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/cassettes/text_only/responses/resp-single-gpt-4o-nonstreaming.yaml"
    ));
    let turn = &cassette.turns[0];
    let server = MockServer::start_deque(vec![MockResponse::from_turn(turn)]).await;
    let pool = setup_pool().await;
    let context = Arc::new(ExecutionContext::new(
        ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
        ResponseHandler::new(ResponseStore::new(Arc::clone(&pool))),
        Arc::new(reqwest::Client::new()),
        server.url().to_owned(),
    ));
    let first = unwrap_blocking(
        execute(
            make_request(&turn.request.body.input, true, false, None, None),
            Arc::clone(&context),
        )
        .await?,
    );
    sqlx::query("UPDATE responses SET metadata = $1 WHERE id = $2")
        .bind(format!(r#"{{"model":"test","effective_tool_choice":"{SECRET}"}}"#))
        .bind(&first.id)
        .execute(pool.as_ref())
        .await?;
    for stream in [false, true] {
        let result = execute(
            make_request("continue", true, stream, Some(first.id.clone()), None),
            Arc::clone(&context),
        )
        .await;
        let Err(agentic_core::executor::ExecutorError::Storage(error)) = result else {
            panic!("corrupt continuation must fail before returning a payload or stream");
        };
        assert!(matches!(error, StorageError::InvalidResponseMetadata { .. }));
        assert_redacted(&error);
    }
    assert_eq!(
        server.request_bodies().await.len(),
        1,
        "no continuation reached upstream"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM responses")
        .fetch_one(pool.as_ref())
        .await?;
    assert_eq!(count, 1, "no continuation was persisted");
    Ok(())
}
