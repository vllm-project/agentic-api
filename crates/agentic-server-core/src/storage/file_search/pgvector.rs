//! PostgreSQL vector projection and bounded ANN candidates. Generated columns keep
//! legacy data and every metadata publication/deletion transaction consistent.

use futures::TryStreamExt;
use std::collections::HashSet;

use super::StoredChunk;
use crate::storage::{DbPool, DbTransaction};
mod filter;
use crate::types::file_search::{FileSearchError, PgvectorConfig, PgvectorIndex, SearchFilter, SearchMode, invalid};
use filter::{CandidateSql, push_filter};

#[derive(Clone)]
pub(crate) struct PgvectorStorage {
    dimensions: u16,
    index: PgvectorIndex,
    limit: u16,
}

impl PgvectorStorage {
    pub(crate) async fn open(pool: &DbPool, config: &PgvectorConfig) -> Result<Self, FileSearchError> {
        config.validate()?;
        if !matches!(pool.connect_options().database_url.scheme(), "postgres" | "postgresql") {
            return invalid("pgvector requires PostgreSQL");
        }
        let index = Self {
            dimensions: config.dimensions,
            index: config.index,
            limit: config.candidate_limit,
        };
        index.initialize_schema(pool).await?;
        Ok(index)
    }

    pub(crate) fn dimensions(&self) -> usize {
        usize::from(self.dimensions)
    }

