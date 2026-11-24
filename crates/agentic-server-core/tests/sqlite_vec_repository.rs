use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use agentic_core::{
    storage::create_pool_with_schema,
    tool::file_search::{Collection, FileSearchRepository, PreparedAttachment, SqliteVecRepository, StoredChunk},
    types::file_search::{
        AttributeValue, ChunkingStrategy, ComparisonFilter, ComparisonOperator, FileCounts, FileObject,
        FileSearchError, FilterValue, SearchFilter, SearchMode, SearchQuery, SearchRequest, VectorStoreFileObject,
        VectorStoreObject,
    },
};
use tokio_util::sync::CancellationToken;

fn test_file() -> FileObject {
    FileObject {
        id: format!("file_{}", uuid::Uuid::now_v7()),
        object: "file".into(),
        bytes: 10,
        created_at: 1,
        filename: "search.txt".into(),
        purpose: "assistants".into(),
        status: "processed".into(),
    }
}

fn test_store() -> VectorStoreObject {
    VectorStoreObject {
        id: format!("vs_{}", uuid::Uuid::now_v7()),
        object: "vector_store".into(),
        created_at: 1,
        name: "search".into(),
        usage_bytes: 0,
        file_counts: FileCounts::default(),
        status: "completed".into(),
        metadata: BTreeMap::default(),
    }
}

fn test_attachment(file: &FileObject, store: &VectorStoreObject, chunks: Vec<StoredChunk>) -> PreparedAttachment {
    PreparedAttachment {
        object: VectorStoreFileObject {
            id: file.id.clone(),
            object: "vector_store.file".into(),
            created_at: 1,
            vector_store_id: store.id.clone(),
            status: "completed".into(),
            usage_bytes: file.bytes,
            attributes: BTreeMap::default(),
            chunking_strategy: ChunkingStrategy::Auto,
            last_error: None,
        },
        chunks,
        dimensions: 2,
    }
}

async fn assert_semantic_searches(
    repository: &SqliteVecRepository,
    store: &VectorStoreObject,
    request: &SearchRequest,
) {
    let results = repository
        .search(
            std::slice::from_ref(&store.id),
            &["reef".into()],
            &[vec![1e300, 0.0]],
            Some(2),
            SearchMode::Semantic,
            request,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content[0].text, "coral reef");
    assert!(results[0].score > 0.99);

    let top_one = SearchRequest {
        max_num_results: Some(1),
        ..request.clone()
    };
    let results = repository
        .search(
            std::slice::from_ref(&store.id),
            &["trail".into()],
            &[vec![0.1, 1.0]],
            Some(2),
            SearchMode::Semantic,
            &top_one,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content[0].text, "mountain trail");

    let top_two = SearchRequest {
        max_num_results: Some(2),
        ..request.clone()
    };
    let results = repository
        .search(
            std::slice::from_ref(&store.id),
            &["reef".into()],
            &[vec![1.0, 0.1]],
            Some(2),
            SearchMode::Semantic,
            &top_two,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].content[0].text, "coral reef");
    assert_eq!(results[1].content[0].text, "mountain trail");

    let filtered = SearchRequest {
        max_num_results: Some(1),
        filters: Some(SearchFilter::Comparison(ComparisonFilter {
            operator: ComparisonOperator::Eq,
            key: "section".into(),
            value: FilterValue::Scalar(AttributeValue::String("trail".into())),
        })),
        ..request.clone()
    };
    let results = repository
        .search(
            std::slice::from_ref(&store.id),
            &["reef".into()],
            &[vec![1.0, 0.1]],
            Some(2),
            SearchMode::Semantic,
            &filtered,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content[0].text, "mountain trail");
}

