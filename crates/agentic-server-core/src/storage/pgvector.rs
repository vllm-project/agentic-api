//! PostgreSQL vector projection and bounded ANN candidates. Generated columns keep
//! legacy data and every metadata publication/deletion transaction consistent.

use futures::TryStreamExt;
use std::{collections::HashSet, sync::Arc};
use tokio::sync::OnceCell;

use super::{
    DbPool,
    file_search::{ChunkRow, RetrievedChunk},
};
use crate::types::file_search::{
    AttributeValue, ComparisonOperator, CompoundOperator, FileSearchBackend, FileSearchError, FilterValue,
    PgvectorIndex, SearchFilter, SearchMode, invalid,
};

#[derive(Clone)]
pub(crate) struct PgvectorStorage {
    dimensions: u16,
    index: PgvectorIndex,
    limit: u16,
    initialized: Arc<OnceCell<()>>,
}

impl PgvectorStorage {
    pub(crate) fn from_config(pool: &DbPool, backend: &FileSearchBackend) -> Result<Option<Self>, FileSearchError> {
        backend.validate()?;
        let FileSearchBackend::Pgvector {
            dimensions,
            index,
            candidate_limit,
        } = backend
        else {
            return Ok(None);
        };
        if !matches!(pool.connect_options().database_url.scheme(), "postgres" | "postgresql") {
            return invalid("pgvector requires PostgreSQL; select the exact backend for SQLite");
        }
        Ok(Some(Self {
            dimensions: *dimensions,
            index: index.clone(),
            limit: *candidate_limit,
            initialized: Arc::new(OnceCell::new()),
        }))
    }

    pub(crate) fn dimensions(&self) -> usize {
        usize::from(self.dimensions)
    }

