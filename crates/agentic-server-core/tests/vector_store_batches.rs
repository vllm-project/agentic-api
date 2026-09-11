use agentic_core::{
    storage::create_pool_with_schema,
    tool::file_search::{FileSearchRuntime, FileSearchService},
    types::file_search::*,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};

#[allow(
    clippy::too_many_lines,
    reason = "keeps atomic membership, partial failure, and pagination assertions together"
)]
async fn batches(database: &str) {
    let pool = create_pool_with_schema(Some(database)).await.unwrap();
    let files = tempfile::tempdir().unwrap();
    let service = FileSearchService::new(
        pool.clone(),
        Arc::new(reqwest::Client::new()),
        FileSearchConfig {
            files_storage_dir: Some(files.path().into()),
            ..Default::default()
        },
    )
    .unwrap();
    let store = service
        .create_vector_store(serde_json::from_value(json!({})).unwrap())
        .await
        .unwrap();
    let good = service
        .upload_file("good.txt", "text/plain", "assistants", b"lunar policy".to_vec())
        .await
        .unwrap();
    let bad = service
        .upload_file("bad.bin", "application/octet-stream", "assistants", vec![0, 255])
        .await
        .unwrap();
    for invalid in [
        json!({}),
        json!({"file_ids":[]}),
        json!({"file_ids":[good.id],"files":[]}),
        json!({"file_ids":[good.id,good.id]}),
        json!({"file_ids":[good.id,"missing"]}),
    ] {
        assert!(
            service
                .create_file_batch(&store.id, serde_json::from_value(invalid).unwrap())
                .await
                .is_err()
        );
        assert_eq!(service.get_vector_store(&store.id).await.unwrap().file_counts.total, 0);
    }
    let batch = service
        .create_file_batch(
            &store.id,
            serde_json::from_value(
                json!({"files":[{"file_id":good.id,"attributes":{"team":"moon"}},{"file_id":bad.id}]}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(batch.object, "vector_store.files_batch");
    assert_eq!(batch.file_counts.in_progress, 2);
    assert_eq!(
        service.get_vector_store_file(&store.id, &good.id).await.unwrap().status,
        AttachmentStatus::InProgress
    );
    assert_eq!(
        service
            .create_file_batch(
                &store.id,
                serde_json::from_value(json!({"file_ids":[good.id]})).unwrap()
            )
            .await
            .unwrap_err()
            .status_code(),
        409
    );
    let first = service
        .list_file_batch_files(
            &store.id,
            &batch.id,
            &ListParams {
                limit: Some(1),
                order: Some(ListOrder::Asc),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(first.has_more);
    let next = service
        .list_file_batch_files(
            &store.id,
            &batch.id,
            &ListParams {
                limit: Some(1),
                order: Some(ListOrder::Asc),
                after: Some(first.data[0].id.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!next.has_more);
    assert_ne!(first.data[0].id, next.data[0].id);
    let previous = service
        .list_file_batch_files(
            &store.id,
            &batch.id,
            &ListParams {
                limit: Some(1),
                order: Some(ListOrder::Asc),
                before: Some(next.data[0].id.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(previous.data[0].id, first.data[0].id);
    let runtime = FileSearchRuntime::start(service.clone());
    let other = FileSearchRuntime::start(service.clone());
    let finished = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let batch = service.get_file_batch(&store.id, &batch.id).await.unwrap();
            if batch.status != BatchStatus::InProgress {
                break batch;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
    other.shutdown().await.unwrap();
    assert_eq!(finished.status, BatchStatus::Completed);
    assert_eq!(finished.file_counts.completed, 1);
    assert_eq!(finished.file_counts.failed, 1);
    let failed = service.get_vector_store_file(&store.id, &bad.id).await.unwrap();
    assert_eq!(
        failed.last_error.unwrap().code,
        VectorStoreFileErrorCode::UnsupportedFile
    );

    let page = service
        .list_file_batch_files(
            &store.id,
            &batch.id,
            &ListParams {
                filter: Some(AttachmentStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.data.len(), 1);
    assert_eq!(page.data[0].id, good.id);
    let reused = service
        .create_file_batch(
            &store.id,
            serde_json::from_value(json!({"files":[{"file_id":good.id,"attributes":{"team":"changed"}}]})).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reused.status, BatchStatus::Completed);
    assert_eq!(
        service
            .list_file_batch_files(&store.id, &reused.id, &ListParams::default())
            .await
            .unwrap()
            .data[0]
            .attributes,
        page.data[0].attributes
    );
    assert_eq!(
        service
            .create_file_batch(&store.id, serde_json::from_value(json!({"file_ids":[bad.id]})).unwrap())
            .await
            .unwrap_err()
            .status_code(),
        409
    );
    service.detach_file(&store.id, &good.id).await.unwrap();
    assert_eq!(
        service
            .get_file_batch(&store.id, &batch.id)
            .await
            .unwrap()
            .file_counts
            .completed,
        1
    );
    service.delete_vector_store(&store.id).await.unwrap();
    service.delete_file(&good.id).await.unwrap();
    service.delete_file(&bad.id).await.unwrap();
}
#[tokio::test]
async fn sqlite_batches() {
    batches("sqlite::memory:").await;
}
#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL"]
async fn postgres_batches() {
    batches(&std::env::var("TEST_POSTGRES_URL").unwrap()).await;
}

#[derive(Clone, Default)]
struct Barrier {
    blocked: Arc<std::sync::atomic::AtomicBool>,
    started: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}
async fn embedding(
    axum::extract::State(barrier): axum::extract::State<Barrier>,
    axum::Json(input): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    if barrier.blocked.load(std::sync::atomic::Ordering::SeqCst) {
        barrier.started.notify_one();
        barrier.resume.notified().await;
    }
    axum::Json(
        json!({"model":input["model"],"data":input["input"].as_array().unwrap().iter().enumerate().map(|(index,_)|json!({"index":index,"embedding":[1.0,0.0]})).collect::<Vec<_>>()}),
    )
}
#[allow(
    clippy::too_many_lines,
    reason = "keeps the blocked model cancellation and restart sequence in one fixture"
)]
async fn blocked_lifecycle(database: &str) {
    let barrier = Barrier::default();
    barrier.blocked.store(true, std::sync::atomic::Ordering::SeqCst);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route("/v1/embeddings", axum::routing::post(embedding))
        .with_state(barrier.clone());
    let http = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let pool = create_pool_with_schema(Some(database)).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let service = FileSearchService::new(
        pool.clone(),
        Arc::new(reqwest::Client::new()),
        FileSearchConfig {
            files_storage_dir: Some(directory.path().into()),
            embedding_base_url: Some(url),
            embedding_model: Some("fixture".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let store = service
        .create_vector_store(serde_json::from_value(json!({})).unwrap())
        .await
        .unwrap();
    let file = service
        .upload_file("blocked.txt", "text/plain", "assistants", b"blocked embedding".to_vec())
        .await
        .unwrap();
    let request = || serde_json::from_value(json!({"file_ids":[file.id]})).unwrap();
    let batch = service.create_file_batch(&store.id, request()).await.unwrap();
    let runtime = FileSearchRuntime::start(service.clone());
    tokio::time::timeout(Duration::from_secs(5), barrier.started.notified())
        .await
        .unwrap();
    // A second service instance issues cancellation while the first owns the model request.
    let cancelled = service.clone().cancel_file_batch(&store.id, &batch.id).await.unwrap();
    assert_eq!(cancelled.file_counts.cancelled, 1);
    tokio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .unwrap()
        .unwrap();
    let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE store_id=$1")
        .bind(&store.id)
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(chunks, 0);
    service.detach_file(&store.id, &file.id).await.unwrap();
    let resumable = service.create_file_batch(&store.id, request()).await.unwrap();
    let runtime = FileSearchRuntime::start(service.clone());
    tokio::time::timeout(Duration::from_secs(5), barrier.started.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .unwrap()
        .unwrap();
    let states: Vec<String> = sqlx::query_scalar("SELECT state FROM file_search_jobs WHERE batch_id=$1")
        .bind(&resumable.id)
        .fetch_all(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(states, ["queued"]);
    assert_eq!(
        service.get_file_batch(&store.id, &resumable.id).await.unwrap().status,
        BatchStatus::InProgress
    );
    service
        .update_vector_store_file(
            &store.id,
            &file.id,
            serde_json::from_value(json!({"attributes":{"team":"updated"}})).unwrap(),
        )
        .await
        .unwrap();
    barrier.blocked.store(false, std::sync::atomic::Ordering::SeqCst);
    barrier.resume.notify_waiters();
    let restarted = FileSearchRuntime::start(service.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if service
                .get_file_batch(&store.id, &resumable.id)
                .await
                .unwrap()
                .file_counts
                .completed
                == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    restarted.shutdown().await.unwrap();
    assert_eq!(
        service
            .get_vector_store_file(&store.id, &file.id)
            .await
            .unwrap()
            .attributes,
        serde_json::from_value(json!({"team":"updated"})).unwrap()
    );
    let result = service
        .search(
            std::slice::from_ref(&store.id),
            &serde_json::from_value(json!({"query":"blocked","filters":{"type":"eq","key":"team","value":"updated"}}))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result.data.len(), 1);

    let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE store_id=$1")
        .bind(&store.id)
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(chunks, 1);
    // Fresh starts preserve completed work and never reset another runtime's valid claim.
    let again = FileSearchRuntime::start(service.clone());
    again.shutdown().await.unwrap();
    assert_eq!(
        service
            .get_file_batch(&store.id, &resumable.id)
            .await
            .unwrap()
            .file_counts
            .completed,
        1
    );
    service.delete_vector_store(&store.id).await.unwrap();
    service.delete_file(&file.id).await.unwrap();
    http.abort();
    let _ = http.await;
}
#[tokio::test]
async fn sqlite_blocked_cancel_shutdown_restart() {
    blocked_lifecycle("sqlite::memory:").await;
}
#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL"]
async fn postgres_blocked_cancel_shutdown_restart() {
    blocked_lifecycle(&std::env::var("TEST_POSTGRES_URL").unwrap()).await;
}

async fn changed_contextual_default(database: &str) {
    let pool = create_pool_with_schema(Some(database)).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut config: FileSearchConfig = serde_json::from_value(json!({"vector_stores":{
        "providers":{"local":{"base_url":"http://127.0.0.1:9/v1","models":["embed","old","new"]}},
        "default_embedding_model":{"provider_id":"local","model_id":"embed","embedding_dimensions":2},
        "contextual_retrieval_params":{"model":{"provider_id":"local","model_id":"old"}}
    }}))
    .unwrap();
    config.files_storage_dir = Some(directory.path().into());
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config.clone()).unwrap();
    let store = service
        .create_vector_store(serde_json::from_value(json!({})).unwrap())
        .await
        .unwrap();
    let file = service
        .upload_file(
            "context.txt",
            "text/plain",
            "assistants",
            b"contextual recovery".to_vec(),
        )
        .await
        .unwrap();
    let batch = service
        .create_file_batch(
            &store.id,
            serde_json::from_value(
                json!({"files":[{"file_id":file.id,"chunking_strategy":{"type":"contextual","contextual":{}}}]}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    config
        .vector_stores
        .contextual_retrieval_params
        .model
        .as_mut()
        .unwrap()
        .model_id = "new".into();
    let changed = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config).unwrap();
    let runtime = FileSearchRuntime::start(changed);
    tokio::time::timeout(Duration::from_secs(5), async {
        while service
            .get_file_batch(&store.id, &batch.id)
            .await
            .unwrap()
            .file_counts
            .failed
            == 0
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
    let failed = service.get_vector_store_file(&store.id, &file.id).await.unwrap();
    assert_eq!(
        failed.last_error.unwrap().message,
        "Restore the batch ingestion model configuration before ingestion"
    );
    service.delete_vector_store(&store.id).await.unwrap();
    service.delete_file(&file.id).await.unwrap();
}
#[tokio::test]
async fn sqlite_changed_contextual_default_is_rejected() {
    changed_contextual_default("sqlite::memory:").await;
}
#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL"]
async fn postgres_changed_contextual_default_is_rejected() {
    changed_contextual_default(&std::env::var("TEST_POSTGRES_URL").unwrap()).await;
}
