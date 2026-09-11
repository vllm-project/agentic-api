use agentic_core::storage::create_pool_with_schema;
use agentic_core::tool::file_search::FileSearchService;
use agentic_core::types::file_search::*;
#[cfg(feature = "file-search-pdf")]
use std::fmt::Write as _;
use std::sync::Arc;

struct TestService {
    service: FileSearchService,
    _files: tempfile::TempDir,
}

impl std::ops::Deref for TestService {
    type Target = FileSearchService;
    fn deref(&self) -> &Self::Target {
        &self.service
    }
}

fn file_config(files: &tempfile::TempDir) -> FileSearchConfig {
    FileSearchConfig {
        files_storage_dir: Some(files.path().to_owned()),
        ..FileSearchConfig::default()
    }
}

async fn service() -> TestService {
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let files = tempfile::tempdir().unwrap();
    let service = FileSearchService::new(pool, Arc::new(reqwest::Client::new()), file_config(&files)).unwrap();
    TestService { service, _files: files }
}

fn query(text: &str) -> SearchRequest {
    SearchRequest {
        query: SearchQuery::Text(text.into()),
        ..SearchRequest::default()
    }
}

#[tokio::test]
async fn local_files_store_generated_keys_restart_and_delete() {
    let files = tempfile::tempdir().unwrap();
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let config = file_config(&files);
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config.clone()).unwrap();
    let file = service
        .upload_file("../../outside.txt", "text/plain", "assistants", b"on disk".to_vec())
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(files.path().join(&file.id)).await.unwrap(), b"on disk");
    let encoded: String = sqlx::query_scalar("SELECT content_base64 FROM file_search_files WHERE id = $1")
        .bind(&file.id)
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert!(encoded.is_empty(), "new uploads must not duplicate bytes in SQL");
    assert_eq!(std::fs::read_dir(files.path()).unwrap().count(), 1);
    drop(service);
    let restarted = FileSearchService::new(pool, Arc::new(reqwest::Client::new()), config).unwrap();
    assert_eq!(restarted.file_content(&file.id).await.unwrap(), b"on disk");
    restarted.delete_file(&file.id).await.unwrap();
    assert!(!files.path().join(&file.id).exists());
    assert_eq!(restarted.get_file(&file.id).await.unwrap_err().status_code(), 404);
}

#[tokio::test]
async fn local_files_accept_binary_uploads_but_validate_ingestion() {
    let service = service().await;
    let bytes = vec![0, 255, 128, 1];
    let file = service
        .upload_file("payload.bin", "application/octet-stream", "user_data", bytes.clone())
        .await
        .unwrap();
    assert_eq!(service.file_content(&file.id).await.unwrap(), bytes);
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    assert_eq!(
        service
            .attach_file(
                &store.id,
                AttachFileRequest {
                    file_id: file.id,
                    ..AttachFileRequest::default()
                }
            )
            .await
            .unwrap_err()
            .status_code(),
        400
    );
}

#[tokio::test]
async fn local_files_reject_malformed_keys_and_missing_blobs_are_storage_failures() {
    let files = tempfile::tempdir().unwrap();
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let service = FileSearchService::new(pool, Arc::new(reqwest::Client::new()), file_config(&files)).unwrap();
    for key in [
        "../outside",
        "/tmp/outside",
        "file-../outside",
        "file-invalid",
        "file-00000000-0000-0000-0000-000000000000/child",
    ] {
        assert_eq!(service.file_content(key).await.unwrap_err().status_code(), 400);
        assert_eq!(service.delete_file(key).await.unwrap_err().status_code(), 400);
    }
    let file = service
        .upload_file("lost.txt", "text/plain", "assistants", b"lost".to_vec())
        .await
        .unwrap();
    tokio::fs::remove_file(files.path().join(&file.id)).await.unwrap();
    assert_eq!(service.file_content(&file.id).await.unwrap_err().status_code(), 503);
    assert_eq!(service.get_file(&file.id).await.unwrap().id, file.id);
}

#[tokio::test]
async fn local_files_publication_failure_and_cancellation_clean_up_bytes() {
    let files = tempfile::tempdir().unwrap();
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), file_config(&files)).unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_upload BEFORE INSERT ON file_search_files BEGIN SELECT RAISE(ABORT, 'injected upload failure'); END;").execute(pool.as_ref()).await.unwrap();
    assert!(
        service
            .upload_file("fail.txt", "text/plain", "assistants", b"failure".to_vec())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(files.path()).unwrap().count(), 0);
    sqlx::raw_sql("DROP TRIGGER reject_upload;")
        .execute(pool.as_ref())
        .await
        .unwrap();
    let connection = pool.acquire().await.unwrap();
    let cloned = service.clone();
    let upload = tokio::spawn(async move {
        cloned
            .upload_file("cancel.txt", "text/plain", "assistants", b"cancelled".to_vec())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while std::fs::read_dir(files.path()).unwrap().count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    upload.abort();
    assert!(upload.await.unwrap_err().is_cancelled());
    drop(connection);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while std::fs::read_dir(files.path()).unwrap().count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        service
            .list_files(&ListParams::default())
            .await
            .unwrap()
            .data
            .is_empty()
    );
}

