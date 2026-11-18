//! Real PostgreSQL checks for optional DDL and supervisor ownership.

use super::*;
use crate::storage::create_pool;
use sqlx::any::AnyPoolOptions;

async fn fixture(index: PgvectorIndex) -> (DbPool, String, PgvectorSchema) {
    let url = std::env::var("TEST_POSTGRES_URL").expect("isolated TEST_POSTGRES_URL required");
    let admin = create_pool(Some(&url)).await.unwrap();
    sqlx::query("CREATE EXTENSION IF NOT EXISTS vector WITH SCHEMA public")
        .execute(admin.as_ref())
        .await
        .unwrap();
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .connect_with(admin.connect_options().as_ref().clone())
        .await
        .unwrap();
    let name = format!("pgvector_schema_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(&format!("CREATE SCHEMA {name}"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("SET search_path = {name}, public"))
        .execute(&pool)
        .await
        .unwrap();
    // Only the portable chunk columns are prerequisites for this backend step.
    sqlx::query("CREATE TABLE file_search_chunks (data TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let schema = PgvectorSchema::new(&PgvectorConfig {
        dimensions: 2,
        index,
        candidate_limit: 100,
    });
    (pool, name, schema)
}

async fn read_only(pool: &DbPool, enabled: bool) {
    sqlx::query(if enabled {
        "SET default_transaction_read_only = on"
    } else {
        "SET default_transaction_read_only = off"
    })
    .execute(pool)
    .await
    .unwrap();
}

async fn cleanup(pool: &DbPool, name: &str) {
    read_only(pool, false).await;
    sqlx::query(&format!("DROP SCHEMA {name} CASCADE"))
        .execute(pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL with pgvector >= 0.8.0; run serially"]
async fn postgres_pgvector_supervisor_verifies_without_ddl() {
    let (pool, name, schema) = fixture(PgvectorIndex::Hnsw {
        m: 16,
        ef_construction: 64,
        ef_search: 100,
    })
    .await;
    read_only(&pool, true).await;
    assert!(
        schema.ensure_ready_with_marker(&pool, true).await.is_err(),
        "a supervisor marker cannot conceal missing schema"
    );
    read_only(&pool, false).await;
    schema.ensure_ready_with_marker(&pool, false).await.unwrap();
    read_only(&pool, true).await;
    schema.ensure_ready_with_marker(&pool, true).await.unwrap();
    // The normal initialization would issue DDL even on an initialized schema.
    assert!(schema.ensure_ready_with_marker(&pool, false).await.is_err());
    let changed = PgvectorSchema {
        dimensions: 2,
        index: PgvectorIndex::Hnsw {
            m: 8,
            ef_construction: 64,
            ef_search: 100,
        },
    };
    assert!(changed.ensure_ready_with_marker(&pool, true).await.is_err());
    schema.ensure_ready_with_marker(&pool, true).await.unwrap();
    for ddl in [
        "DROP INDEX file_search_chunks_text",
        "DROP INDEX file_search_vector_2",
        "ALTER TABLE file_search_chunks DROP COLUMN embedding CASCADE",
    ] {
        read_only(&pool, false).await;
        sqlx::query(ddl).execute(&pool).await.unwrap();
        read_only(&pool, true).await;
        assert!(
            schema.ensure_ready_with_marker(&pool, true).await.is_err(),
            "missing object after {ddl}"
        );
        read_only(&pool, false).await;
        schema.ensure_ready_with_marker(&pool, false).await.unwrap();
        read_only(&pool, true).await;
        schema.ensure_ready_with_marker(&pool, true).await.unwrap();
    }
    cleanup(&pool, &name).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL with pgvector >= 0.8.0; run serially"]
async fn postgres_pgvector_supervisor_leaves_ivfflat_training_to_operator() {
    let (pool, name, schema) = fixture(PgvectorIndex::Ivfflat { lists: 1, probes: 1 }).await;
    schema.ensure_ready_with_marker(&pool, false).await.unwrap();
    read_only(&pool, true).await;
    schema.ensure_ready_with_marker(&pool, true).await.unwrap();
    schema.maintain_index_with_marker(&pool, true).await.unwrap();
    read_only(&pool, false).await;
    sqlx::query(r#"INSERT INTO file_search_chunks(data) SELECT '{"embedding":[1.0,0.1],"text":"training"}' FROM generate_series(1,1000)"#).execute(&pool).await.unwrap();
    read_only(&pool, true).await;
    assert!(schema.ensure_ready_with_marker(&pool, true).await.is_err());
    assert!(schema.maintain_index_with_marker(&pool, true).await.is_err());
    let missing: bool =
        sqlx::query_scalar("SELECT to_regclass(format('%I.file_search_vector_2', current_schema())) IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        missing,
        "supervisor mode must not create an index when training becomes possible"
    );
    read_only(&pool, false).await;
    schema.maintain_index_with_marker(&pool, false).await.unwrap();
    read_only(&pool, true).await;
    schema.ensure_ready_with_marker(&pool, true).await.unwrap();
    schema.maintain_index_with_marker(&pool, true).await.unwrap();
    cleanup(&pool, &name).await;
}