async fn assert_recovery(
    pool: Arc<agentic_core::storage::DbPool>,
    path: &Path,
    repository: SqliteVecRepository,
    store: &VectorStoreObject,
    attachment: &mut PreparedAttachment,
    identity: &str,
    request: &SearchRequest,
) {
    // Simulate an interrupted publication by removing the published metadata,
    // then retry after reopening the repository.
    sqlx::query("DELETE FROM file_search_attachments WHERE store_id = $1 AND file_id = $2")
        .bind(&store.id)
        .bind(&attachment.object.id)
        .execute(pool.as_ref())
        .await
        .unwrap();
    drop(repository);
    let reopened = SqliteVecRepository::open(pool, path).await.unwrap();
    attachment.chunks[0].embedding = Some(vec![-1e-300, 0.0]);
    reopened.attach(&store.id, identity, attachment).await.unwrap();
    let results = reopened
        .search(
            std::slice::from_ref(&store.id),
            &["reef".into()],
            &[vec![-1.0, 0.0]],
            Some(2),
            SearchMode::Semantic,
            request,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content[0].text, "coral reef");
    assert!(results[0].score > 0.99);
    assert!(
        reopened
            .search(&[], &["reef".into()], &[], Some(2), SearchMode::Semantic, request)
            .await
            .unwrap()
            .is_empty()
    );
    let mismatch = reopened
        .search(
            std::slice::from_ref(&store.id),
            &["reef".into(), "trail".into()],
            &[vec![-1.0, 0.0]],
            Some(2),
            SearchMode::Semantic,
            request,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(mismatch, FileSearchError::InvalidRequest(message) if message.contains("one embedding per query"))
    );
    reopened
        .delete(Collection::Attachments, &attachment.object.id, Some(&store.id))
        .await
        .unwrap();
    let results = reopened
        .search(
            std::slice::from_ref(&store.id),
            &["reef".into()],
            &[vec![-1.0, 0.0]],
            Some(2),
            SearchMode::Semantic,
            request,
        )
        .await
        .unwrap();
    assert!(results.is_empty());
    assert!(matches!(
        reopened.attachment(&store.id, &attachment.object.id).await,
        Ok(None)
    ));
}

async fn assert_orphan_recovery(
    pool: Arc<agentic_core::storage::DbPool>,
    path: &Path,
    index: &sqlx::SqlitePool,
    orphan_id: i64,
    store: &VectorStoreObject,
    file: &FileObject,
    request: &SearchRequest,
) {
    let reopened = SqliteVecRepository::open(Arc::clone(&pool), path).await.unwrap();
    let orphan: Option<i64> = sqlx::query_scalar("SELECT id FROM file_search_vec_map WHERE id = $1")
        .bind(orphan_id)
        .fetch_optional(index)
        .await
        .unwrap();
    assert!(orphan.is_none(), "recovery must reclaim an orphan immediately");
    let results = reopened
        .search(
            std::slice::from_ref(&store.id),
            &["result".into()],
            &[vec![1.0, 0.0]],
            Some(2),
            SearchMode::Semantic,
            request,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].file_id, file.id);
    // A crash after metadata deletion can leave the per-store virtual table.
    sqlx::query("DELETE FROM file_search_stores WHERE id = $1")
        .bind(&store.id)
        .execute(pool.as_ref())
        .await
        .unwrap();
    drop(reopened);
    SqliteVecRepository::open(Arc::clone(&pool), path).await.unwrap();
    let table = format!(
        "file_search_vec_{}",
        uuid::Uuid::parse_str(store.id.strip_prefix("vs_").unwrap())
            .unwrap()
            .simple()
    );
    let remaining: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = $1")
            .bind(table)
            .fetch_optional(pool.as_ref())
            .await
            .unwrap();
    assert!(remaining.is_none(), "recovery must drop orphaned vec0 tables");
}

#[tokio::test]
#[ignore = "requires SQLITE_VEC_EXTENSION_PATH pointing to a trusted sqlite-vec loadable extension"]
async fn sqlite_vec_indexes_published_chunks_and_excludes_deleted_rows() {
    let path = PathBuf::from(std::env::var_os("SQLITE_VEC_EXTENSION_PATH").expect("set SQLITE_VEC_EXTENSION_PATH"));
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", directory.path().join("files.db").display());
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    let repository = SqliteVecRepository::open(Arc::clone(&pool), &path).await.unwrap();
    assert_eq!(repository.index_identity(), Some("sqlite-vec:cosine-f32:v1"));

    let file = FileObject {
        id: format!("file_{}", uuid::Uuid::now_v7()),
        object: "file".into(),
        bytes: 12,
        created_at: 1,
        filename: "notes.txt".into(),
        purpose: "assistants".into(),
        status: "processed".into(),
    };
    repository
        .stage_blob_cleanup(&file.id, "test-upload", i64::MAX)
        .await
        .unwrap();
    repository
        .upload_file_metadata(&file, "text/plain", "test-upload", &CancellationToken::new())
        .await
        .unwrap();
    let store = VectorStoreObject {
        id: format!("vs_{}", uuid::Uuid::now_v7()),
        object: "vector_store".into(),
        created_at: 1,
        name: "notes".into(),
        usage_bytes: 0,
        file_counts: FileCounts::default(),
        status: "completed".into(),
        metadata: BTreeMap::default(),
    };
    let attachment = PreparedAttachment {
        object: VectorStoreFileObject {
            id: file.id.clone(),
            object: "vector_store.file".into(),
            created_at: 1,
            vector_store_id: store.id.clone(),
            status: "completed".into(),
            usage_bytes: 12,
            attributes: BTreeMap::default(),
            chunking_strategy: ChunkingStrategy::Auto,
            last_error: None,
        },
        chunks: vec![
            StoredChunk {
                file_id: file.id.clone(),
                filename: file.filename.clone(),
                chunk_index: 0,
                text: "coral reef".into(),
                embedding: Some(vec![1e-300, 0.0]),
                attributes: BTreeMap::from([("section".into(), AttributeValue::String("reef".into()))]),
            },
            StoredChunk {
                file_id: file.id.clone(),
                filename: file.filename.clone(),
                chunk_index: 1,
                text: "mountain trail".into(),
                embedding: Some(vec![0.0, 1e300]),
                attributes: BTreeMap::from([("section".into(), AttributeValue::String("trail".into()))]),
            },
            StoredChunk {
                file_id: file.id.clone(),
                filename: file.filename.clone(),
                chunk_index: 2,
                text: "coral reef".into(),
                embedding: Some(vec![1e-300, 0.0]),
                attributes: BTreeMap::from([("section".into(), AttributeValue::String("reef".into()))]),
            },
        ],
        dimensions: 2,
    };
    let identity = "sqlite-vec-test-model";
    let mut attachments = [attachment];
    repository.create_store(&store, identity, &attachments).await.unwrap();

    let request = SearchRequest {
        query: SearchQuery::Text("reef".into()),
        search_mode: Some(SearchMode::Semantic),
        ..SearchRequest::default()
    };
    assert_semantic_searches(&repository, &store, &request).await;

    assert_recovery(pool, &path, repository, &store, &mut attachments[0], identity, &request).await;
}