    async fn initialize_schema(&self, pool: &DbPool) -> Result<(), FileSearchError> {
        let mut tx = pool.begin().await?;
        // Serialize optional schema setup across independent service instances.
        sqlx::query("SELECT pg_advisory_xact_lock(752041291)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(&mut *tx)
            .await?;
        let version: String = sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
            .fetch_one(&mut *tx)
            .await?;
        let parts = version.split('.').map(str::parse::<u32>).collect::<Result<Vec<_>, _>>();
        if !parts.is_ok_and(|parts| parts.len() == 3 && parts.as_slice() >= [0, 8, 0].as_slice()) {
            return invalid("pgvector 0.8.0 or later is required for filtered iterative index scans");
        }
        sqlx::query("ALTER TABLE file_search_chunks ADD COLUMN IF NOT EXISTS embedding vector GENERATED ALWAYS AS (l2_normalize((data::jsonb ->> 'embedding')::vector)) STORED")
            .execute(&mut *tx).await?;
        self.configure_index(&mut tx).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS file_search_chunks_text ON file_search_chunks USING gin (to_tsvector('simple', data::jsonb ->> 'text'))")
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    fn index_configuration(&self) -> (&'static str, String) {
        match self.index {
            PgvectorIndex::Hnsw { m, ef_construction, .. } => {
                ("hnsw", format!("m = {m}, ef_construction = {ef_construction}"))
            }
            PgvectorIndex::Ivfflat { lists, .. } => ("ivfflat", format!("lists = {lists}")),
        }
    }

    async fn index_is_current(&self, tx: &mut DbTransaction<'_>) -> Result<bool, FileSearchError> {
        let (method, options) = self.index_configuration();
        let existing: Option<String> =
            sqlx::query_scalar("SELECT obj_description(oid, 'pg_class') FROM pg_class WHERE oid = to_regclass($1)")
                .bind(format!("file_search_vector_{}", self.dimensions))
                .fetch_optional(&mut **tx)
                .await?
                .flatten();
        Ok(existing.as_deref() == Some(format!("{method} {options}").as_str()))
    }

    async fn maintain_index(&self, pool: &DbPool) -> Result<(), FileSearchError> {
        // Established indexes need only an unlocked catalog read. Missing or
        // changed indexes are rechecked under the maintenance lock. Commit
        // before retrieval so the lock never covers streamed candidate queries.
        let mut tx = pool.begin().await?;
        if !self.index_is_current(&mut tx).await? {
            self.configure_index(&mut tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn configure_index(&self, tx: &mut DbTransaction<'_>) -> Result<(), FileSearchError> {
        sqlx::query("SELECT pg_advisory_xact_lock(752041291)")
            .execute(&mut **tx)
            .await?;
        let dims = self.dimensions;
        let (method, options) = self.index_configuration();
        let name = format!("file_search_vector_{dims}");
        let signature = format!("{method} {options}");
        if !self.index_is_current(tx).await? {
            sqlx::query(&format!("DROP INDEX IF EXISTS {name}"))
                .execute(&mut **tx)
                .await?;
            if let PgvectorIndex::Ivfflat { lists, .. } = self.index {
                let rows: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE vector_dims(embedding) = $1")
                        .bind(i32::from(dims))
                        .fetch_one(&mut **tx)
                        .await?;
                // Match OGX's conservative training floor. Until then, bounded
                // SQL distance retrieval remains available without an ANN index.
                if rows < i64::from(lists) * 1000 {
                    return Ok(());
                }
            }
            // One selected ANN index per dimension; reconfiguration rebuilds it
            // atomically so PostgreSQL cannot silently pick an obsolete index.
            // Identifiers and DDL values are exclusively validated integer configuration.
            let sql = format!(
                "CREATE INDEX {name} ON file_search_chunks USING {method} ((embedding::vector({dims})) vector_cosine_ops) WITH ({options}) WHERE vector_dims(embedding) = {dims}"
            );
            sqlx::query(&sql).execute(&mut **tx).await?;
            sqlx::query(&format!("COMMENT ON INDEX {name} IS '{signature}'"))
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn candidates(
        &self,
        pool: &DbPool,
        stores: &[String],
        queries: &[String],
        vectors: &[Vec<f64>],
        mode: SearchMode,
        filter: Option<&SearchFilter>,
    ) -> Result<Vec<StoredChunk>, FileSearchError> {
        if mode != SearchMode::Keyword && matches!(self.index, PgvectorIndex::Ivfflat { .. }) {
            self.maintain_index(pool).await?;
        }
        let mut tx = pool.begin().await?;
        let settings = match self.index {
            PgvectorIndex::Hnsw { ef_search, .. } => [
                ("hnsw.ef_search", ef_search.to_string()),
                ("hnsw.iterative_scan", "strict_order".into()),
            ],
            PgvectorIndex::Ivfflat { probes, .. } => [
                ("ivfflat.probes", probes.to_string()),
                ("ivfflat.iterative_scan", "relaxed_order".into()),
            ],
        };
        for (key, value) in settings {
            sqlx::query("SELECT set_config($1, $2, true)")
                .bind(key)
                .bind(value)
                .execute(&mut *tx)
                .await?;
        }
        let mut seen = HashSet::new();
        let mut chunks = Vec::new();
        let mut candidate_bytes = 0usize;
        let mut candidate_count = 0usize;
        for (i, query) in queries.iter().enumerate() {
            for semantic in [true, false] {
                if (semantic && mode == SearchMode::Keyword) || (!semantic && mode == SearchMode::Semantic) {
                    continue;
                }
                let mut sql = CandidateSql::new("SELECT store_id, data FROM file_search_chunks WHERE store_id IN (");
                for (i, store) in stores.iter().enumerate() {
                    if i > 0 {
                        sql.push(", ");
                    }
                    sql.push_bind(store);
                }
                sql.push(")");
                if let Some(filter) = filter {
                    sql.push(" AND ");
                    push_filter(&mut sql, filter)?;
                }
                if semantic {
                    let vector = vectors.get(i).ok_or(FileSearchError::ProviderProtocol)?;
                    validate_vector(vector, self.dimensions())?;
                    let dims = self.dimensions;
                    sql.push(format!(
                        " AND vector_dims(embedding) = {dims} ORDER BY embedding::vector({dims}) <=> "
                    ));
                    sql.push("l2_normalize(")
                        .push_bind(serde_json::to_string(vector)?)
                        .push("::vector)");
                } else {
                    let terms = query
                        .split(|character: char| !character.is_alphanumeric())
                        .filter(|term| !term.is_empty())
                        .collect::<Vec<_>>()
                        .join(" | ");
                    if terms.is_empty() {
                        continue;
                    }
                    sql.push(" AND to_tsvector('simple', data::jsonb ->> 'text') @@ to_tsquery('simple', ")
                        .push_bind(terms.clone())
                        .push(") ORDER BY ts_rank(to_tsvector('simple', data::jsonb ->> 'text'), to_tsquery('simple', ")
                        .push_bind(terms)
                        .push(")) DESC");
                }
                sql.push(format!(" LIMIT {}", self.limit));
                let mut query = sqlx::query_as::<_, (String, String)>(&sql.text);
                for value in &sql.parameters {
                    query = query.bind(value);
                }
                let mut rows = query.fetch(&mut *tx);
                while let Some((store_id, data)) = rows.try_next().await? {
                    candidate_bytes = candidate_bytes.saturating_add(data.len());
                    candidate_count += 1;
                    if candidate_bytes > 64 * 1024 * 1024 || candidate_count > 10_000 {
                        return Err(FileSearchError::Unavailable("Indexed candidate union exceeds 64 MiB or 10000 rows; reduce candidate_limit or the number of queries".into()));
                    }
                    let chunk: StoredChunk = serde_json::from_str(&data)?;
                    if seen.insert((store_id, chunk.file_id.clone(), chunk.chunk_index)) {
                        chunks.push(chunk);
                    }
                }
            }
        }
        tx.commit().await?;
        Ok(chunks)
    }
}

pub(crate) fn validate_vector(vector: &[f64], dimensions: usize) -> Result<(), FileSearchError> {
    if vector.len() != dimensions
        || vector
            .iter()
            .any(|value| !value.is_finite() || value.abs() > f64::from(f32::MAX))
        || !vector.iter().any(|value| value.abs() >= f64::from(f32::MIN_POSITIVE))
    {
        return invalid(
            "pgvector embeddings must match configured dimensions and contain finite, nonzero float32 values",
        );
    }
    Ok(())
}
