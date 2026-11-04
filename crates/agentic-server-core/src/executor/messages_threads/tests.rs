use super::*;
use crate::types::messages::thread::MESSAGE_THREADS_BETA;
use serde_json::json;

mod claude_code_compatibility;

fn request(thread: &Value) -> Value {
    json!({"model":"local-model","max_tokens":32,"messages":[{"role":"user","content":"hello"}],"thread":thread})
}

async fn prepare(store: &MessagesThreadStore, value: &Value) -> Result<PreparedMessagesThread, MessagesThreadError> {
    store
        .prepare(
            &serde_json::to_vec(value).unwrap(),
            store.owner(&[b"alice"]),
            vec![MESSAGE_THREADS_BETA.to_owned()],
        )
        .await
        .map(Option::unwrap)
}

async fn finish(store: &MessagesThreadStore, value: &Value, stop: &str, content: &Value) -> String {
    let mut ctx = prepare(store, value).await.unwrap().into_context().unwrap();
    let id = ctx.thread_message_id().unwrap().to_owned();
    ctx.complete_thread(
        content,
        ThreadRetention::for_response(
            Some(stop),
            content
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use")),
        ),
    )
    .await
    .unwrap();
    id
}

fn continuation(id: &str) -> Value {
    request(&json!({"type":"continue","previous_message_id":id}))
}

fn code(error: MessagesThreadError) -> Value {
    serde_json::to_value(error.into_body()).unwrap()["error"]["details"]["error_code"].clone()
}

