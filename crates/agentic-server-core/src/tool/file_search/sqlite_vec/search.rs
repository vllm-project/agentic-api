//! Semantic retrieval from published sqlite-vec rows.

use std::collections::HashSet;

use super::{
    SqliteVecRepository,
    index::{normalized_vector, table_name},
};
use crate::tool::file_search::ranking;
use crate::{
    storage::file_search::{MAX_CORPUS_BYTES, MAX_CORPUS_CHUNKS, StoredChunk},
    types::file_search::{FileSearchError, SearchMode, SearchRequest, SearchResult},
};
use sqlx::SqliteConnection;

const SQLITE_VEC_MAX_K: i64 = 4096;

impl SqliteVecRepository {
    pub(super) async fn semantic_search(
        &self,
        store_ids: &[String],
        queries: &[String],
        query_embeddings: &[Vec<f64>],
        expected_dimensions: Option<usize>,
        request: &SearchRequest,
    ) -> Result<Vec<SearchResult>, FileSearchError> {
        if store_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = (1..=store_ids.len())
            .map(|index| format!("${index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(data)), 0) FROM file_search_chunks WHERE store_id IN ({placeholders})"
        );
        let mut query = sqlx::query_as::<_, (i64, i64)>(&sql);
        for store_id in store_ids {
            query = query.bind(store_id);
        }
        let (count, bytes) = query.fetch_one(&self.index).await?;
        if count > MAX_CORPUS_CHUNKS || bytes > MAX_CORPUS_BYTES {
            return Err(FileSearchError::Unavailable(
                "Selected corpus exceeds the exact retrieval capacity; search fewer vector stores or files".into(),
            ));
        }
        if count == 0 {
            return Ok(Vec::new());
        }
        if queries.len() != query_embeddings.len() {
            return Err(FileSearchError::InvalidRequest(
                "Semantic search requires one embedding per query".into(),
            ));
        }
        for vector in query_embeddings {
            if expected_dimensions.is_some_and(|dimensions| dimensions != vector.len()) {
                return Err(FileSearchError::ProviderProtocol);
            }
            normalized_vector(vector)?;
        }
        let mut matches = Vec::new();
        let mut tx = self.index.begin().await?;
        let mut stores = Vec::with_capacity(store_ids.len());
        for store_id in store_ids {
            let table = table_name(store_id)?;
            let (published_rows, index_rows) = validate_index_rows(&mut tx, store_id, &table).await?;
            if index_rows > 0 {
                stores.push((store_id, table, published_rows, index_rows));
            }
        }
        let mut use_exact = request.filters.is_some();
        'queries: for vector in query_embeddings {
            if use_exact {
                break;
            }
            if vector.iter().all(|value| *value == 0.0) {
                continue;
            }
            let encoded = normalized_vector(vector)?;
            for (store_id, table, published_rows, index_rows) in &stores {
                if let Some(candidates) = fetch_candidates(
                    &mut tx,
                    table,
                    store_id,
                    &encoded,
                    *index_rows,
                    *published_rows,
                    request,
                )
                .await?
                {
                    matches.extend(candidates);
                } else {
                    use_exact = true;
                    break 'queries;
                }
            }
        }
        tx.commit().await?;
        if use_exact {
            let chunks = self.metadata.chunks(store_ids).await?;
            return Ok(ranking::rank(
                chunks,
                queries,
                query_embeddings,
                SearchMode::Semantic,
                request,
            ));
        }
        Ok(ranking::rank_semantic_candidates(matches, request))
    }
}

async fn fetch_candidates(
    connection: &mut SqliteConnection,
    table: &str,
    store_id: &str,
    encoded: &str,
    index_rows: i64,
    published_rows: i64,
    request: &SearchRequest,
) -> Result<Option<Vec<(StoredChunk, f64)>>, FileSearchError> {
    let target = request.max_num_results.unwrap_or(10).max(1);
    let mut k = i64::try_from(target.saturating_add(1))
        .unwrap_or(i64::MAX)
        .min(index_rows);
    let sql = format!(
        "SELECT chunks.data, nearest.distance FROM \
         (SELECT rowid, distance FROM {table} WHERE embedding MATCH $1 AND k = $2) AS nearest \
         JOIN file_search_vec_map AS indexed ON indexed.id = nearest.rowid \
         JOIN file_search_chunks AS chunks ON chunks.store_id = indexed.store_id \
           AND chunks.file_id = indexed.file_id AND chunks.chunk_index = indexed.chunk_index \
         WHERE indexed.store_id = $3 ORDER BY nearest.distance"
    );
    loop {
        if k > SQLITE_VEC_MAX_K {
            return Ok(None);
        }
        let rows: Vec<(String, Option<f64>)> = sqlx::query_as(&sql)
            .bind(encoded)
            .bind(k)
            .bind(store_id)
            .fetch_all(&mut *connection)
            .await?;
        if k == index_rows && i64::try_from(rows.len()).unwrap_or(i64::MAX) != published_rows {
            return Err(FileSearchError::Unavailable(
                "SQLite vector index is incomplete; recreate this vector store".into(),
            ));
        }
        let last_score = rows
            .last()
            .and_then(|(_, distance)| *distance)
            .filter(|distance| distance.is_finite())
            .map_or(0.0, |distance| (1.0 - distance).clamp(0.0, 1.0));
        let mut candidates = Vec::with_capacity(rows.len());
        let mut seen = HashSet::new();
        let mut unique_scores = Vec::new();
        for (data, distance) in rows {
            if let Some(distance) = distance.filter(|distance| distance.is_finite()) {
                let chunk = serde_json::from_str::<StoredChunk>(&data)?;
                let score = (1.0 - distance).clamp(0.0, 1.0);
                if seen.insert((chunk.file_id.clone(), chunk.text.clone())) {
                    unique_scores.push(score);
                }
                candidates.push((chunk, score));
            }
        }
        if k == index_rows {
            return Ok(Some(candidates));
        }
        if unique_scores.get(target - 1).is_some_and(|score| *score > last_score) {
            return Ok(Some(candidates));
        }
        k = k.saturating_mul(2).min(index_rows);
    }
}

async fn validate_index_rows(
    connection: &mut SqliteConnection,
    store_id: &str,
    table: &str,
) -> Result<(i64, i64), FileSearchError> {
    let published_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE store_id = $1")
        .bind(store_id)
        .fetch_one(&mut *connection)
        .await?;
    let indexed_published_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM file_search_vec_map AS indexed \
         JOIN file_search_chunks AS chunks ON chunks.store_id = indexed.store_id \
           AND chunks.file_id = indexed.file_id AND chunks.chunk_index = indexed.chunk_index \
         WHERE indexed.store_id = $1",
    )
    .bind(store_id)
    .fetch_one(&mut *connection)
    .await?;
    let index_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_search_vec_map WHERE store_id = $1")
        .bind(store_id)
        .fetch_one(&mut *connection)
        .await?;
    if published_rows == 0 && index_rows == 0 {
        return Ok((0, 0));
    }
    let vector_rows: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(&mut *connection)
        .await?;
    if indexed_published_rows != published_rows || vector_rows != index_rows {
        return Err(FileSearchError::Unavailable(
            "SQLite vector index is incomplete; recreate this vector store".into(),
        ));
    }
    Ok((published_rows, index_rows))
}
