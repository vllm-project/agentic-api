use agentic_core::storage::create_pool_with_schema;
use agentic_core::tool::file_search::FileSearchService;
use agentic_core::types::file_search::*;
#[cfg(feature = "file-search-pdf")]
use std::fmt::Write as _;
use std::sync::Arc;

async fn service() -> FileSearchService {
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    FileSearchService::new(pool, Arc::new(reqwest::Client::new()), FileSearchConfig::default()).unwrap()
}

fn query(text: &str) -> SearchRequest {
    SearchRequest {
        query: SearchQuery::Text(text.into()),
        ..SearchRequest::default()
    }
}

#[tokio::test]
async fn uploads_keep_capacity_reserved_while_waiting_for_database() {
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let service = FileSearchService::new(
        pool.clone(),
        Arc::new(reqwest::Client::new()),
        FileSearchConfig::default(),
    )
    .unwrap();
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
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let client = Arc::new(reqwest::Client::new());
    let service = FileSearchService::new(pool.clone(), client.clone(), FileSearchConfig::default()).unwrap();
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
    let recreated = FileSearchService::new(pool, client, FileSearchConfig::default()).unwrap();
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
    FileSearchService,
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
    let config = FileSearchConfig {
        embedding_base_url: Some(format!("http://{addr}/v1")),
        embedding_model: Some("fixture".into()),
        embedding_api_key: Some("never-log-this-key".into()),
    };
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config.clone()).unwrap();
    (service, state, task, pool, config)
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
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let service = FileSearchService::new(
        pool.clone(),
        Arc::new(reqwest::Client::new()),
        FileSearchConfig::default(),
    )
    .unwrap();
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
        embedding_base_url: Some("https://private.example/v1".into()),
        embedding_model: Some("model".into()),
        embedding_api_key: Some("secret-credential".into()),
    };
    assert!(!format!("{config:?}").contains("secret-credential"));
}

#[tokio::test]
async fn uploaded_bytes_and_search_survive_closing_and_reopening_database_pool() {
    let path = std::env::temp_dir().join(format!("file-search-restart-{}.db", uuid::Uuid::now_v7()));
    let url = format!("sqlite://{}", path.display());
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    let service = FileSearchService::new(
        pool.clone(),
        Arc::new(reqwest::Client::new()),
        FileSearchConfig::default(),
    )
    .unwrap();
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
    let service = FileSearchService::new(
        reopened.clone(),
        Arc::new(reqwest::Client::new()),
        FileSearchConfig::default(),
    )
    .unwrap();
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
    let url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    let service = FileSearchService::new(
        pool.clone(),
        Arc::new(reqwest::Client::new()),
        FileSearchConfig::default(),
    )
    .unwrap();
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
    let restarted =
        FileSearchService::new(pool, Arc::new(reqwest::Client::new()), FileSearchConfig::default()).unwrap();
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
async fn pdf_disabled_build_rejects_upload_with_actionable_error() {
    let service = service().await;
    let error = service
        .upload_file("document.pdf", "application/pdf", "assistants", b"%PDF-1.4\n".to_vec())
        .await
        .unwrap_err();
    assert_eq!(error.status_code(), 400);
    assert!(error.public_message().contains("file-search-pdf"));
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
