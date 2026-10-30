//! Background mode on `POST /v1/responses`: the request is validated and stored
//! as `queued`, a supervised job writes the terminal snapshot, and
//! `POST /v1/responses/{id}/cancel` stops it.
#[allow(dead_code)]
mod background_support;
#[allow(dead_code)]
mod common;

use background_support::{Gateway, HeldSearch, Model, TestDb, Turn, id, mcp_server};
use http::StatusCode;
use serde_json::{Value, json};

fn text(response: &Value) -> &str {
    response["output"][0]["content"][0]["text"]
        .as_str()
        .expect("first output text")
}

#[tokio::test]
async fn background_response_is_queued_then_completes_with_a_stable_snapshot() {
    let model = Model::start(vec![Turn::Answer("first answer")]).await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&Gateway::config(&model.url, &db)).await;

    let queued = gateway
        .create_background(json!({"instructions": "be brief", "metadata": {"k": "v"}}))
        .await;
    assert_eq!(queued["object"], "response");
    assert_eq!(queued["status"], "queued");
    assert_eq!(queued["background"], true);
    assert_eq!(queued["store"], true);
    assert_eq!(queued["output"], json!([]));
    assert_eq!(queued["usage"], Value::Null);
    assert_eq!(queued["error"], Value::Null);
    assert_eq!(queued["instructions"], "be brief");
    assert_eq!(queued["metadata"], json!({"k": "v"}));
    assert!(id(&queued).starts_with("resp_"), "{queued}");

    let completed = gateway.wait_for_status(id(&queued), "completed").await;
    assert_eq!(completed["id"], queued["id"]);
    assert_eq!(completed["background"], true);
    assert_eq!(
        completed["created_at"], queued["created_at"],
        "created_at is the admission time"
    );
    assert_eq!(text(&completed), "first answer");
    assert_eq!(completed["usage"]["total_tokens"], 4);
    assert_eq!(completed["metadata"], json!({"k": "v"}));
    let upstream = model.requests.lock().await.clone();
    assert_eq!(upstream.len(), 1);
    assert!(upstream[0].get("background").is_none(), "{}", upstream[0]);

    // A finished background response is an ordinary stored response.
    let (status, items) = gateway.input_items(id(&queued)).await;
    assert_eq!(status, StatusCode::OK, "{items}");
    assert_eq!(items["data"][0]["content"][0]["text"], "hi");
    let (status, next) = gateway
        .create(json!({"model": "test-model", "input": "again", "previous_response_id": queued["id"]}))
        .await;
    assert_eq!(status, StatusCode::OK, "{next}");
    let continued = model.requests.lock().await[1].clone();
    assert_eq!(
        continued["input"].as_array().map(Vec::len),
        Some(3),
        "the continuation sees the background turn: {continued}"
    );
}

#[tokio::test]
async fn background_rejects_unstored_streaming_and_multi_agent_requests() {
    let model = Model::start(Vec::new()).await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&Gateway::config(&model.url, &db)).await;

    for (extra, param, message) in [
        (
            json!({"store": false}),
            "store",
            "background mode requires store: true; store: false background responses are not supported yet",
        ),
        (
            json!({"store": false, "stream": true}),
            "store",
            "background mode requires store: true; store: false background responses are not supported yet",
        ),
        (
            json!({"stream": true}),
            "stream",
            "background mode does not support stream: true yet; poll GET /v1/responses/{response_id} instead",
        ),
        (
            json!({"multi_agent": {"enabled": true}}),
            "multi_agent",
            "background mode does not support multi_agent yet",
        ),
    ] {
        let mut request = json!({"model": "test-model", "input": "hi", "background": true});
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let (status, body) = gateway.create(request.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{request}: {body}");
        assert_eq!(
            body,
            json!({"error": {
                "message": message, "type": "invalid_request_error", "code": "unsupported_value", "param": param
            }}),
            "{request}"
        );
    }
    assert_eq!(model.request_count().await, 0, "rejected before inference");
}