#[tokio::test]
#[ignore = "requires SQLITE_VEC_EXTENSION_PATH pointing to a trusted sqlite-vec loadable extension"]
async fn sqlite_vec_fences_concurrent_publication_and_skips_unpublished_rows() {
    let path = PathBuf::from(std::env::var_os("SQLITE_VEC_EXTENSION_PATH").expect("set SQLITE_VEC_EXTENSION_PATH"));
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", directory.path().join("concurrent.db").display());
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    let first = SqliteVecRepository::open(Arc::clone(&pool), &path).await.unwrap();
    let second = SqliteVecRepository::open(Arc::clone(&pool), &path).await.unwrap();
    let file = test_file();
    first
        .stage_blob_cleanup(&file.id, "test-upload", i64::MAX)
        .await
        .unwrap();
    first
        .upload_file_metadata(&file, "text/plain", "test-upload", &CancellationToken::new())
        .await
        .unwrap();
    let store = test_store();
    let empty = test_store();
    first.create_store(&store, "model", &[]).await.unwrap();
    first.create_store(&empty, "model", &[]).await.unwrap();
    let attachment = test_attachment(
        &file,
        &store,
        vec![StoredChunk {
            file_id: file.id.clone(),
            filename: file.filename.clone(),
            chunk_index: 0,
            text: "published result".into(),
            embedding: Some(vec![1.0, 0.0]),
            attributes: BTreeMap::default(),
        }],
    );
    let (left, right) = tokio::join!(
        first.attach(&store.id, "model", &attachment),
        second.attach(&store.id, "model", &attachment)
    );
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    assert!(matches!(
        left.as_ref().err().or(right.as_ref().err()),
        Some(FileSearchError::Conflict(_))
    ));

    let table = format!(
        "file_search_vec_{}",
        uuid::Uuid::parse_str(store.id.strip_prefix("vs_").unwrap())
            .unwrap()
            .simple()
    );
    let orphan_id: i64 = sqlx::query_scalar(
        "INSERT INTO file_search_vec_map (store_id, file_id, chunk_index, created_at) \
         VALUES ($1, 'file_unpublished', 0, $2) RETURNING id",
    )
    .bind(&store.id)
    .bind(chrono::Utc::now().timestamp())
    .fetch_one(pool.as_ref())
    .await
    .unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
        .unwrap()
        .extension(path.to_string_lossy().into_owned());
    let index = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {table} (rowid, embedding) VALUES ($1, '[1.0,0.0]')"
    ))
    .bind(orphan_id)
    .execute(&index)
    .await
    .unwrap();

    let request = SearchRequest {
        query: SearchQuery::Text("result".into()),
        search_mode: Some(SearchMode::Semantic),
        ..SearchRequest::default()
    };
    let results = first
        .search(
            &[empty.id.clone(), store.id.clone()],
            &["result".into()],
            &[vec![1.0, 0.0]],
            Some(2),
            SearchMode::Semantic,
            &request,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].file_id, file.id);

    assert_orphan_recovery(pool, &path, &index, orphan_id, &store, &file, &request).await;
}