#[tokio::test]
async fn local_files_preserve_legacy_database_content() {
    use base64::Engine as _;
    let files = tempfile::tempdir().unwrap();
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), file_config(&files)).unwrap();
    let file = FileObject {
        id: format!("file-{}", uuid::Uuid::now_v7()),
        object: "file".into(),
        bytes: 11,
        created_at: 0,
        filename: "legacy.txt".into(),
        purpose: "assistants".into(),
        status: "processed".into(),
    };
    sqlx::query("INSERT INTO file_search_files (id, created_at, data, content_type, content_base64) VALUES ($1, 0, $2, 'text/plain', $3)")
        .bind(&file.id).bind(serde_json::to_string(&file).unwrap()).bind(base64::engine::general_purpose::STANDARD.encode(b"legacy reef")).execute(pool.as_ref()).await.unwrap();
    assert_eq!(service.file_content(&file.id).await.unwrap(), b"legacy reef");
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    service
        .attach_file(
            &store.id,
            AttachFileRequest {
                file_id: file.id.clone(),
                ..AttachFileRequest::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(service.search(&[store.id], &query("reef")).await.unwrap().data.len(), 1);
    service.delete_file(&file.id).await.unwrap();
    assert_eq!(std::fs::read_dir(files.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn local_files_do_not_follow_blob_symlinks_or_read_oversized_replacements() {
    let files = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    tokio::fs::write(outside.path(), b"private outside content")
        .await
        .unwrap();
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let service = FileSearchService::new(pool, Arc::new(reqwest::Client::new()), file_config(&files)).unwrap();
    let file = service
        .upload_file("safe.txt", "text/plain", "assistants", b"safe".to_vec())
        .await
        .unwrap();
    let path = files.path().join(&file.id);
    tokio::fs::remove_file(&path).await.unwrap();
    std::os::unix::fs::symlink(outside.path(), &path).unwrap();
    assert_eq!(service.file_content(&file.id).await.unwrap_err().status_code(), 503);
    tokio::fs::remove_file(&path).await.unwrap();
    let oversized = tokio::fs::File::create(&path).await.unwrap();
    oversized
        .set_len((agentic_core::tool::file_search::MAX_FILE_BYTES + 1) as u64)
        .await
        .unwrap();
    assert_eq!(service.file_content(&file.id).await.unwrap_err().status_code(), 503);
    service.delete_file(&file.id).await.unwrap();
    assert_eq!(
        tokio::fs::read(outside.path()).await.unwrap(),
        b"private outside content"
    );
}

#[tokio::test]
async fn uploads_keep_capacity_reserved_while_waiting_for_database() {
    let files = tempfile::tempdir().unwrap();
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), file_config(&files)).unwrap();
    let connection = pool.acquire().await.unwrap();
    let mut uploads =
        Box::pin(futures::future::join_all((0..4).map(|_| {
            service.upload_file("queued.txt", "text/plain", "assistants", b"queued".to_vec())
        })));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut uploads)
            .await
            .is_err()
    );
    let overflow = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        service.upload_file("overflow.txt", "text/plain", "assistants", b"overflow".to_vec()),
    )
    .await;
    drop(connection);
    assert!(uploads.await.into_iter().all(|result| result.is_ok()));
    assert_eq!(
        overflow
            .expect("busy upload must fail without waiting for SQL")
            .unwrap_err()
            .status_code(),
        503
    );
}

#[tokio::test]
async fn before_pagination_returns_adjacent_files_stores_and_attachments() {
    let service = service().await;
    let mut stores = Vec::new();
    let mut files = Vec::new();
    for _ in 0..6 {
        stores.push(
            service
                .create_vector_store(CreateVectorStoreRequest::default())
                .await
                .unwrap()
                .id,
        );
        files.push(
            service
                .upload_file("page.txt", "text/plain", "assistants", b"pagination".to_vec())
                .await
                .unwrap()
                .id,
        );
    }
    for file in &files {
        service
            .attach_file(
                &stores[0],
                AttachFileRequest {
                    file_id: file.clone(),
                    ..AttachFileRequest::default()
                },
            )
            .await
            .unwrap();
    }
    for order in [ListOrder::Asc, ListOrder::Desc] {
        let full = ListParams {
            order: Some(order),
            ..ListParams::default()
        };
        let file_ids: Vec<_> = service
            .list_files(&full)
            .await
            .unwrap()
            .data
            .into_iter()
            .map(|file| file.id)
            .collect();
        let store_ids: Vec<_> = service
            .list_vector_stores(&full)
            .await
            .unwrap()
            .data
            .into_iter()
            .map(|store| store.id)
            .collect();
        let attachment_ids: Vec<_> = service
            .list_vector_store_files(&stores[0], &full)
            .await
            .unwrap()
            .data
            .into_iter()
            .map(|file| file.id)
            .collect();
        let before = |id: &String| ListParams {
            limit: Some(2),
            before: Some(id.clone()),
            order: Some(order),
            after: None,
        };
        let page = service.list_files(&before(&file_ids[5])).await.unwrap();
        assert!(page.has_more);
        assert_eq!(
            page.data.into_iter().map(|file| file.id).collect::<Vec<_>>(),
            file_ids[3..5]
        );
        let page = service.list_vector_stores(&before(&store_ids[5])).await.unwrap();
        assert!(page.has_more);
        assert_eq!(
            page.data.into_iter().map(|store| store.id).collect::<Vec<_>>(),
            store_ids[3..5]
        );
        let page = service
            .list_vector_store_files(&stores[0], &before(&attachment_ids[5]))
            .await
            .unwrap();
        assert!(page.has_more);
        assert_eq!(
            page.data.into_iter().map(|file| file.id).collect::<Vec<_>>(),
            attachment_ids[3..5]
        );
    }
}

#[tokio::test]
async fn upload_attach_search_survives_service_recreation_and_deletion() {
    let files = tempfile::tempdir().unwrap();
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let client = Arc::new(reqwest::Client::new());
    let service = FileSearchService::new(pool.clone(), client.clone(), file_config(&files)).unwrap();
    let bytes = b"The conservation policy protects coral reefs and marine ecosystems.".to_vec();
    let file = service
        .upload_file("ocean.txt", "text/plain", "assistants", bytes.clone())
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
                ..AttachFileRequest::default()
            },
        )
        .await
        .unwrap();
    drop(service);
    let recreated = FileSearchService::new(pool, client, file_config(&files)).unwrap();
    assert_eq!(recreated.file_content(&file.id).await.unwrap(), bytes);
    let found = recreated
        .search(std::slice::from_ref(&store.id), &query("coral"))
        .await
        .unwrap();
    assert_eq!(found.data.len(), 1);
    assert_eq!(found.data[0].file_id, file.id);
    assert_eq!(found.data[0].filename, "ocean.txt");
    assert!(found.data[0].content[0].text.contains("coral reefs"));
    recreated.delete_file(&file.id).await.unwrap();
    assert_eq!(
        recreated.get_vector_store(&store.id).await.unwrap().file_counts.total,
        0
    );
    assert!(
        recreated
            .search(&[store.id], &query("coral"))
            .await
            .unwrap()
            .data
            .is_empty()
    );
}

#[tokio::test]
async fn invalid_search_and_unsupported_options_are_explicit() {
    let service = service().await;
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    for request in [
        SearchRequest::default(),
        SearchRequest {
            max_num_results: Some(0),
            ..query("valid")
        },
        SearchRequest {
            rewrite_query: true,
            ..query("valid")
        },
        SearchRequest {
            ranking_options: Some(RankingOptions {
                ranker: Some("neural".into()),
                ..RankingOptions::default()
            }),
            ..query("valid")
        },
    ] {
        assert_eq!(
            service
                .search(std::slice::from_ref(&store.id), &request)
                .await
                .unwrap_err()
                .status_code(),
            400
        );
    }
    assert_eq!(
        service
            .search(
                &[store.id],
                &SearchRequest {
                    search_mode: Some(SearchMode::Semantic),
                    ..query("coral")
                }
            )
            .await
            .unwrap_err()
            .status_code(),
        503
    );
}