#[tokio::test]
async fn background_requests_are_validated_before_they_are_queued() {
    let model = Model::start(Vec::new()).await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&Gateway::config(&model.url, &db)).await;

    let (status, body) = gateway
        .create(json!({"model": "test-model", "input": "hi", "background": true,
            "previous_response_id": "resp_missing"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "previous_response_not_found");

    let (status, body) = gateway
        .create(json!({"model": "test-model", "input": "hi", "background": "true"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "invalid_type");
    assert_eq!(model.request_count().await, 0);
}

#[tokio::test]
async fn retrieval_rejects_streaming_and_resume_parameters() {
    let model = Model::start(vec![Turn::Answer("answer")]).await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&Gateway::config(&model.url, &db)).await;
    let queued = gateway.create_background(json!({})).await;
    gateway.wait_for_status(id(&queued), "completed").await;

    for (query, param) in [
        ("stream=true", "stream"),
        ("stream=1", "stream"),
        ("starting_after=3", "starting_after"),
        ("stream=false&starting_after=0", "starting_after"),
    ] {
        let (status, body) = gateway.get_with_query(id(&queued), query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["error"]["param"], param, "{query}");
        assert_eq!(body["error"]["code"], "unsupported_parameter", "{query}");
    }
    let (status, body) = gateway.get_with_query(id(&queued), "stream=false").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "completed");
}

#[tokio::test]
async fn cancel_during_inference_drops_the_model_request() {
    let mut model = Model::start(vec![Turn::Hold("never sent")]).await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&Gateway::config(&model.url, &db)).await;
    let queued = gateway.create_background(json!({})).await;
    model.wait_started(0).await;
    gateway.wait_for_status(id(&queued), "in_progress").await;

    let (status, cancelled) = gateway.cancel(id(&queued)).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["background"], true);
    assert_eq!(cancelled["output"], json!([]));
    model.wait_dropped(1).await;

    let (status, retrieved) = gateway.get(id(&queued)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retrieved, cancelled);
    assert_eq!(model.request_count().await, 1, "a cancelled job is not retried");
}

#[tokio::test]
async fn cancel_during_a_gateway_tool_call_drops_the_tool_call() {
    let mut model = Model::start(vec![Turn::Search, Turn::Answer("unreachable")]).await;
    let search = HeldSearch::new();
    let db = TestDb::new().await;
    let gateway = Gateway::start_with_search(&Gateway::config(&model.url, &db), &search).await;
    let queued = gateway
        .create_background(json!({"tools": [{"type": "web_search_preview"}]}))
        .await;
    model.wait_started(0).await;
    search.wait_started().await;

    let (status, cancelled) = gateway.cancel(id(&queued)).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["status"], "cancelled");
    search.wait_dropped().await;
    assert_eq!(
        gateway.get(id(&queued)).await.1["status"],
        "cancelled",
        "the dropped tool call does not overwrite the cancellation"
    );
    assert_eq!(
        model.request_count().await,
        1,
        "no inference round follows the cancelled tool call"
    );
}

#[tokio::test]
async fn cancel_is_idempotent_and_rejects_finished_foreground_and_unknown_responses() {
    let mut model = Model::start(vec![Turn::Hold("never sent"), Turn::Answer("done"), Turn::Answer("fg")]).await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&Gateway::config(&model.url, &db)).await;

    let held = gateway.create_background(json!({})).await;
    model.wait_started(0).await;
    let (first_status, first) = gateway.cancel(id(&held)).await;
    let (second_status, second) = gateway.cancel(id(&held)).await;
    assert_eq!((first_status, second_status), (StatusCode::OK, StatusCode::OK));
    assert_eq!(first, second, "cancelling twice returns the same cancelled response");
    model.wait_dropped(1).await;

    let finished = gateway.create_background(json!({})).await;
    gateway.wait_for_status(id(&finished), "completed").await;
    let (status, body) = gateway.cancel(id(&finished)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body,
        json!({"error": {
            "message": "Cannot cancel a response with status 'completed'.",
            "type": "invalid_request_error", "code": "response_not_cancellable", "param": "response_id"
        }})
    );

    let (status, foreground) = gateway.create(json!({"model": "test-model", "input": "fg"})).await;
    assert_eq!(status, StatusCode::OK, "{foreground}");
    let (status, body) = gateway.cancel(id(&foreground)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body,
        json!({"error": {
            "message": "Only responses created with background=true can be cancelled.",
            "type": "invalid_request_error", "code": "response_not_cancellable", "param": "response_id"
        }})
    );

    let (status, body) = gateway.cancel("resp_missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["message"], "Response with id 'resp_missing' not found.");
}

#[tokio::test]
async fn continuation_and_input_items_wait_for_a_finished_background_response() {
    let mut model = Model::start(vec![Turn::Hold("never sent")]).await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&Gateway::config(&model.url, &db)).await;
    let held = gateway.create_background(json!({})).await;
    model.wait_started(0).await;
    gateway.wait_for_status(id(&held), "in_progress").await;

    for expected in ["in_progress", "cancelled"] {
        let (status, body) = gateway
            .create(json!({"model": "test-model", "input": "next", "previous_response_id": held["id"]}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            body,
            json!({"error": {
                "message": format!(
                    "Previous response with id '{}' has status '{expected}'; \
                     only a completed or incomplete response can be continued.",
                    id(&held)
                ),
                "type": "invalid_request_error", "code": "previous_response_not_completed",
                "param": "previous_response_id"
            }})
        );
        let (status, body) = gateway.input_items(id(&held)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "response_not_completed");
        if expected == "in_progress" {
            assert_eq!(gateway.cancel(id(&held)).await.0, StatusCode::OK);
        }
    }
    assert_eq!(
        model.request_count().await,
        1,
        "rejected continuations never reach the model"
    );
}

