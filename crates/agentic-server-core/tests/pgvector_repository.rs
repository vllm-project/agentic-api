//! Run ignored tests serially against an isolated PostgreSQL database with pgvector >= 0.8.0.
use std::{collections::BTreeMap, sync::Arc, time::Duration};

use agentic_core::{
    storage::{DbPool, create_pool_with_schema},
    tool::file_search::{
        Collection, FileSearchRepository, FileSearchService, PgvectorRepository, PreparedAttachment,
        SqlFileSearchRepository, StoredChunk,
    },
    types::file_search::{
        AttributeValue, ChunkingStrategy, ComparisonFilter, ComparisonOperator, CompoundFilter, CompoundOperator,
        FileCounts, FileObject, FileSearchConfig, FilterValue, PgvectorConfig, PgvectorIndex, RankingOptions,
        SearchFilter, SearchMode, SearchQuery, SearchRequest, SearchResult, VectorStoreFileObject, VectorStoreObject,
    },
};
use tokio_util::sync::CancellationToken;

fn config(index: PgvectorIndex) -> PgvectorConfig {
    PgvectorConfig {
        dimensions: 2,
        candidate_limit: 100,
        index,
    }
}

fn hnsw() -> PgvectorConfig {
    config(PgvectorIndex::Hnsw {
        m: 16,
        ef_construction: 64,
        ef_search: 100,
    })
}

async fn pool() -> Arc<DbPool> {
    let url = std::env::var("TEST_POSTGRES_URL").expect("isolated PostgreSQL TEST_POSTGRES_URL required");
    create_pool_with_schema(Some(&url)).await.unwrap()
}

fn store() -> VectorStoreObject {
    VectorStoreObject {
        id: format!("vs_{}", uuid::Uuid::now_v7()),
        object: "vector_store".into(),
        created_at: 1,
        name: "pgvector".into(),
        usage_bytes: 0,
        file_counts: FileCounts::default(),
        status: "completed".into(),
        metadata: BTreeMap::new(),
    }
}

async fn file(repository: &dyn FileSearchRepository) -> FileObject {
    let file = FileObject {
        id: format!("file-{}", uuid::Uuid::now_v7()),
        object: "file".into(),
        bytes: 10,
        created_at: 1,
        filename: "search.txt".into(),
        purpose: "assistants".into(),
        status: "processed".into(),
    };
    repository.stage_blob_cleanup(&file.id, "test", 0).await.unwrap();
    repository
        .upload_file_metadata(&file, "text/plain", "test", &CancellationToken::new())
        .await
        .unwrap();
    file
}

fn attachment(file: &FileObject, store: &VectorStoreObject) -> PreparedAttachment {
    let chunks = [
        ("coral reef", vec![1.0, 0.1], "Z", 1.0),
        ("mountain trail", vec![0.1, 1.0], "a", 2.0),
    ]
    .into_iter()
    .enumerate()
    .map(|(chunk_index, (text, embedding, name, number))| StoredChunk {
        file_id: file.id.clone(),
        filename: file.filename.clone(),
        chunk_index,
        text: text.into(),
        embedding: Some(embedding),
        attributes: BTreeMap::from([
            ("name".into(), AttributeValue::String(name.into())),
            ("number".into(), AttributeValue::Number(number)),
            ("enabled".into(), AttributeValue::Boolean(chunk_index == 0)),
        ]),
    })
    .collect();
    PreparedAttachment {
        object: VectorStoreFileObject {
            id: file.id.clone(),
            object: "vector_store.file".into(),
            created_at: 1,
            vector_store_id: store.id.clone(),
            status: "completed".into(),
            usage_bytes: 10,
            attributes: BTreeMap::new(),
            chunking_strategy: ChunkingStrategy::Auto,
            last_error: None,
        },
        chunks,
        dimensions: 2,
    }
}

fn request(mode: SearchMode) -> SearchRequest {
    SearchRequest {
        query: SearchQuery::Text("reef".into()),
        search_mode: Some(mode),
        ..Default::default()
    }
}

async fn search(
    repository: &dyn FileSearchRepository,
    stores: &[String],
    request: &SearchRequest,
) -> Vec<SearchResult> {
    repository
        .search(
            stores,
            &["reef".into()],
            &[vec![1.0, 0.1]],
            Some(2),
            request.search_mode.unwrap_or(SearchMode::Semantic),
            request,
        )
        .await
        .unwrap()
}