#[tokio::test]
async fn file_search_persistence_schema_is_available() {
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let tables: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'file_search_files'")
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
    assert_eq!(tables, 1, "durable uploaded file storage must be migrated");
}

async fn attach_text(
    service: &FileSearchService,
    store_id: &str,
    filename: &str,
    text: &str,
    attributes: FileAttributes,
) -> FileObject {
    let file = service
        .upload_file(filename, "text/plain", "assistants", text.as_bytes().to_vec())
        .await
        .unwrap();
    service
        .attach_file(
            store_id,
            AttachFileRequest {
                file_id: file.id.clone(),
                attributes,
                chunking_strategy: None,
            },
        )
        .await
        .unwrap();
    file
}

#[tokio::test]
async fn filters_global_limits_and_duplicate_stores_do_not_leak_other_files() {
    let service = service().await;
    let first = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let second = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let allowed: FileAttributes = serde_json::from_str(r#"{"team":"ocean","year":2026,"published":true}"#).unwrap();
    let excluded: FileAttributes = serde_json::from_str(r#"{"team":"private","year":2020,"published":false}"#).unwrap();
    let file = attach_text(
        &service,
        &first.id,
        "ocean.txt",
        "coral habitat conservation",
        allowed.clone(),
    )
    .await;
    service
        .attach_file(
            &second.id,
            AttachFileRequest {
                file_id: file.id.clone(),
                attributes: allowed,
                chunking_strategy: None,
            },
        )
        .await
        .unwrap();
    attach_text(
        &service,
        &second.id,
        "secret.txt",
        "coral coral coral confidential",
        excluded,
    )
    .await;
    let request = SearchRequest {query: SearchQuery::Texts(vec!["coral".into(), "conservation".into()]), filters: Some(serde_json::from_str(r#"{"type":"and","filters":[{"type":"eq","key":"team","value":"ocean"},{"type":"or","filters":[{"type":"gte","key":"year","value":2025},{"type":"eq","key":"published","value":true}]}]}"#).unwrap()), max_num_results: Some(1), ..SearchRequest::default()};
    let results = service
        .search(&[first.id.clone(), second.id.clone(), second.id], &request)
        .await
        .unwrap();
    assert_eq!(results.data.len(), 1);
    assert_eq!(results.data[0].file_id, file.id);
    assert_eq!(results.data[0].filename, "ocean.txt");
    assert_eq!(
        results.data[0].attributes.get("team"),
        Some(&AttributeValue::String("ocean".into()))
    );
    assert!(
        service
            .search(&[first.id], &query("unrelated"))
            .await
            .unwrap()
            .data
            .is_empty()
    );
}

#[tokio::test]
async fn attachment_conflicts_detachment_and_pagination_are_consistent() {
    let service = service().await;
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let file = attach_text(&service, &store.id, "a.txt", "first coral", FileAttributes::default()).await;
    assert_eq!(
        service
            .attach_file(
                &store.id,
                AttachFileRequest {
                    file_id: file.id.clone(),
                    ..AttachFileRequest::default()
                }
            )
            .await
            .unwrap_err()
            .status_code(),
        409
    );
    attach_text(&service, &store.id, "b.txt", "second coral", FileAttributes::default()).await;
    attach_text(&service, &store.id, "c.txt", "third coral", FileAttributes::default()).await;
    let first = service
        .list_files(&ListParams {
            limit: Some(2),
            order: Some(ListOrder::Asc),
            ..ListParams::default()
        })
        .await
        .unwrap();
    assert!(first.has_more);
    assert_eq!(first.first_id.as_deref(), Some(file.id.as_str()));
    let second = service
        .list_files(&ListParams {
            limit: Some(2),
            order: Some(ListOrder::Asc),
            after: first.last_id,
            ..ListParams::default()
        })
        .await
        .unwrap();
    assert!(!second.has_more);
    assert_eq!(second.data.len(), 1);
    assert_eq!(second.data[0].filename, "c.txt");
    assert_eq!(service.get_vector_store(&store.id).await.unwrap().file_counts.total, 3);
    service.detach_file(&store.id, &file.id).await.unwrap();
    assert!(service.get_file(&file.id).await.is_ok());
    assert_eq!(service.get_vector_store(&store.id).await.unwrap().file_counts.total, 2);
    assert_eq!(
        service
            .get_vector_store_file(&store.id, &file.id)
            .await
            .unwrap_err()
            .status_code(),
        404
    );
    service.delete_vector_store(&store.id).await.unwrap();
    assert!(service.get_file(&file.id).await.is_ok());
    assert_eq!(
        service
            .search(&[store.id], &query("coral"))
            .await
            .unwrap_err()
            .status_code(),
        404
    );
}

#[tokio::test]
async fn failed_initial_ingestion_publishes_neither_store_nor_chunks() {
    let service = service().await;
    let file = service
        .upload_file("empty.txt", "text/plain", "assistants", b"   ".to_vec())
        .await
        .unwrap();
    assert!(
        service
            .create_vector_store(CreateVectorStoreRequest {
                file_ids: vec![file.id.clone()],
                ..CreateVectorStoreRequest::default()
            })
            .await
            .is_err()
    );
    assert!(
        service
            .list_vector_stores(&ListParams::default())
            .await
            .unwrap()
            .data
            .is_empty()
    );
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    assert!(
        service
            .attach_file(
                &store.id,
                AttachFileRequest {
                    file_id: file.id,
                    ..AttachFileRequest::default()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(service.get_vector_store(&store.id).await.unwrap().file_counts.total, 0);
    assert!(
        service
            .list_vector_store_files(&store.id, &ListParams::default())
            .await
            .unwrap()
            .data
            .is_empty()
    );
}

#[test]
fn filter_types_and_recursive_limits_are_enforced() {
    for filter in [
        r#"{"type":"eq","key":"a","value":[1]}"#,
        r#"{"type":"gt","key":"a","value":true}"#,
        r#"{"type":"in","key":"a","value":1}"#,
        r#"{"type":"or","filters":[]}"#,
    ] {
        let filter: SearchFilter = serde_json::from_str(filter).unwrap();
        assert!(
            SearchRequest {
                filters: Some(filter),
                ..query("anything")
            }
            .validate()
            .is_err()
        );
    }
    let mut filter: SearchFilter = serde_json::from_str(r#"{"type":"eq","key":"a","value":1}"#).unwrap();
    for _ in 0..10 {
        filter = SearchFilter::Compound(CompoundFilter {
            operator: CompoundOperator::And,
            filters: vec![filter],
        });
    }
    assert!(
        SearchRequest {
            filters: Some(filter),
            ..query("anything")
        }
        .validate()
        .is_err()
    );
    assert!(
        serde_json::from_str::<AttachFileRequest>(r#"{"file_id":"file-1","options":{"contextual":true}}"#).is_err()
    );
}

#[derive(Clone, Copy)]
enum ProviderMode {
    Good,
    DuplicateIndex,
    WrongDimensions,
    Fail,
    Wait,
}
#[derive(Clone)]
struct ProviderState {
    mode: Arc<std::sync::Mutex<ProviderMode>>,
    started: Arc<tokio::sync::Notify>,
}
#[derive(serde::Deserialize)]
struct EmbeddingInput {
    model: String,
    input: Vec<String>,
}

async fn provider(
    axum::extract::State(state): axum::extract::State<ProviderState>,
    axum::Json(input): axum::Json<EmbeddingInput>,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    let mode = *state.mode.lock().unwrap();
    if matches!(mode, ProviderMode::Wait) {
        state.started.notify_one();
        std::future::pending::<()>().await;
    }
    if matches!(mode, ProviderMode::Fail) {
        return (
            axum::http::StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({"error":"secret-api-key-private-provider-response"})),
        );
    }
    let data: Vec<_> = input.input.iter().enumerate().map(|(index, text)| {
        let vector = if matches!(mode, ProviderMode::WrongDimensions) {vec![1.0, 0.0, 0.0]} else if text.contains("coral") || text.contains("aquatic") {vec![1.0, 0.0]} else {vec![0.0, 1.0]};
        serde_json::json!({"index": if matches!(mode, ProviderMode::DuplicateIndex) {1} else {index}, "embedding":vector})
    }).collect();
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({"model":input.model,"data":data})),
    )
}

async fn embedding_service() -> (
    TestService,
    ProviderState,
    tokio::task::JoinHandle<()>,
    Arc<agentic_core::storage::DbPool>,
    FileSearchConfig,
) {
    let state = ProviderState {
        mode: Arc::new(std::sync::Mutex::new(ProviderMode::Good)),
        started: Arc::new(tokio::sync::Notify::new()),
    };
    let app = axum::Router::new()
        .route("/v1/embeddings", axum::routing::post(provider))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let files = tempfile::tempdir().unwrap();
    let config = FileSearchConfig {
        files_storage_dir: Some(files.path().to_owned()),
        embedding_base_url: Some(format!("http://{addr}/v1")),
        embedding_model: Some("fixture".into()),
        embedding_api_key: Some("never-log-this-key".into()),
        ..FileSearchConfig::default()
    };
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config.clone()).unwrap();
    (TestService { service, _files: files }, state, task, pool, config)
}

#[tokio::test]
async fn semantic_retrieval_matches_without_lexical_overlap_and_hybrid_obeys_weights() {
    let (service, _, task, _, _) = embedding_service().await;
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let coral = attach_text(
        &service,
        &store.id,
        "coral.txt",
        "coral reefs",
        FileAttributes::default(),
    )
    .await;
    attach_text(&service, &store.id, "sky.txt", "blue clouds", FileAttributes::default()).await;
    let semantic = SearchRequest {
        search_mode: Some(SearchMode::Semantic),
        ..query("aquatic")
    };
    let result = service
        .search(std::slice::from_ref(&store.id), &semantic)
        .await
        .unwrap();
    assert_eq!(result.data.len(), 1);
    assert_eq!(result.data[0].file_id, coral.id);
    assert!((result.data[0].score - 1.0).abs() < f64::EPSILON);
    assert!(
        service
            .search(
                std::slice::from_ref(&store.id),
                &SearchRequest {
                    search_mode: Some(SearchMode::Keyword),
                    ..query("aquatic")
                }
            )
            .await
            .unwrap()
            .data
            .is_empty()
    );
    let hybrid = SearchRequest {
        search_mode: Some(SearchMode::Hybrid),
        ranking_options: Some(RankingOptions {
            hybrid_search: Some(HybridSearchOptions {
                embedding_weight: Some(1.0),
                text_weight: Some(0.0),
            }),
            ..RankingOptions::default()
        }),
        ..query("aquatic")
    };
    assert_eq!(
        service.search(&[store.id], &hybrid).await.unwrap().data[0].file_id,
        coral.id
    );
    task.abort();
}

#[tokio::test]
async fn embedding_failures_and_cancellation_never_publish_partial_ingestion() {
    let (service, state, task, _, _) = embedding_service().await;
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let file = service
        .upload_file("coral.txt", "text/plain", "assistants", b"coral ecosystems".to_vec())
        .await
        .unwrap();
    for mode in [ProviderMode::DuplicateIndex, ProviderMode::Fail] {
        *state.mode.lock().unwrap() = mode;
        let error = service
            .attach_file(
                &store.id,
                AttachFileRequest {
                    file_id: file.id.clone(),
                    ..AttachFileRequest::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.status_code(), 502);
        assert!(!error.public_message().contains("secret"));
        assert!(!format!("{error:?}").contains("never-log-this-key"));
        assert_eq!(service.get_vector_store(&store.id).await.unwrap().file_counts.total, 0);
    }
    *state.mode.lock().unwrap() = ProviderMode::Wait;
    let work_service = service.clone();
    let store_id = store.id.clone();
    let work = tokio::spawn(async move {
        work_service
            .attach_file(
                &store_id,
                AttachFileRequest {
                    file_id: file.id,
                    ..AttachFileRequest::default()
                },
            )
            .await
    });
    state.started.notified().await;
    work.abort();
    assert!(work.await.unwrap_err().is_cancelled());
    assert!(
        service
            .list_vector_store_files(&store.id, &ListParams::default())
            .await
            .unwrap()
            .data
            .is_empty()
    );
    assert!(
        service
            .search(
                &[store.id],
                &SearchRequest {
                    search_mode: Some(SearchMode::Keyword),
                    ..query("coral")
                }
            )
            .await
            .unwrap()
            .data
            .is_empty()
    );
    task.abort();
}

#[tokio::test]
async fn embedding_identity_and_dimensions_survive_restart() {
    let (service, state, task, pool, mut config) = embedding_service().await;
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    attach_text(
        &service,
        &store.id,
        "coral.txt",
        "coral ecosystems",
        FileAttributes::default(),
    )
    .await;
    *state.mode.lock().unwrap() = ProviderMode::WrongDimensions;
    assert_eq!(
        service
            .search(std::slice::from_ref(&store.id), &query("aquatic"))
            .await
            .unwrap_err()
            .status_code(),
        502
    );
    config.embedding_model = Some("other-model".into());
    let restarted = FileSearchService::new(pool, Arc::new(reqwest::Client::new()), config).unwrap();
    assert_eq!(
        restarted
            .search(std::slice::from_ref(&store.id), &query("aquatic"))
            .await
            .unwrap_err()
            .status_code(),
        409
    );
    assert_eq!(
        restarted
            .search(
                &[store.id],
                &SearchRequest {
                    search_mode: Some(SearchMode::Keyword),
                    ..query("coral")
                }
            )
            .await
            .unwrap()
            .data
            .len(),
        1
    );
    task.abort();
}

#[cfg(feature = "file-search-pdf")]
fn text_pdf(text: &str) -> Vec<u8> {
    let stream = format!("BT /F1 12 Tf 72 700 Td ({text}) Tj ET");
    let objects = ["<< /Type /Catalog /Pages 2 0 R >>".to_owned(), "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(), "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_owned(), format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()), "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned()];
    let mut document = "%PDF-1.4\n".to_owned();
    let mut offsets = vec![0usize];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(document.len());
        write!(document, "{} 0 obj\n{object}\nendobj\n", index + 1).unwrap();
    }
    let xref = document.len();
    write!(document, "xref\n0 {}\n0000000000 65535 f \n", offsets.len()).unwrap();
    for offset in offsets.iter().skip(1) {
        writeln!(document, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        document,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        offsets.len()
    )
    .unwrap();
    document.into_bytes()
}

#[tokio::test]
#[cfg(feature = "file-search-pdf")]
async fn pdf_text_is_extracted_and_original_pdf_remains_durable() {
    let service = service().await;
    let bytes = text_pdf("Coral ecosystems protect coastal biodiversity.");
    let file = service
        .upload_file("research.pdf", "application/pdf", "assistants", bytes.clone())
        .await
        .unwrap();
    let store = service
        .create_vector_store(CreateVectorStoreRequest {
            file_ids: vec![file.id.clone()],
            ..CreateVectorStoreRequest::default()
        })
        .await
        .unwrap();
    let result = service.search(&[store.id], &query("biodiversity")).await.unwrap();
    assert_eq!(result.data.len(), 1);
    assert_eq!(result.data[0].filename, "research.pdf");
    assert!(result.data[0].content[0].text.contains("Coral ecosystems"));
    assert_eq!(service.file_content(&file.id).await.unwrap(), bytes);
}

#[tokio::test]
async fn database_failure_mid_publication_rolls_back_attachment_and_earlier_chunks() {
    let files = tempfile::tempdir().unwrap();
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), file_config(&files)).unwrap();
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let text = "coral habitat conservation ".repeat(300);
    let file = service
        .upload_file("large.txt", "text/plain", "assistants", text.into_bytes())
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_second_chunk BEFORE INSERT ON file_search_chunks WHEN NEW.chunk_index = 1 BEGIN SELECT RAISE(ABORT, 'injected publication failure'); END;").execute(pool.as_ref()).await.unwrap();
    let error = service
        .attach_file(
            &store.id,
            AttachFileRequest {
                file_id: file.id,
                chunking_strategy: Some(ChunkingStrategy::Static {
                    config: StaticChunking {
                        max_chunk_size_tokens: 100,
                        chunk_overlap_tokens: 0,
                    },
                }),
                ..AttachFileRequest::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.status_code(), 500);
    let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    let attachments: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_attachments")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(chunks, 0);
    assert_eq!(attachments, 0);
    assert_eq!(service.get_vector_store(&store.id).await.unwrap().file_counts.total, 0);
}

#[test]
fn ogx_vector_mode_alias_and_secret_safe_debug_are_supported() {
    assert_eq!(
        serde_json::from_str::<SearchMode>("\"vector\"").unwrap(),
        SearchMode::Semantic
    );
    let config = FileSearchConfig {
        files_storage_dir: None,
        embedding_base_url: Some("https://private.example/v1".into()),
        embedding_model: Some("model".into()),
        embedding_api_key: Some("secret-credential".into()),
        ..FileSearchConfig::default()
    };
    assert!(!format!("{config:?}").contains("secret-credential"));
}

#[tokio::test]
async fn uploaded_bytes_and_search_survive_closing_and_reopening_database_pool() {
    let files = tempfile::tempdir().unwrap();
    let path = std::env::temp_dir().join(format!("file-search-restart-{}.db", uuid::Uuid::now_v7()));
    let url = format!("sqlite://{}", path.display());
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), file_config(&files)).unwrap();
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let file = attach_text(
        &service,
        &store.id,
        "persistent.txt",
        "coral persisted across connections",
        FileAttributes::default(),
    )
    .await;
    drop(service);
    pool.close().await;
    drop(pool);
    let reopened = create_pool_with_schema(Some(&url)).await.unwrap();
    let service =
        FileSearchService::new(reopened.clone(), Arc::new(reqwest::Client::new()), file_config(&files)).unwrap();
    assert_eq!(
        service.file_content(&file.id).await.unwrap(),
        b"coral persisted across connections"
    );
    assert_eq!(
        service.search(&[store.id], &query("coral")).await.unwrap().data[0].file_id,
        file.id
    );
    drop(service);
    reopened.close().await;
    drop(reopened);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn score_threshold_and_global_result_limit_apply_after_multiquery_merge() {
    let service = service().await;
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    attach_text(
        &service,
        &store.id,
        "a.txt",
        "coral ecosystems",
        FileAttributes::default(),
    )
    .await;
    attach_text(
        &service,
        &store.id,
        "b.txt",
        "marine conservation",
        FileAttributes::default(),
    )
    .await;
    let request = SearchRequest {
        query: SearchQuery::Texts(vec!["coral".into(), "marine".into()]),
        max_num_results: Some(1),
        ..SearchRequest::default()
    };
    assert_eq!(
        service
            .search(std::slice::from_ref(&store.id), &request)
            .await
            .unwrap()
            .data
            .len(),
        1
    );
    let request = SearchRequest {
        ranking_options: Some(RankingOptions {
            score_threshold: Some(1.0),
            ..RankingOptions::default()
        }),
        ..request
    };
    assert!(service.search(&[store.id], &request).await.unwrap().data.is_empty());
}

#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
async fn postgres_file_search_persists_paginates_and_cascades_deletion() {
    let files = tempfile::tempdir().unwrap();
    let url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), file_config(&files)).unwrap();
    let store = service
        .create_vector_store(CreateVectorStoreRequest {
            name: Some("PostgreSQL file search verification".into()),
            ..CreateVectorStoreRequest::default()
        })
        .await
        .unwrap();
    let file = attach_text(
        &service,
        &store.id,
        "postgres.txt",
        "coral survives persistent PostgreSQL storage",
        FileAttributes::default(),
    )
    .await;
    let other = attach_text(
        &service,
        &store.id,
        "other.txt",
        "marine policy",
        FileAttributes::default(),
    )
    .await;
    let page = service
        .list_vector_store_files(
            &store.id,
            &ListParams {
                limit: Some(1),
                order: Some(ListOrder::Asc),
                ..ListParams::default()
            },
        )
        .await
        .unwrap();
    assert!(page.has_more);
    assert_eq!(page.data[0].id, file.id);
    let next = service
        .list_vector_store_files(
            &store.id,
            &ListParams {
                limit: Some(1),
                order: Some(ListOrder::Asc),
                after: page.last_id,
                ..ListParams::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(next.data[0].id, other.id);
    assert!(!next.has_more);
    assert_eq!(service.get_vector_store(&store.id).await.unwrap().file_counts.total, 2);
    drop(service);
    let restarted = FileSearchService::new(pool, Arc::new(reqwest::Client::new()), file_config(&files)).unwrap();
    assert_eq!(
        restarted
            .search(std::slice::from_ref(&store.id), &query("coral"))
            .await
            .unwrap()
            .data[0]
            .file_id,
        file.id
    );
    restarted.delete_file(&file.id).await.unwrap();
    assert!(
        restarted
            .search(std::slice::from_ref(&store.id), &query("coral"))
            .await
            .unwrap()
            .data
            .is_empty()
    );
    assert_eq!(
        restarted.get_vector_store(&store.id).await.unwrap().file_counts.total,
        1
    );
    restarted.delete_vector_store(&store.id).await.unwrap();
    assert!(restarted.file_content(&other.id).await.is_ok());
    restarted.delete_file(&other.id).await.unwrap();
}

#[tokio::test]
#[cfg(not(feature = "file-search-pdf"))]
async fn pdf_disabled_build_preserves_upload_but_rejects_ingestion() {
    let service = service().await;
    let file = service
        .upload_file("document.pdf", "application/pdf", "assistants", b"%PDF-1.4\n".to_vec())
        .await
        .unwrap();
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let error = service
        .attach_file(
            &store.id,
            AttachFileRequest {
                file_id: file.id.clone(),
                ..AttachFileRequest::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.status_code(), 400);
    assert!(error.public_message().contains("file-search-pdf"));
    assert_eq!(service.file_content(&file.id).await.unwrap(), b"%PDF-1.4\n");
}

#[tokio::test]
async fn local_files_directory_is_lazy_and_configuration_failures_are_actionable() {
    let parent = tempfile::tempdir().unwrap();
    let directory = parent.path().join("created-on-upload");
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let config = FileSearchConfig {
        files_storage_dir: Some(directory.clone()),
        ..FileSearchConfig::default()
    };
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config).unwrap();
    assert!(!directory.exists());
    assert!(
        service
            .list_files(&ListParams::default())
            .await
            .unwrap()
            .data
            .is_empty()
    );
    assert!(!directory.exists());
    let file = service
        .upload_file("new.txt", "text/plain", "assistants", b"new".to_vec())
        .await
        .unwrap();
    assert!(directory.join(file.id).is_file());
    let invalid_config = FileSearchConfig {
        files_storage_dir: Some("relative".into()),
        ..FileSearchConfig::default()
    };
    assert!(
        FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), invalid_config)
            .unwrap_err()
            .to_string()
            .contains("absolute")
    );
    let blocker = parent.path().join("not-a-directory");
    tokio::fs::write(&blocker, b"file").await.unwrap();
    let config = FileSearchConfig {
        files_storage_dir: Some(blocker.join("files")),
        ..FileSearchConfig::default()
    };
    let service = FileSearchService::new(pool, Arc::new(reqwest::Client::new()), config).unwrap();
    let error = service
        .upload_file("blocked.txt", "text/plain", "assistants", b"blocked".to_vec())
        .await
        .unwrap_err();
    assert_eq!(error.status_code(), 503);
    assert!(error.public_message().contains("permissions"));
    assert!(std::error::Error::source(&error).is_some());
}

#[tokio::test]
#[cfg(feature = "file-search-pdf")]
async fn compressed_pdf_expansion_is_rejected_without_publishing_chunks() {
    let service = service().await;
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    for (count, size) in [(1, 17 * 1024 * 1024), (3, 12 * 1024 * 1024)] {
        let mut document = lopdf::Document::load_mem(&text_pdf("coral")).unwrap();
        let mut content = lopdf::Stream::new(lopdf::Dictionary::new(), vec![b' '; size]);
        content.compress().unwrap();
        assert!(content.content.len() < 100_000);
        document.objects.insert((4, 0), lopdf::Object::Stream(content.clone()));
        for _ in 1..count {
            document.add_object(content.clone());
        }
        let mut pdf = Vec::new();
        document.save_to(&mut pdf).unwrap();
        let file = service
            .upload_file("compressed.pdf", "application/pdf", "assistants", pdf)
            .await
            .unwrap();
        let error = service
            .attach_file(
                &store.id,
                AttachFileRequest {
                    file_id: file.id,
                    ..AttachFileRequest::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                FileSearchError::PdfParse(lopdf::Error::Decompress(
                    lopdf::DecompressError::MemoryLimitExceeded { .. }
                ))
            ),
            "{error:?}"
        );
        assert_eq!(service.get_vector_store(&store.id).await.unwrap().file_counts.total, 0);
    }
}

#[tokio::test]
async fn pgvector_configuration_rejects_sqlite() {
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let files = tempfile::tempdir().unwrap();
    let parsed = serde_json::from_value::<FileSearchConfig>(serde_json::json!({
        "files_storage_dir": files.path(),
        "backend": {"type":"pgvector", "dimensions":2, "index":{"type":"hnsw", "m":16, "ef_construction":64, "ef_search":40}, "candidate_limit":100},
        "embedding_base_url":"http://localhost:8000/v1", "embedding_model":"fixture"
    }));
    assert!(
        parsed.is_ok(),
        "pgvector must be a typed deployment configuration: {parsed:?}"
    );
    let error = FileSearchService::new(pool, Arc::new(reqwest::Client::new()), parsed.unwrap()).unwrap_err();
    assert!(error.to_string().contains("PostgreSQL"), "{error}");
}

#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL pointing to an isolated pgvector PostgreSQL database"]
#[allow(
    clippy::too_many_lines,
    reason = "isolated indexed retrieval lifecycle and filter compatibility cases"
)]
async fn postgres_pgvector_indexed_semantics_restart_filters_and_deletion() {
    let (_fixture, _, task, _, mut config) = embedding_service().await;
    let files = tempfile::tempdir().unwrap();
    config.files_storage_dir = Some(files.path().to_owned());
    let mut value = serde_json::to_value(&config).unwrap();
    value["backend"] = serde_json::json!({"type":"pgvector", "dimensions":2, "index":{"type":"hnsw", "m":16, "ef_construction":64, "ef_search":100}, "candidate_limit":100});
    let config: FileSearchConfig = serde_json::from_value(value).expect("typed pgvector configuration");
    let url = std::env::var("TEST_POSTGRES_URL").unwrap();
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config.clone()).unwrap();
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let coral = attach_text(
        &service,
        &store.id,
        "reef.txt",
        "coral reef",
        [
            ("region".into(), AttributeValue::String("sea".into())),
            ("rank".into(), AttributeValue::Number(42.0)),
            ("active".into(), AttributeValue::Boolean(true)),
        ]
        .into(),
    )
    .await;
    let other = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    attach_text(
        &service,
        &other.id,
        "hidden.txt",
        "coral hidden",
        FileAttributes::default(),
    )
    .await;
    let query = SearchRequest {
        search_mode: Some(SearchMode::Semantic),
        ..query("aquatic")
    };
    let found = service.search(std::slice::from_ref(&store.id), &query).await.unwrap();
    assert_eq!(found.data.len(), 1);
    assert_eq!(found.data[0].file_id, coral.id);
    let keyword = SearchRequest {
        search_mode: Some(SearchMode::Keyword),
        query: SearchQuery::Text("coral absent".into()),
        ..SearchRequest::default()
    };
    assert_eq!(
        service
            .search(std::slice::from_ref(&store.id), &keyword)
            .await
            .unwrap()
            .data[0]
            .file_id,
        coral.id,
        "multiword keyword retrieval must retain partial term matches"
    );

    let indexes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_indexes WHERE tablename = 'file_search_chunks' AND indexdef LIKE '%USING hnsw%'",
    )
    .fetch_one(pool.as_ref())
    .await
    .unwrap();
    assert!(indexes > 0);
    let mut filtered = query.clone();
    filtered.filters =
        Some(serde_json::from_value(serde_json::json!({"type":"eq", "key":"region", "value":"land"})).unwrap());
    assert!(
        service
            .search(std::slice::from_ref(&store.id), &filtered)
            .await
            .unwrap()
            .data
            .is_empty()
    );
    for (filter, expected) in [
        (serde_json::json!({"type":"eq", "key":"region", "value":"sea"}), 1),
        (serde_json::json!({"type":"ne", "key":"region", "value":"sea"}), 0),
        (serde_json::json!({"type":"ne", "key":"region", "value":5}), 0),
        (serde_json::json!({"type":"ne", "key":"missing", "value":"sea"}), 0),
        (
            serde_json::json!({"type":"in", "key":"region", "value":["land", "sea"]}),
            1,
        ),
        (serde_json::json!({"type":"nin", "key":"region", "value":["land"]}), 1),
        (serde_json::json!({"type":"gt", "key":"rank", "value":40}), 1),
        (serde_json::json!({"type":"lte", "key":"rank", "value":41}), 0),
        (serde_json::json!({"type":"eq", "key":"active", "value":true}), 1),
        (
            serde_json::json!({"type":"eq", "key":"region", "value":"sea' OR true --"}),
            0,
        ),
        (
            serde_json::json!({"type":"eq", "key":"region') OR true --", "value":"sea"}),
            0,
        ),
        (
            serde_json::json!({"type":"and", "filters":[{"type":"eq", "key":"region", "value":"sea"}, {"type":"gt", "key":"rank", "value":40}]}),
            1,
        ),
    ] {
        filtered.filters = Some(serde_json::from_value(filter.clone()).unwrap());
        assert_eq!(
            service
                .search(std::slice::from_ref(&store.id), &filtered)
                .await
                .unwrap()
                .data
                .len(),
            expected,
            "{filter}"
        );
    }
    drop(service);
    let restarted = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config).unwrap();
    assert_eq!(
        restarted
            .search(std::slice::from_ref(&store.id), &query)
            .await
            .unwrap()
            .data[0]
            .file_id,
        coral.id
    );
    restarted.detach_file(&store.id, &coral.id).await.unwrap();
    assert!(
        restarted
            .search(std::slice::from_ref(&store.id), &query)
            .await
            .unwrap()
            .data
            .is_empty()
    );
    restarted.delete_vector_store(&store.id).await.unwrap();
    restarted.delete_vector_store(&other.id).await.unwrap();
    task.abort();
}