#[tokio::test]
async fn deleting_a_running_background_response_stops_it_for_good() {
    let mut model = Model::start(vec![Turn::Hold("never stored")]).await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&Gateway::config(&model.url, &db)).await;
    let held = gateway.create_background(json!({})).await;
    model.wait_started(0).await;

    let (status, deleted) = gateway.delete(id(&held)).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    model.wait_dropped(1).await;
    model.release();
    let (status, body) = gateway.get(id(&held)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the stopped job did not store its row again: {body}"
    );
}

#[tokio::test]
async fn startup_fails_only_rows_whose_owner_lease_expired() {
    let mut model = Model::start(vec![Turn::Hold("lost"), Turn::Hold("kept")]).await;
    let db = TestDb::new().await;
    let config = Gateway::config(&model.url, &db);
    let first = Gateway::start(&config).await;
    let lost = first.create_background(json!({})).await;
    model.wait_started(0).await;
    let kept = first.create_background(json!({})).await;
    model.wait_started(1).await;

    // The owner of `lost` stops renewing; `kept` still has a live lease.
    let pool = first.exec.storage_pool().expect("storage pool");
    let expired = sqlx::query("UPDATE responses SET background_lease_expires_at = 0 WHERE id = $1")
        .bind(id(&lost))
        .execute(pool)
        .await
        .expect("expire the lease");
    assert_eq!(expired.rows_affected(), 1);

    let restarted = Gateway::start(&config).await;
    let failed = restarted.get(id(&lost)).await.1;
    assert_eq!(failed["status"], "failed", "{failed}");
    assert_eq!(failed["error"]["code"], "server_error", "{failed}");
    assert_eq!(failed["background"], true);
    assert_eq!(
        restarted.get(id(&kept)).await.1["status"],
        "in_progress",
        "another live instance keeps its job"
    );

    model.release();
    model.release();
    restarted.wait_for_status(id(&kept), "completed").await;
}

#[tokio::test]
async fn a_background_turn_joins_its_conversation_only_when_it_completes() {
    let mut model = Model::start(vec![
        Turn::Hold("background answer"),
        Turn::Hold("stale answer"),
        Turn::Answer("foreground answer"),
    ])
    .await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&Gateway::config(&model.url, &db)).await;
    let conversation = gateway.create_conversation().await;

    let first = gateway
        .create_background(json!({"conversation": conversation, "input": "first"}))
        .await;
    model.wait_started(0).await;
    assert!(
        gateway.conversation_texts(&conversation).await.is_empty(),
        "a running turn is not part of the conversation yet"
    );
    model.release();
    let completed = gateway.wait_for_status(id(&first), "completed").await;
    assert_eq!(completed["conversation"]["id"], conversation.as_str());
    assert_eq!(
        gateway.conversation_texts(&conversation).await,
        ["first", "background answer"]
    );

    // A turn that commits while a background turn runs changes the conversation it started from.
    let stale = gateway
        .create_background(json!({"conversation": conversation, "input": "second"}))
        .await;
    model.wait_started(1).await;
    let (status, foreground) = gateway
        .create(json!({"model": "test-model", "input": "third", "conversation": conversation}))
        .await;
    assert_eq!(status, StatusCode::OK, "{foreground}");
    model.release();
    let failed = gateway.wait_for_status(id(&stale), "failed").await;
    assert_eq!(failed["error"]["code"], "conversation_locked", "{failed}");
    assert_eq!(
        gateway.conversation_texts(&conversation).await,
        ["first", "background answer", "third", "foreground answer"],
        "the failed background turn added nothing"
    );
}

#[tokio::test]
async fn background_rows_never_store_request_scoped_mcp_credentials() {
    let mut model = Model::start(vec![Turn::Hold("never sent")]).await;
    let mcp = mcp_server().await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&Gateway::config(&model.url, &db)).await;
    let declaration = json!({
        "type": "mcp", "server_label": "stub", "server_url": mcp,
        "headers": {"Authorization": "Bearer header-secret"}, "authorization": "field-secret",
        "require_approval": "never"
    });
    let queued = gateway.create_background(json!({"tools": [declaration]})).await;
    assert_eq!(
        queued["tools"],
        json!([{"type": "mcp", "server_label": "stub", "server_url": mcp, "require_approval": "never"}])
    );
    model.wait_started(0).await;
    let pool = gateway.exec.storage_pool().expect("storage pool");
    let stored = || async {
        sqlx::query_scalar::<_, String>("SELECT metadata FROM responses WHERE id = $1")
            .bind(id(&queued))
            .fetch_one(pool)
            .await
            .expect("the background row")
    };
    let mut rows = vec![stored().await];
    gateway.wait_for_status(id(&queued), "in_progress").await;
    rows.push(stored().await);
    assert_eq!(gateway.cancel(id(&queued)).await.0, StatusCode::OK);
    rows.push(stored().await);
    for row in rows {
        for secret in ["header-secret", "field-secret"] {
            assert!(!row.contains(secret), "{secret} reached storage: {row}");
        }
    }
}
