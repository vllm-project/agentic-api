use std::sync::Arc;

use agentic_core::{
    storage::{DbPool, create_pool_with_schema},
    tool::file_search::FileSearchService,
    types::file_search::*,
};
use serde_json::json;

async fn fixture(database: &str) -> (FileSearchService, Arc<DbPool>, tempfile::TempDir) {
    let pool = create_pool_with_schema(Some(database)).await.unwrap();
    let files = tempfile::tempdir().unwrap();
    let service = FileSearchService::new(
        pool.clone(),
        Arc::new(reqwest::Client::new()),
        FileSearchConfig {
            files_storage_dir: Some(files.path().to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    (service, pool, files)
}

fn request(value: serde_json::Value) -> UpdateVectorStoreRequest {
    serde_json::from_value(value).unwrap()
}
fn query() -> SearchRequest {
    serde_json::from_value(json!({"query":"lunar"})).unwrap()
}

#[allow(
    clippy::too_many_lines,
    reason = "keeps expiry visibility, cleanup, and independent source assertions in one database fixture"
)]
async fn lifecycle(database: &str) {
    let (service, pool, _files) = fixture(database).await;
    let file = service
        .upload_file(
            "original.txt",
            "text/plain",
            "assistants",
            b"Original lunar policy.\n".repeat(100),
        )
        .await
        .unwrap();
    let first = service
        .create_vector_store(
            serde_json::from_value(json!({"file_ids":[file.id],"expires_after":{"anchor":"last_active_at","days":1}}))
                .unwrap(),
        )
        .await
        .unwrap();
    let second = service
        .create_vector_store(serde_json::from_value(json!({"file_ids":[file.id]})).unwrap())
        .await
        .unwrap();
    sqlx::query("UPDATE file_search_stores SET expires_at = 1 WHERE id = $1")
        .bind(&first.id)
        .execute(pool.as_ref())
        .await
        .unwrap();
    let expired = service.get_vector_store(&first.id).await.unwrap();
    assert_eq!(expired.status, VectorStoreStatus::Expired);
    assert_eq!(expired.file_counts.total, 0);
    assert_eq!(expired.usage_bytes, 0);
    assert_eq!(
        service
            .search(std::slice::from_ref(&first.id), &query())
            .await
            .unwrap_err()
            .status_code(),
        404
    );
    assert_eq!(
        service
            .get_vector_store_file(&first.id, &file.id)
            .await
            .unwrap_err()
            .status_code(),
        404
    );
    assert_eq!(
        service
            .vector_store_file_content(&first.id, &file.id)
            .await
            .unwrap_err()
            .status_code(),
        404
    );
    assert_eq!(
        service
            .update_vector_store(&first.id, request(json!({"expires_after":null})))
            .await
            .unwrap_err()
            .status_code(),
        404
    );
    assert_eq!(
        service
            .update_vector_store_file(
                &first.id,
                &file.id,
                UpdateVectorStoreFileRequest {
                    attributes: FileAttributes::default()
                }
            )
            .await
            .unwrap_err()
            .status_code(),
        404
    );
    assert_eq!(
        service
            .detach_file(&first.id, &file.id)
            .await
            .unwrap_err()
            .status_code(),
        404
    );
    assert_eq!(service.cleanup_expired_vector_stores(1).await.unwrap(), 1);
    assert_eq!(service.cleanup_expired_vector_stores(1).await.unwrap(), 0);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE store_id = $1")
        .bind(&first.id)
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(service.get_file(&file.id).await.unwrap().bytes, file.bytes);
    assert!(
        !service
            .search(std::slice::from_ref(&second.id), &query())
            .await
            .unwrap()
            .data
            .is_empty()
    );
    let text = service.vector_store_file_content(&second.id, &file.id).await.unwrap();
    assert_eq!(
        serde_json::to_value(&text).unwrap()["data"][0]["text"],
        "Original lunar policy.\n".repeat(100)
    );
    // Legacy request-shaped strategies remain readable but are never echoed as response options.
    let mut legacy = serde_json::to_value(service.get_vector_store_file(&second.id, &file.id).await.unwrap()).unwrap();
    for strategy in [
        json!({"type":"auto"}),
        json!({"type":"contextual","contextual":{"max_chunk_size_tokens":700,"chunk_overlap_tokens":400}}),
    ] {
        legacy["chunking_strategy"] = strategy;
        sqlx::query("UPDATE file_search_attachments SET data = $3 WHERE store_id = $1 AND file_id = $2")
            .bind(&second.id)
            .bind(&file.id)
            .bind(serde_json::to_string(&legacy).unwrap())
            .execute(pool.as_ref())
            .await
            .unwrap();
        let attachment = service.get_vector_store_file(&second.id, &file.id).await.unwrap();
        assert_eq!(
            serde_json::to_value(attachment).unwrap()["chunking_strategy"],
            json!({"type":"other"})
        );
    }
    // Legacy attachments recover original text from the uploaded bytes, never overlap chunks.
    sqlx::query("UPDATE file_search_attachments SET parsed_content = NULL WHERE store_id = $1")
        .bind(&second.id)
        .execute(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(service.vector_store_file_content(&second.id, &file.id).await.unwrap()).unwrap(),
        serde_json::to_value(text).unwrap()
    );
    service.delete_vector_store(&first.id).await.unwrap();
    service.delete_vector_store(&second.id).await.unwrap();
    service.delete_file(&file.id).await.unwrap();
}

#[tokio::test]
async fn sqlite_store_expiration_preserves_uploads_and_other_stores() {
    lifecycle("sqlite::memory:").await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL"]
async fn postgres_store_expiration_preserves_uploads_and_other_stores() {
    lifecycle(&std::env::var("TEST_POSTGRES_URL").unwrap()).await;
}

#[allow(
    clippy::too_many_lines,
    reason = "exercises one ordered mixed-status corpus through filtered updates and idle-store expiration"
)]
async fn updates_and_pages(database: &str) {
    let (service, pool, _files) = fixture(database).await;
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let mut ids = Vec::new();
    for status in [
        AttachmentStatus::Failed,
        AttachmentStatus::Completed,
        AttachmentStatus::Failed,
        AttachmentStatus::Completed,
        AttachmentStatus::Cancelled,
    ] {
        let file = service
            .upload_file("page.txt", "text/plain", "assistants", b"lunar page".to_vec())
            .await
            .unwrap();
        let mut attachment = service
            .attach_file(
                &store.id,
                AttachFileRequest {
                    file_id: file.id.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        attachment.status = status;
        sqlx::query("UPDATE file_search_attachments SET created_at = 100, status = $3, data = $4 WHERE store_id = $1 AND file_id = $2").bind(&store.id).bind(&file.id).bind(status.as_str()).bind(serde_json::to_string(&attachment).unwrap()).execute(pool.as_ref()).await.unwrap();
        ids.push(file.id);
    }
    let counts = service.get_vector_store(&store.id).await.unwrap().file_counts;
    assert_eq!(
        (counts.total, counts.completed, counts.failed, counts.cancelled),
        (5, 2, 2, 1)
    );
    let params = ListParams {
        filter: Some(AttachmentStatus::Completed),
        limit: Some(1),
        order: Some(ListOrder::Asc),
        ..Default::default()
    };
    let first = service.list_vector_store_files(&store.id, &params).await.unwrap();
    assert_eq!(first.data[0].id, ids[1]);
    assert!(first.has_more);
    let next = service
        .list_vector_store_files(
            &store.id,
            &ListParams {
                after: Some(ids[1].clone()),
                ..params.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(next.data[0].id, ids[3]);
    assert!(!next.has_more);
    let previous = service
        .list_vector_store_files(
            &store.id,
            &ListParams {
                before: Some(ids[3].clone()),
                ..params
            },
        )
        .await
        .unwrap();
    assert_eq!(previous.data[0].id, ids[1]);
    assert!(!previous.has_more);
    let hits = service.search(std::slice::from_ref(&store.id), &query()).await.unwrap();
    assert_eq!(hits.data.len(), 2, "only completed attachments are searchable");
    let attributes: FileAttributes = serde_json::from_value(json!({"kind":"selected"})).unwrap();
    service
        .update_vector_store_file(&store.id, &ids[1], UpdateVectorStoreFileRequest { attributes })
        .await
        .unwrap();
    let filter_query: SearchRequest =
        serde_json::from_value(json!({"query":"lunar","filters":{"type":"eq","key":"kind","value":"selected"}}))
            .unwrap();
    assert_eq!(
        service
            .search(std::slice::from_ref(&store.id), &filter_query)
            .await
            .unwrap()
            .data[0]
            .file_id,
        ids[1]
    );
    service
        .update_vector_store_file(
            &store.id,
            &ids[1],
            serde_json::from_value(json!({"attributes":null})).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        service
            .search(std::slice::from_ref(&store.id), &filter_query)
            .await
            .unwrap()
            .data
            .is_empty()
    );
    // Policy updates use prior activity rather than refreshing an idle store.
    sqlx::query("UPDATE file_search_stores SET last_active_at = 100 WHERE id = $1")
        .bind(&store.id)
        .execute(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(
        service.get_vector_store(&store.id).await.unwrap().last_active_at,
        Some(100)
    );
    let unchanged = service
        .update_vector_store(&store.id, request(json!({"name":"renamed","metadata":{"team":"docs"}})))
        .await
        .unwrap();
    assert_eq!(unchanged.last_active_at, Some(100));
    let expired = service
        .update_vector_store(
            &store.id,
            request(json!({"expires_after":{"anchor":"last_active_at","days":1}})),
        )
        .await
        .unwrap();
    assert_eq!(expired.expires_at, Some(86500));
    assert_eq!(expired.status, VectorStoreStatus::Expired);
    assert_eq!(
        service
            .update_vector_store(&store.id, request(json!({"expires_after":null})))
            .await
            .unwrap_err()
            .status_code(),
        404
    );
    service.delete_vector_store(&store.id).await.unwrap();
    for id in ids {
        service.delete_file(&id).await.unwrap();
    }
}

#[tokio::test]
async fn sqlite_status_filter_precedes_pagination_and_updates_do_not_refresh_activity() {
    updates_and_pages("sqlite::memory:").await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL"]
async fn postgres_status_filter_precedes_pagination_and_updates_do_not_refresh_activity() {
    updates_and_pages(&std::env::var("TEST_POSTGRES_URL").unwrap()).await;
}

async fn database_clock(pool: &DbPool) -> i64 {
    sqlx::query_scalar("SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp())) AS BIGINT)")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn wait_for_lock(pool: &DbPool, blocker: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE wait_event_type = 'Lock' AND CAST($1 AS INTEGER) = ANY(pg_blocking_pids(pid)))").bind(blocker).fetch_one(pool).await.unwrap();
            if waiting { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("operation must reach the contended store lock");
}

async fn wait_until(pool: &DbPool, deadline: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while database_clock(pool).await < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL"]
async fn postgres_expiry_is_rechecked_after_contended_store_lock_for_publication_and_policy() {
    let (service, pool, _files) = fixture(&std::env::var("TEST_POSTGRES_URL").unwrap()).await;
    for publication in [true, false] {
        let store = service
            .create_vector_store(CreateVectorStoreRequest::default())
            .await
            .unwrap();
        let file = service
            .upload_file("wait.txt", "text/plain", "assistants", b"lunar".to_vec())
            .await
            .unwrap();
        let deadline = database_clock(&pool).await + 3;
        sqlx::query("UPDATE file_search_stores SET expires_at = $2 WHERE id = $1")
            .bind(&store.id)
            .bind(deadline)
            .execute(pool.as_ref())
            .await
            .unwrap();
        let mut blocker = pool.begin().await.unwrap();
        let pid: i64 = sqlx::query_scalar("SELECT CAST(pg_backend_pid() AS BIGINT)")
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        sqlx::query("UPDATE file_search_stores SET id = id WHERE id = $1")
            .bind(&store.id)
            .execute(&mut *blocker)
            .await
            .unwrap();
        let worker = service.clone();
        let store_id = store.id.clone();
        let file_id = file.id.clone();
        let operation = tokio::spawn(async move {
            if publication {
                worker
                    .attach_file(
                        &store_id,
                        AttachFileRequest {
                            file_id,
                            ..Default::default()
                        },
                    )
                    .await
                    .map(|_| ())
            } else {
                worker
                    .update_vector_store(&store_id, request(json!({"expires_after":null})))
                    .await
                    .map(|_| ())
            }
        });
        wait_for_lock(&pool, pid).await;
        assert!(database_clock(&pool).await < deadline);
        wait_until(&pool, deadline).await;
        blocker.commit().await.unwrap();
        let result = operation.await.unwrap();
        let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE store_id = $1")
            .bind(&store.id)
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
        let expired = service.get_vector_store(&store.id).await.unwrap();
        service.delete_vector_store(&store.id).await.unwrap();
        service.delete_file(&file.id).await.unwrap();
        assert_eq!(result.unwrap_err().status_code(), 404);
        assert_eq!(expired.expires_at, Some(deadline));
        assert_eq!(expired.status, VectorStoreStatus::Expired);
        assert_eq!(chunks, 0);
    }
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL"]
async fn postgres_cleanup_rechecks_policy_after_waiting_for_committed_extension() {
    let (service, pool, _files) = fixture(&std::env::var("TEST_POSTGRES_URL").unwrap()).await;
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let file = service
        .upload_file("policy.txt", "text/plain", "assistants", b"lunar".to_vec())
        .await
        .unwrap();
    service
        .attach_file(
            &store.id,
            AttachFileRequest {
                file_id: file.id.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let deadline = database_clock(&pool).await + 2;
    sqlx::query("UPDATE file_search_stores SET expires_at = $2 WHERE id = $1")
        .bind(&store.id)
        .bind(deadline)
        .execute(pool.as_ref())
        .await
        .unwrap();
    // Hold the same store-row policy write as update_store before COMMIT. A sweeper sees
    // the previously committed deadline, waits, then must recheck the new policy.
    let mut policy = pool.begin().await.unwrap();
    let pid: i64 = sqlx::query_scalar("SELECT CAST(pg_backend_pid() AS BIGINT)")
        .fetch_one(&mut *policy)
        .await
        .unwrap();
    sqlx::query("UPDATE file_search_stores SET expires_after_days = 1, expires_at = $2 WHERE id = $1")
        .bind(&store.id)
        .bind(deadline + 86400)
        .execute(&mut *policy)
        .await
        .unwrap();
    wait_until(&pool, deadline).await;
    let worker = service.clone();
    let cleanup = tokio::spawn(async move { worker.cleanup_expired_vector_stores(1000).await });
    wait_for_lock(&pool, pid).await;
    policy.commit().await.unwrap();
    let count = cleanup.await.unwrap().unwrap();
    let current = service.get_vector_store(&store.id).await.unwrap();
    let content = service.vector_store_file_content(&store.id, &file.id).await;
    service.delete_vector_store(&store.id).await.unwrap();
    service.delete_file(&file.id).await.unwrap();
    assert_eq!(count, 0);
    assert_eq!(current.status, VectorStoreStatus::Completed);
    assert_eq!(current.file_counts.completed, 1);
    assert!(content.is_ok());
}

#[tokio::test]
async fn sqlite_ingestion_and_empty_search_refresh_activity_and_cleanup_is_bounded() {
    let (service, pool, _files) = fixture("sqlite::memory:").await;
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    sqlx::query("UPDATE file_search_stores SET last_active_at = 1 WHERE id = $1")
        .bind(&store.id)
        .execute(pool.as_ref())
        .await
        .unwrap();
    let file = service
        .upload_file("activity.txt", "text/plain", "assistants", b"lunar activity".to_vec())
        .await
        .unwrap();
    service
        .attach_file(
            &store.id,
            AttachFileRequest {
                file_id: file.id,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let active = service.get_vector_store(&store.id).await.unwrap();
    assert!(active.last_active_at.unwrap() > 1);
    sqlx::query("UPDATE file_search_stores SET last_active_at = 2 WHERE id = $1")
        .bind(&store.id)
        .execute(pool.as_ref())
        .await
        .unwrap();
    let miss = service
        .search(
            std::slice::from_ref(&store.id),
            &serde_json::from_value(json!({"query":"absent"})).unwrap(),
        )
        .await
        .unwrap();
    assert!(miss.data.is_empty());
    assert!(
        service
            .get_vector_store(&store.id)
            .await
            .unwrap()
            .last_active_at
            .unwrap()
            > 2
    );
    let second = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    sqlx::query("UPDATE file_search_stores SET expires_at = 1 WHERE id IN ($1, $2)")
        .bind(&store.id)
        .bind(&second.id)
        .execute(pool.as_ref())
        .await
        .unwrap();
    assert!(service.cleanup_expired_vector_stores(0).await.is_err());
    assert!(service.cleanup_expired_vector_stores(1001).await.is_err());
    assert_eq!(service.cleanup_expired_vector_stores(1).await.unwrap(), 1);
    assert_eq!(service.cleanup_expired_vector_stores(1).await.unwrap(), 1);
    assert_eq!(service.cleanup_expired_vector_stores(1).await.unwrap(), 0);
}

#[tokio::test]
async fn sqlite_cleanup_waiting_for_policy_extension_rechecks_the_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let (service, pool, _files) = fixture(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("lifecycle.db").display()
    ))
    .await;
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let deadline: i64 = sqlx::query_scalar("SELECT CAST(strftime('%s', 'now') AS BIGINT) + 1")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    sqlx::query("UPDATE file_search_stores SET expires_at = $2 WHERE id = $1")
        .bind(&store.id)
        .bind(deadline)
        .execute(pool.as_ref())
        .await
        .unwrap();
    let mut policy = pool.begin().await.unwrap();
    sqlx::query("UPDATE file_search_stores SET expires_after_days = 1, expires_at = $2 WHERE id = $1")
        .bind(&store.id)
        .bind(deadline + 86400)
        .execute(&mut *policy)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let mut cleanup = Box::pin(service.cleanup_expired_vector_stores(1));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut cleanup)
            .await
            .is_err()
    );
    policy.commit().await.unwrap();
    assert_eq!(cleanup.await.unwrap(), 0);
    assert_eq!(
        service.get_vector_store(&store.id).await.unwrap().status,
        VectorStoreStatus::Completed
    );
    pool.close().await;
}

#[tokio::test]
async fn legacy_large_chunk_content_recovery_does_not_rechunk() {
    let (service, pool, _files) = fixture("sqlite::memory:").await;
    // Each repeated word requires a cl100k token, exceeding the default 819,600-token
    // budget while remaining well inside the 4096/0 ingestion and extracted-byte limits.
    let text = "a ".repeat(900_000);
    let file = service
        .upload_file("large-legacy.txt", "text/plain", "assistants", text.as_bytes().to_vec())
        .await
        .unwrap();
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    service
        .attach_file(
            &store.id,
            AttachFileRequest {
                file_id: file.id.clone(),
                chunking_strategy: Some(ChunkingStrategy::Static {
                    config: StaticChunking {
                        max_chunk_size_tokens: 4096,
                        chunk_overlap_tokens: 0,
                    },
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Migration 0007 leaves this column null for existing attachments.
    sqlx::query("UPDATE file_search_attachments SET parsed_content = NULL WHERE store_id = $1 AND file_id = $2")
        .bind(&store.id)
        .bind(&file.id)
        .execute(pool.as_ref())
        .await
        .unwrap();
    let content = service.vector_store_file_content(&store.id, &file.id).await.unwrap();
    assert!(!content.has_more);
    assert!(content.next_page.is_none());
    assert_eq!(content.data.len(), 1);
    let ParsedFileContent::Text { text: recovered } = &content.data[0];
    assert_eq!(recovered, &text);
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL"]
async fn postgres_store_updates_round_trip_omitted_null_and_value() {
    let (service, _pool, _files) = fixture(&std::env::var("TEST_POSTGRES_URL").unwrap()).await;
    let store = service
        .create_vector_store(
            serde_json::from_value(json!({
                "name":"original", "metadata":{"purpose":"docs"},
                "expires_after":{"anchor":"last_active_at","days":2}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    service
        .update_vector_store(&store.id, request(json!({})))
        .await
        .unwrap();
    let omitted = service.get_vector_store(&store.id).await.unwrap();
    service
        .update_vector_store(
            &store.id,
            request(json!({
                "name":"changed", "metadata":{"team":"search"},
                "expires_after":{"anchor":"last_active_at","days":3}
            })),
        )
        .await
        .unwrap();
    let replaced = service.get_vector_store(&store.id).await.unwrap();
    service
        .update_vector_store(&store.id, request(json!({"metadata":null})))
        .await
        .unwrap();
    let cleared_metadata = service.get_vector_store(&store.id).await.unwrap();
    service
        .update_vector_store(&store.id, request(json!({"name":null,"expires_after":null})))
        .await
        .unwrap();
    let cleared = service.get_vector_store(&store.id).await.unwrap();
    service
        .update_vector_store(&store.id, request(json!({})))
        .await
        .unwrap();
    let omitted_after_null = service.get_vector_store(&store.id).await.unwrap();
    service
        .update_vector_store(
            &store.id,
            request(json!({
                "name":"restored", "metadata":{"revision":"2"},
                "expires_after":{"anchor":"last_active_at","days":1}
            })),
        )
        .await
        .unwrap();
    let restored = service.get_vector_store(&store.id).await.unwrap();
    service.delete_vector_store(&store.id).await.unwrap();

    let activity = store.last_active_at.unwrap();
    assert_eq!(omitted.name, "original");
    assert_eq!(
        serde_json::to_value(&omitted.metadata).unwrap(),
        json!({"purpose":"docs"})
    );
    assert_eq!(omitted.expires_after.unwrap().days, 2);
    assert_eq!(omitted.expires_at, Some(activity + 172_800));
    assert_eq!(replaced.name, "changed");
    assert_eq!(
        serde_json::to_value(&replaced.metadata).unwrap(),
        json!({"team":"search"})
    );
    assert_eq!(replaced.expires_after.unwrap().days, 3);
    assert_eq!(replaced.expires_at, Some(activity + 259_200));
    assert_eq!(cleared_metadata.name, "changed");
    assert!(cleared_metadata.metadata.is_none());
    assert_eq!(cleared_metadata.expires_after.unwrap().days, 3);
    assert_eq!(cleared_metadata.expires_at, Some(activity + 259_200));
    for object in [cleared, omitted_after_null] {
        assert_eq!(object.name, "");
        assert!(object.metadata.is_none());
        assert!(object.expires_after.is_none());
        assert!(object.expires_at.is_none());
        assert_eq!(object.last_active_at, Some(activity));
        assert_eq!(object.status, VectorStoreStatus::Completed);
    }
    assert_eq!(restored.name, "restored");
    assert_eq!(
        serde_json::to_value(&restored.metadata).unwrap(),
        json!({"revision":"2"})
    );
    assert_eq!(restored.expires_after.unwrap().days, 1);
    assert_eq!(restored.expires_at, Some(activity + 86400));
    assert_eq!(restored.last_active_at, Some(activity));
}