async fn fixture(pool: Arc<DbPool>, options: &PgvectorConfig) -> (PgvectorRepository, VectorStoreObject, FileObject) {
    let repository = PgvectorRepository::open(pool, options).await.unwrap();
    let file = file(&repository).await;
    let store = store();
    repository
        .create_store(&store, "fixture", &[attachment(&file, &store)])
        .await
        .unwrap();
    (repository, store, file)
}

async fn assert_index_plan(pool: &DbPool, method: &str, store: &VectorStoreObject) {
    sqlx::query("ANALYZE file_search_chunks").execute(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await
        .unwrap();
    let plan: Vec<String> = sqlx::query_scalar("EXPLAIN SELECT store_id, data FROM file_search_chunks WHERE store_id = $1 AND vector_dims(embedding) = 2 ORDER BY embedding::vector(2) <=> '[1,0.1]'::vector LIMIT 100")
        .bind(&store.id).fetch_all(&mut *tx).await.unwrap();
    assert!(plan.join("\n").contains("file_search_vector_2"), "{plan:?}");
    let actual: String = sqlx::query_scalar("SELECT am.amname::text FROM pg_class c JOIN pg_am am ON am.oid = c.relam WHERE c.oid = 'file_search_vector_2'::regclass")
        .fetch_one(&mut *tx).await.unwrap();
    assert_eq!(actual, method);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn pgvector_rejects_sqlite_and_conflicting_providers() {
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    assert!(PgvectorRepository::open(pool.clone(), &hnsw()).await.is_err());
    let files = tempfile::tempdir().unwrap();
    let config = FileSearchConfig {
        pgvector: Some(hnsw()),
        sqlite_vec_extension_path: Some("/must/not/be/loaded".into()),
        files_storage_dir: Some(files.path().into()),
        ..Default::default()
    };
    let result = FileSearchService::configured(pool, Arc::new(reqwest::Client::new()), config).await;
    assert!(result.err().unwrap().to_string().contains("only one"));
}

#[test]
fn pgvector_validates_all_index_boundaries_and_typed_configuration() {
    for (dimensions, candidate_limit) in [(0, 100), (2001, 100), (2, 49), (2, 1001)] {
        assert!(
            PgvectorConfig {
                dimensions,
                candidate_limit,
                ..hnsw()
            }
            .validate()
            .is_err()
        );
    }
    for index in [
        PgvectorIndex::Hnsw {
            m: 1,
            ef_construction: 64,
            ef_search: 40,
        },
        PgvectorIndex::Hnsw {
            m: 16,
            ef_construction: 31,
            ef_search: 40,
        },
        PgvectorIndex::Hnsw {
            m: 16,
            ef_construction: 64,
            ef_search: 0,
        },
        PgvectorIndex::Ivfflat { lists: 1, probes: 1 },
        PgvectorIndex::Ivfflat { lists: 2, probes: 2 },
        PgvectorIndex::Ivfflat { lists: 2, probes: 0 },
    ] {
        assert!(config(index).validate().is_err());
    }
    for index in [
        PgvectorIndex::Hnsw {
            m: 2,
            ef_construction: 4,
            ef_search: 1,
        },
        PgvectorIndex::Hnsw {
            m: 100,
            ef_construction: 1000,
            ef_search: 1000,
        },
        PgvectorIndex::Ivfflat {
            lists: 32768,
            probes: 32767,
        },
    ] {
        config(index).validate().unwrap();
    }
    assert!(serde_json::from_str::<PgvectorConfig>(r#"{"dimensions":2,"candidate_limit":100,"index":{"type":"hnsw","m":16,"ef_construction":64,"ef_search":40,"unknown":true}}"#).is_err());
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL with pgvector >= 0.8.0; run serially"]
async fn postgres_pgvector_backfill_restart_native_index_and_deletion() {
    let pool = pool().await;
    // Seed through the portable repository before optional projection setup.
    sqlx::query("ALTER TABLE file_search_chunks DROP COLUMN IF EXISTS embedding CASCADE")
        .execute(pool.as_ref())
        .await
        .unwrap();
    let exact = SqlFileSearchRepository::new(pool.clone());
    let file = file(&exact).await;
    let store = store();
    exact
        .create_store(&store, "fixture", &[attachment(&file, &store)])
        .await
        .unwrap();
    let repository = PgvectorRepository::open(pool.clone(), &hnsw()).await.unwrap();
    for mode in [SearchMode::Semantic, SearchMode::Keyword, SearchMode::Hybrid] {
        let options = request(mode);
        let actual = search(&repository, std::slice::from_ref(&store.id), &options).await;
        let expected = search(&exact, std::slice::from_ref(&store.id), &options).await;
        assert_eq!(actual[0].content[0].text, expected[0].content[0].text);
        if mode == SearchMode::Semantic {
            assert!((actual[0].score - expected[0].score).abs() < 1e-12);
        }
    }
    assert_index_plan(&pool, "hnsw", &store).await;
    drop(repository);
    pool.close().await;
    let reopened = self::pool().await;
    let repository = PgvectorRepository::open(reopened.clone(), &hnsw()).await.unwrap();
    let scoped = [store.id.clone()];
    assert_eq!(
        search(&repository, &scoped, &request(SearchMode::Semantic)).await.len(),
        2
    );
    // Another store must never leak into selected-store retrieval.
    let (_, other, other_file) = fixture(reopened.clone(), &hnsw()).await;
    let actual = search(&repository, &scoped, &request(SearchMode::Semantic)).await;
    assert!(actual.iter().all(|result| result.file_id == file.id));
    repository
        .delete(Collection::Attachments, &file.id, Some(&store.id))
        .await
        .unwrap();
    assert!(
        search(&repository, &scoped, &request(SearchMode::Semantic))
            .await
            .is_empty()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE file_id = $1")
        .bind(&file.id)
        .fetch_one(reopened.as_ref())
        .await
        .unwrap();
    assert_eq!(count, 0);
    repository.delete(Collection::Stores, &store.id, None).await.unwrap();
    repository.delete(Collection::Stores, &other.id, None).await.unwrap();
    repository.delete(Collection::Files, &file.id, None).await.unwrap();
    repository
        .delete(Collection::Files, &other_file.id, None)
        .await
        .unwrap();
    assert!(repository.pending_blob_cleanup(&file.id).await.unwrap().is_some());
}

fn comparison(key: &str, operator: ComparisonOperator, value: FilterValue) -> SearchFilter {
    SearchFilter::Comparison(ComparisonFilter {
        key: key.into(),
        operator,
        value,
    })
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL with pgvector >= 0.8.0; run serially"]
async fn postgres_pgvector_typed_filters_match_exact_ranking() {
    let pool = pool().await;
    let (repository, store, file) = fixture(pool.clone(), &hnsw()).await;
    let exact = SqlFileSearchRepository::new(pool.clone());
    let scalar = |value| FilterValue::Scalar(value);
    let mut filters = Vec::new();
    for operator in [
        ComparisonOperator::Eq,
        ComparisonOperator::Ne,
        ComparisonOperator::Gt,
        ComparisonOperator::Gte,
        ComparisonOperator::Lt,
        ComparisonOperator::Lte,
    ] {
        filters.push(comparison("name", operator, scalar(AttributeValue::String("Z".into()))));
        filters.push(comparison("number", operator, scalar(AttributeValue::Number(2.0))));
    }
    for operator in [ComparisonOperator::In, ComparisonOperator::Nin] {
        filters.push(comparison(
            "name",
            operator,
            FilterValue::List(vec![AttributeValue::String("Z".into()), AttributeValue::Boolean(false)]),
        ));
        filters.push(comparison(
            "missing",
            operator,
            FilterValue::List(vec![AttributeValue::Number(1.0)]),
        ));
    }
    filters.push(comparison(
        "number",
        ComparisonOperator::Ne,
        scalar(AttributeValue::String("1".into())),
    ));
    filters.push(comparison(
        "enabled",
        ComparisonOperator::Eq,
        scalar(AttributeValue::Boolean(false)),
    ));
    filters.push(comparison(
        "x') OR TRUE --",
        ComparisonOperator::Eq,
        scalar(AttributeValue::String("Z".into())),
    ));
    filters.push(SearchFilter::Compound(CompoundFilter {
        operator: CompoundOperator::Or,
        filters: vec![
            comparison(
                "name",
                ComparisonOperator::Eq,
                scalar(AttributeValue::String("Z".into())),
            ),
            SearchFilter::Compound(CompoundFilter {
                operator: CompoundOperator::And,
                filters: vec![
                    comparison("number", ComparisonOperator::Gte, scalar(AttributeValue::Number(2.0))),
                    comparison("enabled", ComparisonOperator::Ne, scalar(AttributeValue::Boolean(true))),
                ],
            }),
        ],
    }));
    for filter in filters {
        let options = SearchRequest {
            filters: Some(filter),
            ..request(SearchMode::Semantic)
        };
        let actual = search(&repository, std::slice::from_ref(&store.id), &options).await;
        let expected = search(&exact, std::slice::from_ref(&store.id), &options).await;
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap(),
            "{options:?}"
        );
    }
    repository.delete(Collection::Stores, &store.id, None).await.unwrap();
    repository.delete(Collection::Files, &file.id, None).await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL with pgvector >= 0.8.0; run serially"]
async fn postgres_pgvector_rejects_invalid_vectors_and_rolls_back_publication() {
    let pool = pool().await;
    let (repository, store, file) = fixture(pool.clone(), &hnsw()).await;
    let mut invalid = attachment(&file, &store);
    for vector in [
        vec![0.0, 0.0],
        vec![f64::NAN, 1.0],
        vec![f64::INFINITY, 1.0],
        vec![1e300, 1.0],
        vec![1e-300, 0.0],
        vec![1.0],
    ] {
        invalid.chunks[0].embedding = Some(vector);
        assert!(repository.attach(&store.id, "fixture", &invalid).await.is_err());
    }
    let new_store = self::store();
    let mut duplicate = attachment(&file, &new_store);
    duplicate.chunks[1].chunk_index = 0;
    assert!(
        repository
            .create_store(&new_store, "fixture", &[duplicate])
            .await
            .is_err()
    );
    assert!(repository.store(&new_store.id).await.is_err());
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE store_id = $1")
        .bind(&new_store.id)
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(rows, 0);
    let new_file = self::file(&repository).await;
    let mut duplicate = attachment(&new_file, &store);
    duplicate.chunks[1].chunk_index = 0;
    assert!(repository.attach(&store.id, "fixture", &duplicate).await.is_err());
    assert!(repository.attachment(&store.id, &new_file.id).await.unwrap().is_none());
    assert_eq!(
        search(
            &repository,
            std::slice::from_ref(&store.id),
            &request(SearchMode::Semantic)
        )
        .await
        .len(),
        2
    );
    repository.delete(Collection::Stores, &store.id, None).await.unwrap();
    for file in [file, new_file] {
        repository.delete(Collection::Files, &file.id, None).await.unwrap();
    }
}

async fn index_exists(pool: &DbPool) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pg_class WHERE oid = to_regclass('file_search_vector_2')")
        .fetch_one(pool)
        .await
        .unwrap()
        == 1
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL with pgvector >= 0.8.0; run serially"]
async fn postgres_pgvector_ivfflat_training_reconfiguration_and_unlocked_reads() {
    let pool = pool().await;
    let options = config(PgvectorIndex::Ivfflat { lists: 2, probes: 1 });
    let (repository, store, file) = fixture(pool.clone(), &options).await;
    assert!(!index_exists(&pool).await, "untrained IVFFlat must not be installed");
    assert_eq!(
        search(
            &repository,
            std::slice::from_ref(&store.id),
            &request(SearchMode::Semantic)
        )
        .await
        .len(),
        2
    );
    // Enough real rows to train the configured index. Publication still uses the portable transaction.
    let large_file = self::file(&repository).await;
    let mut large = attachment(&large_file, &store);
    let template = large.chunks[0].clone();
    large.chunks = (0..2000)
        .map(|index| StoredChunk {
            chunk_index: index,
            text: format!("reef training {index}"),
            embedding: Some(vec![1.0, f64::from(u32::try_from(index).unwrap()) / 2000.0]),
            ..template.clone()
        })
        .collect();
    repository.attach(&store.id, "fixture", &large).await.unwrap();
    assert!(
        !search(
            &repository,
            std::slice::from_ref(&store.id),
            &request(SearchMode::Semantic)
        )
        .await
        .is_empty()
    );
    assert_index_plan(&pool, "ivfflat", &store).await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(752041291)")
        .execute(&mut *lock)
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        search(
            &repository,
            std::slice::from_ref(&store.id),
            &request(SearchMode::Semantic),
        ),
    )
    .await
    .expect("established retrieval must not acquire initialization lock");
    lock.rollback().await.unwrap();
    let rebuilt = PgvectorRepository::open(pool.clone(), &hnsw()).await.unwrap();
    assert_index_plan(&pool, "hnsw", &store).await;
    let threshold = SearchRequest {
        ranking_options: Some(RankingOptions {
            score_threshold: Some(1.0),
            ..Default::default()
        }),
        ..request(SearchMode::Semantic)
    };
    assert!(
        search(&rebuilt, std::slice::from_ref(&store.id), &threshold)
            .await
            .iter()
            .all(|result| (result.score - 1.0).abs() < f64::EPSILON)
    );
    rebuilt.delete(Collection::Stores, &store.id, None).await.unwrap();
    for file in [file, large_file] {
        rebuilt.delete(Collection::Files, &file.id, None).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL with pgvector >= 0.8.0; run serially"]
async fn postgres_pgvector_filters_before_candidate_limit_and_bounds_query_union() {
    let pool = pool().await;
    let options = PgvectorConfig {
        candidate_limit: 1000,
        ..hnsw()
    };
    let (repository, store, file) = fixture(pool.clone(), &options).await;
    let large_file = self::file(&repository).await;
    let mut large = attachment(&large_file, &store);
    let template = large.chunks[0].clone();
    large.chunks = (0..1500)
        .map(|index| StoredChunk {
            chunk_index: index,
            text: format!("reef distractor {index}"),
            attributes: BTreeMap::from([("enabled".into(), AttributeValue::Boolean(false))]),
            ..template.clone()
        })
        .collect();
    repository.attach(&store.id, "fixture", &large).await.unwrap();
    let filtered = SearchRequest {
        max_num_results: Some(1),
        filters: Some(comparison(
            "enabled",
            ComparisonOperator::Eq,
            FilterValue::Scalar(AttributeValue::Boolean(true)),
        )),
        ..request(SearchMode::Semantic)
    };
    let results = search(&repository, std::slice::from_ref(&store.id), &filtered).await;
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].file_id, file.id,
        "SQL filters must precede candidate truncation"
    );
    let queries = vec!["reef".to_owned(); 16];
    let vectors = vec![vec![1.0, 0.1]; 16];
    let result = repository
        .search(
            std::slice::from_ref(&store.id),
            &queries,
            &vectors,
            Some(2),
            SearchMode::Semantic,
            &request(SearchMode::Semantic),
        )
        .await;
    assert!(result.err().unwrap().to_string().contains("10000 rows"));
    // A bounded multi-query request still uses the global result limit.
    let results = repository
        .search(
            std::slice::from_ref(&store.id),
            &queries[..2],
            &vectors[..2],
            Some(2),
            SearchMode::Semantic,
            &filtered,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    repository.delete(Collection::Stores, &store.id, None).await.unwrap();
    for file in [file, large_file] {
        repository.delete(Collection::Files, &file.id, None).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL with pgvector >= 0.8.0; run serially"]
async fn postgres_pgvector_normalizes_extreme_vectors_and_serializes_replica_setup() {
    let pool = pool().await;
    let options = hnsw();
    let (first, second) = tokio::try_join!(
        PgvectorRepository::open(pool.clone(), &options),
        PgvectorRepository::open(pool.clone(), &options)
    )
    .unwrap();
    assert_eq!(first.index_identity(), second.index_identity());
    let file = self::file(&first).await;
    let store = store();
    let mut attachment = attachment(&file, &store);
    attachment.chunks[0].embedding = Some(vec![1e30, 0.0]);
    attachment.chunks[1].embedding = Some(vec![0.0, 1e-30]);
    first.create_store(&store, "fixture", &[attachment]).await.unwrap();
    for vector in [vec![1e30, 0.0], vec![0.0, 1e-30]] {
        let result = second
            .search(
                std::slice::from_ref(&store.id),
                &["reef".into()],
                &[vector],
                Some(2),
                SearchMode::Semantic,
                &request(SearchMode::Semantic),
            )
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!((result[0].score - 1.0).abs() < f64::EPSILON);
    }
    let norms: Vec<f64> =
        sqlx::query_scalar("SELECT vector_norm(embedding) FROM file_search_chunks WHERE store_id = $1")
            .bind(&store.id)
            .fetch_all(pool.as_ref())
            .await
            .unwrap();
    assert!(norms.iter().all(|norm| (norm - 1.0).abs() < f64::EPSILON));
    second.delete(Collection::Stores, &store.id, None).await.unwrap();
    second.delete(Collection::Files, &file.id, None).await.unwrap();
}