    pub(crate) async fn initialize(&self, pool: &DbPool) -> Result<(), FileSearchError> {
        self.initialized
            .get_or_try_init(|| self.initialize_schema(pool))
            .await?;
        Ok(())
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
        let parts: Vec<u32> = version.split('.').filter_map(|part| part.parse().ok()).collect();
        if parts.as_slice() < [0, 8, 0].as_slice() {
            return invalid("pgvector 0.8.0 or later is required for filtered iterative index scans");
        }
        sqlx::query("ALTER TABLE file_search_chunks ADD COLUMN IF NOT EXISTS embedding vector GENERATED ALWAYS AS ((data::jsonb ->> 'embedding')::vector) STORED")
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

    async fn index_is_current(&self, tx: &mut super::DbTransaction<'_>) -> Result<bool, FileSearchError> {
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

    async fn configure_index(&self, tx: &mut super::DbTransaction<'_>) -> Result<(), FileSearchError> {
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
    ) -> Result<Vec<RetrievedChunk>, FileSearchError> {
        self.initialize(pool).await?;
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
                let mut sql = CandidateSql::new(
                    "SELECT store_id, file_id, chunk_index, (SELECT generation FROM file_search_attachments a WHERE a.store_id=file_search_chunks.store_id AND a.file_id=file_search_chunks.file_id) AS generation, data FROM file_search_chunks WHERE (store_id, file_id) IN (SELECT store_id, file_id FROM file_search_attachments WHERE status = 'completed') AND store_id IN (",
                );
                for (i, store) in stores.iter().enumerate() {
                    if i > 0 {
                        sql.push(", ");
                    }
                    sql.push_bind(store);
                }
                sql.push(") AND store_id IN (SELECT id FROM file_search_stores WHERE lifecycle_status != 'expired' AND (expires_at IS NULL OR expires_at > EXTRACT(EPOCH FROM clock_timestamp()))) AND file_id IN (SELECT id FROM file_search_files WHERE expires_at IS NULL OR expires_at > EXTRACT(EPOCH FROM clock_timestamp()))");
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
                    sql.push_bind(serde_json::to_string(vector)?).push("::vector");
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
                let mut query = sqlx::query_as::<_, ChunkRow>(&sql.text);
                for value in &sql.parameters {
                    query = query.bind(value);
                }
                let mut rows = query.fetch(&mut *tx);
                while let Some(row) = rows.try_next().await? {
                    candidate_bytes = candidate_bytes.saturating_add(row.data.len());
                    candidate_count += 1;
                    if candidate_bytes > 64 * 1024 * 1024 || candidate_count > 10_000 {
                        return Err(FileSearchError::Unavailable("Indexed candidate union exceeds 64 MiB or 10000 rows; reduce candidate_limit or the number of queries".into()));
                    }
                    let chunk = row.decode()?;
                    if seen.insert(chunk.origin.clone()) {
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

fn push_filter(sql: &mut CandidateSql, filter: &SearchFilter) -> Result<(), FileSearchError> {
    sql.push("(");
    match filter {
        SearchFilter::Compound(compound) => {
            for (index, child) in compound.filters.iter().enumerate() {
                if index > 0 {
                    sql.push(match compound.operator {
                        CompoundOperator::And => " AND ",
                        CompoundOperator::Or => " OR ",
                    });
                }
                push_filter(sql, child)?;
            }
        }
        SearchFilter::Comparison(comparison) => {
            let attr = |sql: &mut CandidateSql| {
                sql.push("(data::jsonb -> 'attributes' -> ")
                    .push_bind(comparison.key.clone())
                    .push(")");
            };
            attr(sql);
            sql.push(" IS NOT NULL AND ");
            match &comparison.value {
                FilterValue::List(values) => {
                    attr(sql);
                    sql.push(if comparison.operator == ComparisonOperator::Nin {
                        " NOT IN ("
                    } else {
                        " IN ("
                    });
                    for (i, value) in values.iter().enumerate() {
                        if i > 0 {
                            sql.push(", ");
                        }
                        sql.push_bind(serde_json::to_string(value)?).push("::jsonb");
                    }
                    sql.push(")");
                }
                FilterValue::Scalar(value) => {
                    sql.push("jsonb_typeof(");
                    attr(sql);
                    sql.push(") = ").push_bind(match value {
                        AttributeValue::String(_) => "string",
                        AttributeValue::Number(_) => "number",
                        AttributeValue::Boolean(_) => "boolean",
                    });
                    sql.push(" AND ");
                    if matches!(value, AttributeValue::String(_)) {
                        sql.push("(data::jsonb -> 'attributes' ->> ")
                            .push_bind(comparison.key.clone())
                            .push(") COLLATE \"C\"");
                    } else {
                        attr(sql);
                    }
                    sql.push(match comparison.operator {
                        ComparisonOperator::Eq => " = ",
                        ComparisonOperator::Ne => " <> ",
                        ComparisonOperator::Gt => " > ",
                        ComparisonOperator::Gte => " >= ",
                        ComparisonOperator::Lt => " < ",
                        ComparisonOperator::Lte => " <= ",
                        ComparisonOperator::In | ComparisonOperator::Nin => return invalid("invalid scalar filter"),
                    });
                    if let AttributeValue::String(value) = value {
                        sql.push_bind(value.clone());
                    } else {
                        sql.push_bind(serde_json::to_string(value)?).push("::jsonb");
                    }
                }
            }
        }
    }
    sql.push(")");
    Ok(())
}

// sqlx Any uses question-mark QueryBuilder placeholders, whereas PostgreSQL
// requires numbered parameters. Only typed values enter the bound parameter list.
struct CandidateSql {
    text: String,
    parameters: Vec<String>,
}
impl CandidateSql {
    fn new(text: &str) -> Self {
        Self {
            text: text.into(),
            parameters: Vec::new(),
        }
    }
    fn push(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.text.push_str(text.as_ref());
        self
    }
    fn push_bind(&mut self, value: impl Into<String>) -> &mut Self {
        self.parameters.push(value.into());
        self.text.push('$');
        self.text.push_str(&self.parameters.len().to_string());
        self
    }
}
