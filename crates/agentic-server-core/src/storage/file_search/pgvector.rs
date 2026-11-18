//! PostgreSQL vector projection and bounded ANN candidates. Generated columns keep
//! legacy data and every metadata publication/deletion transaction consistent.

use futures::TryStreamExt;
use std::collections::HashSet;

use super::StoredChunk;
use crate::storage::{DbPool, schema::pgvector::PgvectorSchema};
mod filter;
use crate::types::file_search::{FileSearchError, PgvectorConfig, PgvectorIndex, SearchFilter, SearchMode, invalid};
use filter::{CandidateSql, push_filter};

#[derive(Clone)]
pub(crate) struct PgvectorStorage {
    schema: PgvectorSchema,
    limit: u16,
}

impl PgvectorStorage {
    pub(crate) async fn open(pool: &DbPool, config: &PgvectorConfig) -> Result<Self, FileSearchError> {
        config.validate()?;
        if !matches!(pool.connect_options().database_url.scheme(), "postgres" | "postgresql") {
            return invalid("pgvector requires PostgreSQL");
        }
        let index = Self {
            schema: PgvectorSchema::new(config),
            limit: config.candidate_limit,
        };
        index.schema.ensure_ready(pool).await?;
        Ok(index)
    }

    pub(crate) fn dimensions(&self) -> usize {
        usize::from(self.schema.dimensions())
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
        if mode != SearchMode::Keyword && matches!(self.schema.index(), PgvectorIndex::Ivfflat { .. }) {
            self.schema.maintain_index(pool).await?;
        }
        let mut tx = pool.begin().await?;
        let settings = match self.schema.index() {
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
                    let dims = self.schema.dimensions();
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