#[tokio::test]
async fn rehydrates_lossless_messages_system_and_client_tools() {
    let (store, _pool) = new_store().await;
    let mut create = request(&json!({"type":"create"}));
    create["system"] = json!([{"type":"text","text":"context","cache_control":{"type":"ephemeral"}}]);
    create["tools"] = json!([{"name":"client","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}]);
    create["messages"][0]["content"] = json!([
        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}},
        {"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}
    ]);
    let answer = json!([{"type":"thinking","thinking":"private","signature":"sig"},
        {"type":"tool_use","id":"call","name":"client","input":{"key":1}}]);
    let id = finish(&store, &create, "tool_use", &answer).await;
    let mut next = continuation(&id);
    next["messages"][0]["content"] = json!([{"type":"tool_result","tool_use_id":"call","content":[
        {"type":"image","source":{"type":"base64","data":"result"}}],"is_error":true}]);
    next["max_tokens"] = json!(64);
    let ctx = prepare(&store, &next).await.unwrap().into_context().unwrap();
    let upstream: Value = serde_json::from_str(&ctx.upstream_body().unwrap()).unwrap();
    assert!(upstream.get("thread").is_none());
    assert_eq!(upstream["system"], create["system"]);
    assert_eq!(upstream["tools"], create["tools"]);
    assert_eq!(
        upstream["messages"],
        json!([create["messages"][0],{"role":"assistant","content":answer},next["messages"][0]])
    );
    assert_eq!(upstream["max_tokens"], 64);
}

#[tokio::test]
async fn concurrent_continue_is_exclusive_and_drop_rolls_back_without_refreshing_ttl() {
    let (store, pool) = new_store().await;
    let id = finish(&store, &request(&json!({"type":"create"})), "end_turn", &json!([])).await;
    let expiry: i64 = sqlx::query_scalar("SELECT expires_at FROM message_threads WHERE message_id = $1")
        .bind(&id)
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    let reservation = prepare(&store, &continuation(&id)).await.unwrap();
    assert_eq!(
        code(prepare(&store, &continuation(&id)).await.unwrap_err()),
        "thread_continue_in_flight"
    );
    drop(reservation);
    store.shutdown().await;
    let unchanged: i64 = sqlx::query_scalar("SELECT expires_at FROM message_threads WHERE message_id = $1")
        .bind(&id)
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(unchanged, expiry);
    let latest = finish(&store, &continuation(&id), "end_turn", &json!([])).await;
    assert_eq!(
        code(prepare(&store, &continuation(&id)).await.unwrap_err()),
        "thread_already_continued"
    );
    assert!(prepare(&store, &continuation(&latest)).await.is_ok());
}

#[tokio::test]
async fn expiry_and_wrong_owner_return_not_found_and_do_not_refresh() {
    let (store, pool) = new_store().await;
    let id = finish(&store, &request(&json!({"type":"create"})), "end_turn", &json!([])).await;
    let body = serde_json::to_vec(&continuation(&id)).unwrap();
    assert_eq!(
        code(
            store
                .prepare(&body, store.owner(&[b"bob"]), vec![MESSAGE_THREADS_BETA.to_owned()])
                .await
                .unwrap_err()
        ),
        "thread_not_found"
    );
    sqlx::query("UPDATE message_threads SET expires_at = 0 WHERE message_id = $1")
        .bind(&id)
        .execute(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(
        code(prepare(&store, &continuation(&id)).await.unwrap_err()),
        "thread_not_found"
    );
    store.maintain().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_threads")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn fixed_fields_beta_set_and_create_only_presence_are_validated_before_reservation() {
    let (store, _pool) = new_store().await;
    let mut create = request(&json!({"type":"create"}));
    create["thinking"] = json!({"type":"enabled","budget_tokens":1024});
    let id = finish(&store, &create, "end_turn", &json!([])).await;
    let mut next = continuation(&id);
    assert_eq!(
        code(prepare(&store, &next).await.unwrap_err()),
        "thread_fingerprint_mismatch"
    );
    next["thinking"] = create["thinking"].clone();
    for field in ["system", "tools"] {
        next[field] = Value::Null;
        assert!(prepare(&store, &next).await.unwrap_err().code.is_none());
        next.as_object_mut().unwrap().remove(field);
    }
    let body = serde_json::to_vec(&next).unwrap();
    assert_eq!(
        code(
            store
                .prepare(
                    &body,
                    store.owner(&[b"alice"]),
                    vec![MESSAGE_THREADS_BETA.to_owned(), "extra".to_owned()]
                )
                .await
                .unwrap_err()
        ),
        "thread_fingerprint_mismatch"
    );
    assert!(prepare(&store, &next).await.is_ok());
}

#[tokio::test]
async fn inherited_breakpoints_count_but_earlier_message_breakpoints_do_not() {
    let (store, _pool) = new_store().await;
    let mut create = request(&json!({"type":"create"}));
    create["system"] = json!([{"type":"text","text":"context","cache_control":{"type":"ephemeral"}}]);
    create["cache_control"] = json!({"type":"ephemeral"});
    create["messages"][0]["content"] = json!([{"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}]);
    let id = finish(&store, &create, "end_turn", &json!([])).await;
    let mut next = continuation(&id);
    next["cache_control"] = create["cache_control"].clone();
    let block = json!({"type":"text","text":"next","cache_control":{"type":"ephemeral"}});
    next["messages"][0]["content"] = json!([block, block]);
    assert!(prepare(&store, &next).await.is_ok());
    store.shutdown().await;
    next["messages"][0]["content"].as_array_mut().unwrap().push(block);
    assert_eq!(
        code(prepare(&store, &next).await.unwrap_err()),
        "thread_unsupported_request"
    );
}

#[tokio::test]
async fn paused_threads_require_empty_messages_and_refusals_and_truncated_tools_are_not_retained() {
    let (store, _pool) = new_store().await;
    let id = finish(&store, &request(&json!({"type":"create"})), "pause_turn", &json!([])).await;
    assert!(prepare(&store, &continuation(&id)).await.is_err());
    let mut next = continuation(&id);
    next["messages"] = json!([]);
    assert!(prepare(&store, &next).await.is_ok());
    for (stop, content) in [
        ("refusal", json!([])),
        (
            "max_tokens",
            json!([{"type":"tool_use","id":"t","name":"client","input":{}}]),
        ),
    ] {
        let id = finish(&store, &request(&json!({"type":"create"})), stop, &content).await;
        assert_eq!(
            code(prepare(&store, &continuation(&id)).await.unwrap_err()),
            "thread_not_found"
        );
    }
}

#[tokio::test]
async fn unsupported_context_editing_and_missing_beta_fail_without_inference() {
    let (store, _pool) = new_store().await;
    let mut create = request(&json!({"type":"create"}));
    let body = serde_json::to_vec(&create).unwrap();
    assert!(store.prepare(&body, store.owner(&[]), Vec::new()).await.is_err());
    create["context_management"] = json!({"edits":[{"type":"clear_tool_uses_20250919"}]});
    assert_eq!(
        code(prepare(&store, &create).await.unwrap_err()),
        "thread_unsupported_request"
    );
    create["context_management"] = json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]});
    assert!(prepare(&store, &create).await.is_ok());
}

#[tokio::test]
async fn oversized_checkpoint_fails_without_consuming_parent() {
    let (store, _pool) = new_store().await;
    let id = finish(&store, &request(&json!({"type":"create"})), "end_turn", &json!([])).await;
    let mut ctx = prepare(&store, &continuation(&id))
        .await
        .unwrap()
        .into_context()
        .unwrap();
    let huge = json!([{"type":"text","text":"x".repeat(MAX_CHECKPOINT_BYTES)}]);
    assert!(matches!(
        ctx.complete_thread(&huge, ThreadRetention::Ready).await,
        Err(ExecutorError::PayloadTooLarge(_))
    ));
    store.shutdown().await;
    assert!(prepare(&store, &continuation(&id)).await.is_ok());
}

#[tokio::test]
async fn mcp_authorization_can_rotate_while_configuration_remains_fixed() {
    let (store, _pool) = new_store().await;
    let mut create = request(&json!({"type":"create"}));
    create["mcp_servers"] =
        json!([{"type":"url","name":"mcp","url":"https://example.com","authorization_token":"first"}]);
    create["container"] = json!({"id":"old","skills":[{"type":"custom","skill_id":"s"}]});
    let id = finish(&store, &create, "end_turn", &json!([])).await;
    let mut next = continuation(&id);
    next["mcp_servers"] = create["mcp_servers"].clone();
    next["mcp_servers"][0]["authorization_token"] = json!("rotated");
    next["container"] = create["container"].clone();
    next["container"]["id"] = json!("new");
    let ctx = prepare(&store, &next).await.unwrap().into_context().unwrap();
    let upstream: Value = serde_json::from_str(&ctx.upstream_body().unwrap()).unwrap();
    assert_eq!(upstream["mcp_servers"][0]["authorization_token"], "rotated");
    assert_eq!(upstream["container"]["id"], "new");
    drop(ctx);
    store.shutdown().await;
    next["mcp_servers"][0]["name"] = json!("different");
    assert_eq!(
        code(prepare(&store, &next).await.unwrap_err()),
        "thread_fingerprint_mismatch"
    );
}

async fn new_store() -> (MessagesThreadStore, Arc<DbPool>) {
    let pool = crate::storage::create_pool_with_schema(Some("sqlite::memory:"))
        .await
        .unwrap();
    (MessagesThreadStore::new(Arc::clone(&pool)), pool)
}
