use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

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

#[tokio::test]
#[ignore = "requires SQLITE_VEC_EXTENSION_PATH pointing to a trusted sqlite-vec loadable extension"]
#[expect(
    clippy::too_many_lines,
    reason = "one integration scenario covers publication, restart, search, and deletion"
)]
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
    let results = repository
        .search(
            std::slice::from_ref(&store.id),
            &["reef".into()],
            &[vec![1e300, 0.0]],
            Some(2),
            SearchMode::Semantic,
            &request,
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

    // Simulate a crash after index staging: leave recent vector rows without
    // published chunks, then retry after reopening the repository.
    sqlx::query("DELETE FROM file_search_attachments WHERE store_id = $1 AND file_id = $2")
        .bind(&store.id)
        .bind(&file.id)
        .execute(pool.as_ref())
        .await
        .unwrap();
    drop(repository);
    let reopened = SqliteVecRepository::open(pool, &path).await.unwrap();
    attachments[0].chunks[0].embedding = Some(vec![-1e-300, 0.0]);
    reopened.attach(&store.id, identity, &attachments[0]).await.unwrap();
    let results = reopened
        .search(
            std::slice::from_ref(&store.id),
            &["reef".into()],
            &[vec![-1.0, 0.0]],
            Some(2),
            SearchMode::Semantic,
            &request,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content[0].text, "coral reef");
    assert!(results[0].score > 0.99);
    assert!(
        reopened
            .search(&[], &["reef".into()], &[], Some(2), SearchMode::Semantic, &request)
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
            &request,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(mismatch, FileSearchError::InvalidRequest(message) if message.contains("one embedding per query"))
    );
    reopened
        .delete(Collection::Attachments, &file.id, Some(&store.id))
        .await
        .unwrap();
    let results = reopened
        .search(
            std::slice::from_ref(&store.id),
            &["reef".into()],
            &[vec![-1.0, 0.0]],
            Some(2),
            SearchMode::Semantic,
            &request,
        )
        .await
        .unwrap();
    assert!(results.is_empty());
    assert!(matches!(reopened.attachment(&store.id, &file.id).await, Ok(None)));
}