#[tokio::test]
#[ignore = "requires SQLITE_VEC_EXTENSION_PATH pointing to a trusted sqlite-vec loadable extension"]
async fn sqlite_vec_uses_exact_fallback_above_candidate_limit() {
    let path = PathBuf::from(std::env::var_os("SQLITE_VEC_EXTENSION_PATH").expect("set SQLITE_VEC_EXTENSION_PATH"));
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", directory.path().join("large.db").display());
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    let repository = SqliteVecRepository::open(pool, &path).await.unwrap();
    let file = test_file();
    repository
        .stage_blob_cleanup(&file.id, "test-upload", i64::MAX)
        .await
        .unwrap();
    repository
        .upload_file_metadata(&file, "text/plain", "test-upload", &CancellationToken::new())
        .await
        .unwrap();
    let store = test_store();
    let chunks = (0..4097)
        .map(|chunk_index| StoredChunk {
            file_id: file.id.clone(),
            filename: file.filename.clone(),
            chunk_index,
            text: format!("chunk {chunk_index}"),
            embedding: Some(vec![1.0, 0.0]),
            attributes: BTreeMap::from([("selected".into(), AttributeValue::Boolean(chunk_index == 4096))]),
        })
        .collect();
    let attachment = test_attachment(&file, &store, chunks);
    repository.create_store(&store, "model", &[attachment]).await.unwrap();
    let request = SearchRequest {
        query: SearchQuery::Text("chunk".into()),
        search_mode: Some(SearchMode::Semantic),
        max_num_results: Some(4096),
        ..SearchRequest::default()
    };
    let results = repository
        .search(
            std::slice::from_ref(&store.id),
            &["chunk".into()],
            &[vec![1.0, 0.0]],
            Some(2),
            SearchMode::Semantic,
            &request,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 4096);
    let filtered = SearchRequest {
        max_num_results: Some(1),
        filters: Some(SearchFilter::Comparison(ComparisonFilter {
            operator: ComparisonOperator::Eq,
            key: "selected".into(),
            value: FilterValue::Scalar(AttributeValue::Boolean(true)),
        })),
        ..request
    };
    let results = repository
        .search(
            std::slice::from_ref(&store.id),
            &["chunk".into()],
            &[vec![1.0, 0.0]],
            Some(2),
            SearchMode::Semantic,
            &filtered,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content[0].text, "chunk 4096");
}

#[tokio::test]
#[ignore = "requires SQLITE_VEC_EXTENSION_PATH pointing to a trusted sqlite-vec loadable extension"]
async fn sqlite_vec_exact_searches_use_the_indexed_provider_capacity() {
    let path = PathBuf::from(std::env::var_os("SQLITE_VEC_EXTENSION_PATH").expect("set SQLITE_VEC_EXTENSION_PATH"));
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", directory.path().join("capacity.db").display());
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    let repository = SqliteVecRepository::open(pool, &path).await.unwrap();
    let file = test_file();
    repository
        .stage_blob_cleanup(&file.id, "test-upload", i64::MAX)
        .await
        .unwrap();
    repository
        .upload_file_metadata(&file, "text/plain", "test-upload", &CancellationToken::new())
        .await
        .unwrap();

    let store = test_store();
    // The large attribute keeps ranking cheap while exercising a serialized
    // corpus above the default provider's 64 MiB bound.
    let attributes = BTreeMap::from([
        ("selected".into(), AttributeValue::Boolean(true)),
        ("padding".into(), AttributeValue::String("x".repeat(64 * 1024 * 1024))),
    ]);
    let attachment = test_attachment(
        &file,
        &store,
        vec![StoredChunk {
            file_id: file.id.clone(),
            filename: file.filename.clone(),
            chunk_index: 0,
            text: "coral reef".into(),
            embedding: Some(vec![1.0, 0.0]),
            attributes,
        }],
    );
    repository.create_store(&store, "model", &[attachment]).await.unwrap();

    for mode in [SearchMode::Keyword, SearchMode::Hybrid] {
        let request = SearchRequest {
            query: SearchQuery::Text("reef".into()),
            search_mode: Some(mode),
            ..SearchRequest::default()
        };
        let results = repository
            .search(
                std::slice::from_ref(&store.id),
                &["reef".into()],
                &[vec![1.0, 0.0]],
                Some(2),
                mode,
                &request,
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_id, file.id);
    }

    let filtered = SearchRequest {
        query: SearchQuery::Text("reef".into()),
        search_mode: Some(SearchMode::Semantic),
        filters: Some(SearchFilter::Comparison(ComparisonFilter {
            operator: ComparisonOperator::Eq,
            key: "selected".into(),
            value: FilterValue::Scalar(AttributeValue::Boolean(true)),
        })),
        ..SearchRequest::default()
    };
    let results = repository
        .search(
            std::slice::from_ref(&store.id),
            &["reef".into()],
            &[vec![1.0, 0.0]],
            Some(2),
            SearchMode::Semantic,
            &filtered,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].file_id, file.id);
}
