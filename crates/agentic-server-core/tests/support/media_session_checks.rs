//! Session contracts for media-aware compaction and typed message-file rejection.
use super::*;

fn media_request(previous: Option<&str>, input: Value) -> RequestPayload {
    let mut payload = request(previous, input);
    payload.context_management =
        Some(serde_json::from_value(json!([{"type":"compaction", "compact_threshold":4096}])).unwrap());
    payload
}

fn media_input(image_bytes: usize) -> Value {
    json!([{"role":"user", "content":[
        {"type":"input_text", "text":"describe"},
        {"type":"input_image", "image_url":format!("data:image/png;base64,{}", "A".repeat(image_bytes))}
    ]}])
}

fn retained_image(input: &Value) -> &Value {
    let matches: Vec<_> = input
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part["type"] == "input_image")
        .collect();
    assert_eq!(matches.len(), 1, "exactly one retained image");
    matches[0]
}

#[tokio::test]
async fn combined_media_session_history_and_forks_do_not_compact_image_bytes() {
    let model = LocalModel::start(vec![
        model_message("resp_image", "first answer"),
        model_message("resp_fork", "fork answer"),
        model_message("resp_source", "source answer"),
    ])
    .await;
    let group = ResponseSessionGroup::new(
        NonZeroUsize::new(2).unwrap(),
        NonZeroUsize::new(64).unwrap(),
        NonZeroUsize::new(1024 * 1024).unwrap(),
        NonZeroUsize::new(4 * 1024 * 1024).unwrap(),
    );
    let source = group.new_session().unwrap();
    let fork = group.new_session().unwrap();
    let input = media_input(256 * 1024);
    let first = execute_local(&model, &source, media_request(None, input.clone())).await;
    assert_eq!(
        model.requests.lock().await.len(),
        1,
        "image alone must not add a summary"
    );
    execute_local(&model, &fork, media_request(Some(&first.id), json!("fork question"))).await;
    execute_local(
        &model,
        &source,
        media_request(Some(&first.id), json!("source question")),
    )
    .await;
    let requests = model.requests.lock().await;
    assert_eq!(requests.len(), 3, "one inference per turn, no image-driven summary");
    for request in requests.iter() {
        assert_eq!(retained_image(&request["input"]), &input[0]["content"][1]);
    }
    drop(requests);
    source.wait_until_idle().await.unwrap();
    fork.wait_until_idle().await.unwrap();
    model.close().await;
}

#[tokio::test]
async fn combined_media_text_compaction_promotes_and_restores_canonical_image_history() {
    let old_answer = "obsolete assistant details ".repeat(1024);
    let mut model = LocalModel::start(vec![
        model_message("resp_image", &old_answer),
        model_message("resp_summary", "compact summary"),
        model_message("resp_compacted", "second answer"),
        model_message("resp_restored", "third answer"),
    ])
    .await;
    let pool = storage_pool().await;
    let store = ResponseStore::new(Arc::clone(&pool));
    model.exec.resp_handler = ResponseHandler::new(store.clone());
    let session = ResponseSession::new(
        NonZeroUsize::new(64).unwrap(),
        NonZeroUsize::new(2 * 1024 * 1024).unwrap(),
    );
    let input = media_input(256 * 1024);
    let first = execute_local(&model, &session, media_request(None, input.clone())).await;
    assert_eq!(model.requests.lock().await.len(), 1, "image alone must not compact");
    let mut next = media_request(Some(&first.id), json!("second question"));
    next.store = true;
    let second = execute_local(&model, &session, next).await;
    assert_eq!(
        model.requests.lock().await.len(),
        3,
        "long text must still cause a summary"
    );
    assert!(store.get(&first.id).await.unwrap_err().is_not_found());
    let durable =
        serde_json::to_value(InOutItem::into_input_items(store.rehydrate(&second.id).await.unwrap())).unwrap();
    assert_eq!(retained_image(&durable), &input[0]["content"][1]);
    assert!(!durable.to_string().contains(&old_answer));
    assert_eq!(
        durable
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "compaction")
            .count(),
        1
    );
    drop(session);
    let fresh = ResponseSession::new(
        NonZeroUsize::new(64).unwrap(),
        NonZeroUsize::new(2 * 1024 * 1024).unwrap(),
    );
    let mut restored = media_request(Some(&second.id), json!("third question"));
    restored.store = true;
    execute_local(&model, &fresh, restored).await;
    let requests = model.requests.lock().await;
    assert_eq!(requests.len(), 4, "canonical restore must not recompact image bytes");
    assert_eq!(retained_image(&requests[3]["input"]), &input[0]["content"][1]);
    assert!(!requests[3]["input"].to_string().contains(&old_answer));
    drop(requests);
    model.close().await;
    pool.close().await;
}

#[tokio::test]
async fn combined_media_token_allowance_does_not_bypass_serialized_retention_budget() {
    let model = LocalModel::start(vec![
        model_message("resp_too_large", "answer"),
        model_message("resp_recovered", "small answer"),
    ])
    .await;
    let session = ResponseSession::new(NonZeroUsize::new(64).unwrap(), NonZeroUsize::new(16 * 1024).unwrap());
    let error = ExecuteRequest::new(
        media_request(None, media_input(128 * 1024)),
        Arc::new(model.exec.clone()),
    )
    .with_session(&session)
    .unwrap()
    .run()
    .await
    .err()
    .expect("real image bytes exceed retention");
    assert!(matches!(error, ExecutorError::PayloadTooLarge(_)), "{error}");
    assert!(error.to_string().contains("checkpoint budget"), "{error}");
    assert_eq!(
        model.requests.lock().await.len(),
        1,
        "retention is distinct from image token cost"
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), session.wait_until_idle())
        .await
        .unwrap()
        .unwrap();
    execute_local(&model, &session, media_request(None, json!("small new question"))).await;
    assert_eq!(
        model.requests.lock().await.len(),
        2,
        "one rejected publication, then a successful turn"
    );
    model.close().await;
}

#[tokio::test]
async fn combined_media_message_file_rejection_precedes_compaction_and_releases_session() {
    let model = LocalModel::start(vec![
        model_message("resp_recovered", "answer"),
        model_message("resp_unexpected_extra", "extra answer only if compaction regresses"),
    ])
    .await;
    let session = ResponseSession::new(NonZeroUsize::new(64).unwrap(), NonZeroUsize::new(1024 * 1024).unwrap());
    let mut input = media_input(256 * 1024);
    input[0]["content"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"input_file", "file_data":"encoded document"}));
    let error = ExecuteRequest::new(media_request(None, input), Arc::new(model.exec.clone()))
        .with_session(&session)
        .unwrap()
        .run()
        .await
        .err()
        .expect("unsupported message file");
    assert!(matches!(error, ExecutorError::InvalidRequest(_)));
    assert!(error.to_string().contains("input_file"), "{error}");
    assert!(
        model.requests.lock().await.is_empty(),
        "neither summary nor answer may start"
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), session.wait_until_idle())
        .await
        .unwrap()
        .unwrap();
    execute_local(&model, &session, media_request(None, media_input(256 * 1024))).await;
    assert_eq!(
        model.requests.lock().await.len(),
        1,
        "valid image remains supported after rejection"
    );
    model.close().await;
}
