//! Optional PostgreSQL file-search schema, index maintenance, and supervisor verification.
//! The generated projection preserves atomic publication and cascading deletion
//! without requiring the extension on unconfigured PostgreSQL or SQLite databases.

use super::is_marked_ready;
use crate::{
    storage::{DbPool, DbTransaction},
    types::file_search::{FileSearchError, PgvectorConfig, PgvectorIndex, invalid},
};

#[derive(Clone)]
pub(crate) struct PgvectorSchema {
    dimensions: u16,
    index: PgvectorIndex,
}

impl PgvectorSchema {
    pub(crate) fn new(config: &PgvectorConfig) -> Self {
        Self {
            dimensions: config.dimensions,
            index: config.index,
        }
    }

    pub(crate) fn dimensions(&self) -> u16 {
        self.dimensions
    }

    pub(crate) fn index(&self) -> PgvectorIndex {
        self.index
    }

    pub(crate) async fn ensure_ready(&self, pool: &DbPool) -> Result<(), FileSearchError> {
        self.ensure_ready_with_marker(pool, is_marked_ready()).await
    }

    async fn ensure_ready_with_marker(&self, pool: &DbPool, supervisor_managed: bool) -> Result<(), FileSearchError> {
        let mut tx = pool.begin().await?;
        if supervisor_managed {
            self.verify_schema(&mut tx).await?;
            tx.commit().await?;
            return Ok(());
        }
        self.initialize_schema(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn initialize_schema(&self, tx: &mut DbTransaction<'_>) -> Result<(), FileSearchError> {
        // Serialize optional schema setup across independent service instances.
        sqlx::query("SELECT pg_advisory_xact_lock(752041291)")
            .execute(&mut **tx)
            .await?;
        sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(&mut **tx)
            .await?;
        Self::verify_extension(tx).await?;
        sqlx::query("ALTER TABLE file_search_chunks ADD COLUMN IF NOT EXISTS embedding vector GENERATED ALWAYS AS (l2_normalize((data::jsonb ->> 'embedding')::vector)) STORED")
            .execute(&mut **tx).await?;
        self.configure_index(tx).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS file_search_chunks_text ON file_search_chunks USING gin (to_tsvector('simple', data::jsonb ->> 'text'))")
            .execute(&mut **tx).await?;
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
            sqlx::query_scalar("SELECT obj_description(c.oid, 'pg_class') FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid WHERE c.oid = to_regclass(format('%I.%I', current_schema(), $1)) AND i.indisvalid AND i.indisready")
                .bind(format!("file_search_vector_{}", self.dimensions))
                .fetch_optional(&mut **tx)
                .await?
                .flatten();
        Ok(existing.as_deref() == Some(format!("{method} {options}").as_str()))
    }

    pub(crate) async fn maintain_index(&self, pool: &DbPool) -> Result<(), FileSearchError> {
        // Established indexes need only an unlocked catalog read. Missing or
        // changed indexes are rechecked under the maintenance lock. Commit
        // before retrieval so the lock never covers streamed candidate queries.
        self.maintain_index_with_marker(pool, is_marked_ready()).await
    }

    async fn maintain_index_with_marker(&self, pool: &DbPool, supervisor_managed: bool) -> Result<(), FileSearchError> {
        let mut tx = pool.begin().await?;
        if supervisor_managed {
            self.verify_index(&mut tx).await?;
        } else if !self.index_is_current(&mut tx).await? {
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
        let local_name = format!("file_search_vector_{dims}");
        let name: String = sqlx::query_scalar("SELECT format('%I.%I', current_schema(), $1)")
            .bind(&local_name)
            .fetch_one(&mut **tx)
            .await?;
        let signature = format!("{method} {options}");
        if !self.index_is_current(tx).await? {
            sqlx::query(&format!("DROP INDEX IF EXISTS {name}"))
                .execute(&mut **tx)
                .await?;
            if let PgvectorIndex::Ivfflat { lists, .. } = self.index {
                let rows = self.embedding_count(tx).await?;
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
                "CREATE INDEX {local_name} ON file_search_chunks USING {method} ((embedding::vector({dims})) vector_cosine_ops) WITH ({options}) WHERE vector_dims(embedding) = {dims}"
            );
            sqlx::query(&sql).execute(&mut **tx).await?;
            sqlx::query(&format!("COMMENT ON INDEX {name} IS '{signature}'"))
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    async fn verify_extension(tx: &mut DbTransaction<'_>) -> Result<(), FileSearchError> {
        let version: Option<String> =
            sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
                .fetch_optional(&mut **tx)
                .await?;
        let Some(version) = version else {
            return invalid(
                "pgvector schema is not ready: install the vector extension before supervisor-managed startup",
            );
        };
        let parts = version.split('.').map(str::parse::<u32>).collect::<Result<Vec<_>, _>>();
        if !parts.is_ok_and(|parts| parts.len() == 3 && parts.as_slice() >= [0, 8, 0].as_slice()) {
            return invalid("pgvector 0.8.0 or later is required for filtered iterative index scans");
        }
        Ok(())
    }

    async fn verify_schema(&self, tx: &mut DbTransaction<'_>) -> Result<(), FileSearchError> {
        Self::verify_extension(tx).await?;
        let projection_ready: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = 'file_search_chunks'::regclass AND a.attname = 'embedding' AND a.attgenerated = 's' AND a.atttypid = 'vector'::regtype AND NOT a.attisdropped)",
        ).fetch_one(&mut **tx).await?;
        let text_index_ready: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid JOIN pg_am am ON am.oid = c.relam WHERE c.oid = to_regclass(format('%I.file_search_chunks_text', current_schema())) AND i.indrelid = 'file_search_chunks'::regclass AND i.indisvalid AND i.indisready AND am.amname = 'gin')",
        ).fetch_one(&mut **tx).await?;
        if !projection_ready || !text_index_ready {
            return invalid(
                "pgvector schema is not ready: apply the vector projection and text index before supervisor-managed startup",
            );
        }
        self.verify_index(tx).await
    }

    async fn verify_index(&self, tx: &mut DbTransaction<'_>) -> Result<(), FileSearchError> {
        if self.index_is_current(tx).await? {
            return Ok(());
        }
        // An untrained IVFFlat configuration deliberately uses exact distance
        // retrieval. Supervisor mode leaves training and index creation to the operator.
        if let PgvectorIndex::Ivfflat { lists, .. } = self.index {
            let existing: Option<String> =
                sqlx::query_scalar("SELECT to_regclass(format('%I.%I', current_schema(), $1))::text")
                    .bind(format!("file_search_vector_{}", self.dimensions))
                    .fetch_one(&mut **tx)
                    .await?;
            let rows = self.embedding_count(tx).await?;
            if existing.is_none() && rows < i64::from(lists) * 1000 {
                return Ok(());
            }
        }
        invalid(
            "pgvector schema is not ready: build the configured ANN index before supervisor-managed startup or search",
        )
    }

    async fn embedding_count(&self, tx: &mut DbTransaction<'_>) -> Result<i64, FileSearchError> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE vector_dims(embedding) = $1")
                .bind(i32::from(self.dimensions))
                .fetch_one(&mut **tx)
                .await?,
        )
    }
}

#[cfg(test)]
#[path = "pgvector_tests.rs"]
mod tests;