#[test]
fn pgvector_index_parameters_are_validated() {
    for backend in [
        FileSearchBackend::Pgvector {
            dimensions: 0,
            index: PgvectorIndex::Hnsw {
                m: 16,
                ef_construction: 64,
                ef_search: 40,
            },
            candidate_limit: 100,
        },
        FileSearchBackend::Pgvector {
            dimensions: 2001,
            index: PgvectorIndex::Hnsw {
                m: 16,
                ef_construction: 64,
                ef_search: 40,
            },
            candidate_limit: 100,
        },
        FileSearchBackend::Pgvector {
            dimensions: 2,
            index: PgvectorIndex::Hnsw {
                m: 16,
                ef_construction: 16,
                ef_search: 40,
            },
            candidate_limit: 100,
        },
        FileSearchBackend::Pgvector {
            dimensions: 2,
            index: PgvectorIndex::Ivfflat { lists: 2, probes: 2 },
            candidate_limit: 100,
        },
        FileSearchBackend::Pgvector {
            dimensions: 2,
            index: PgvectorIndex::Ivfflat { lists: 2, probes: 1 },
            candidate_limit: 0,
        },
    ] {
        assert!(backend.validate().is_err(), "{backend:?}");
    }
}

#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL pointing to an isolated pgvector PostgreSQL database"]
#[allow(
    clippy::too_many_lines,
    reason = "one isolated database lifecycle verifies migration, reconfiguration, and rollback"
)]
async fn postgres_pgvector_migrates_exact_data_and_rolls_back_failed_publication() {
    let (_fixture, state, task, _, mut config) = embedding_service().await;
    let files = tempfile::tempdir().unwrap();
    config.files_storage_dir = Some(files.path().to_owned());
    let url = std::env::var("TEST_POSTGRES_URL").unwrap();
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    // This ignored test requires an isolated database and resets only file-search stores.
    sqlx::query("DELETE FROM file_search_stores")
        .execute(pool.as_ref())
        .await
        .unwrap();
    // Remove only the optional projection to exercise a legacy deployment upgrade.
    sqlx::query("ALTER TABLE file_search_chunks DROP COLUMN IF EXISTS embedding CASCADE")
        .execute(pool.as_ref())
        .await
        .unwrap();
    let exact = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config.clone()).unwrap();
    let store = exact
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let file = attach_text(
        &exact,
        &store.id,
        "legacy.txt",
        "coral legacy",
        FileAttributes::default(),
    )
    .await;
    config.backend = FileSearchBackend::Pgvector {
        dimensions: 2,
        index: PgvectorIndex::Ivfflat { lists: 2, probes: 1 },
        candidate_limit: 50,
    };
    let vector = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config.clone()).unwrap();
    let query = SearchRequest {
        search_mode: Some(SearchMode::Semantic),
        ..query("aquatic")
    };
    assert_eq!(
        vector
            .search(std::slice::from_ref(&store.id), &query)
            .await
            .unwrap()
            .data[0]
            .file_id,
        file.id
    );
    let early_indexes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_indexes WHERE tablename = 'file_search_chunks' AND indexdef LIKE '%USING ivfflat%'",
    )
    .fetch_one(pool.as_ref())
    .await
    .unwrap();
    assert_eq!(early_indexes, 0, "IVFFlat must wait for representative training rows");
    let training_store = exact
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let training = exact
        .upload_file(
            "training.txt",
            "text/plain",
            "assistants",
            "forest ".repeat(200_000).into_bytes(),
        )
        .await
        .unwrap();
    exact
        .attach_file(
            &training_store.id,
            AttachFileRequest {
                file_id: training.id,
                chunking_strategy: Some(ChunkingStrategy::Static {
                    config: StaticChunking {
                        max_chunk_size_tokens: 100,
                        chunk_overlap_tokens: 0,
                    },
                }),
                ..AttachFileRequest::default()
            },
        )
        .await
        .unwrap();
    vector.search(std::slice::from_ref(&store.id), &query).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await
        .unwrap();
    let plan: Vec<String> = sqlx::query_scalar("EXPLAIN (ANALYZE, COSTS OFF) SELECT data FROM file_search_chunks WHERE vector_dims(embedding) = 2 ORDER BY embedding::vector(2) <=> '[1,0]'::vector LIMIT 50").fetch_all(&mut *tx).await.unwrap();
    assert!(plan.join("\n").contains("Index Scan"), "{plan:?}");
    tx.rollback().await.unwrap();
    let mut bounded_config = config.clone();
    bounded_config.backend = FileSearchBackend::Pgvector {
        dimensions: 2,
        index: PgvectorIndex::Ivfflat { lists: 2, probes: 1 },
        candidate_limit: 1000,
    };
    let bounded = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), bounded_config).unwrap();
    let many_queries = SearchRequest {
        query: SearchQuery::Texts(vec!["forest".into(); 11]),
        search_mode: Some(SearchMode::Semantic),
        ..SearchRequest::default()
    };
    assert_eq!(
        bounded
            .search(std::slice::from_ref(&training_store.id), &many_queries)
            .await
            .unwrap_err()
            .status_code(),
        503,
        "candidate row limits apply across all queries before deduplication"
    );
    let generated: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE store_id = $1 AND embedding IS NOT NULL")
            .bind(&store.id)
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
    assert_eq!(generated, 1);
    config.backend = FileSearchBackend::Pgvector {
        dimensions: 2,
        index: PgvectorIndex::Hnsw {
            m: 16,
            ef_construction: 64,
            ef_search: 100,
        },
        candidate_limit: 50,
    };
    let changed = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config).unwrap();
    changed.search(std::slice::from_ref(&store.id), &query).await.unwrap();
    let indexes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_indexes WHERE tablename = 'file_search_chunks' AND indexdef LIKE '%vector_cosine_ops%'").fetch_one(pool.as_ref()).await.unwrap();
    assert_eq!(indexes, 1, "changing index configuration must replace the prior index");
    // Provider dimensional drift must not publish attachment metadata or vectors.
    *state.mode.lock().unwrap() = ProviderMode::WrongDimensions;
    let bad = vector
        .upload_file("wrong.txt", "text/plain", "assistants", b"wrong".to_vec())
        .await
        .unwrap();
    assert!(
        vector
            .attach_file(
                &store.id,
                AttachFileRequest {
                    file_id: bad.id.clone(),
                    ..AttachFileRequest::default()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        vector.get_vector_store(&store.id).await.unwrap().file_counts.completed,
        1
    );
    *state.mode.lock().unwrap() = ProviderMode::Good;
    sqlx::query(
        "ALTER TABLE file_search_chunks ADD CONSTRAINT task1_reject_second_chunk CHECK (chunk_index < 1) NOT VALID",
    )
    .execute(pool.as_ref())
    .await
    .unwrap();
    let bad = vector
        .upload_file(
            "rollback.txt",
            "text/plain",
            "assistants",
            "coral ".repeat(300).into_bytes(),
        )
        .await
        .unwrap();
    let result = vector
        .attach_file(
            &store.id,
            AttachFileRequest {
                file_id: bad.id.clone(),
                chunking_strategy: Some(ChunkingStrategy::Static {
                    config: StaticChunking {
                        max_chunk_size_tokens: 100,
                        chunk_overlap_tokens: 0,
                    },
                }),
                ..AttachFileRequest::default()
            },
        )
        .await;
    sqlx::query("ALTER TABLE file_search_chunks DROP CONSTRAINT task1_reject_second_chunk")
        .execute(pool.as_ref())
        .await
        .unwrap();
    assert!(result.is_err());
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE file_id = $1")
        .bind(&bad.id)
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(remaining, 0);
    assert!(vector.get_vector_store_file(&store.id, &bad.id).await.is_err());
    vector.delete_file(&file.id).await.unwrap();
    assert!(
        vector
            .search(std::slice::from_ref(&store.id), &query)
            .await
            .unwrap()
            .data
            .is_empty()
    );
    vector.delete_vector_store(&store.id).await.unwrap();
    vector.delete_vector_store(&training_store.id).await.unwrap();
    task.abort();
}

#[tokio::test]
async fn empty_semantic_search_does_not_require_the_embedding_provider() {
    let (service, state, task, _, _) = embedding_service().await;
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    *state.mode.lock().unwrap() = ProviderMode::Fail;
    let request = SearchRequest {
        search_mode: Some(SearchMode::Semantic),
        ..query("aquatic")
    };
    assert!(
        service
            .search(std::slice::from_ref(&store.id), &request)
            .await
            .unwrap()
            .data
            .is_empty()
    );
    *state.mode.lock().unwrap() = ProviderMode::Good;
    let file = attach_text(&service, &store.id, "transient.txt", "coral", FileAttributes::default()).await;
    service.detach_file(&store.id, &file.id).await.unwrap();
    *state.mode.lock().unwrap() = ProviderMode::Fail;
    assert!(service.search(&[store.id], &request).await.unwrap().data.is_empty());
    task.abort();
}
