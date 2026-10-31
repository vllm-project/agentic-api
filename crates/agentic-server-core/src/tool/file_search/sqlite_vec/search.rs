//! Semantic retrieval from published sqlite-vec rows.

use super::{
    SqliteVecRepository,
    index::{normalized_vector, table_name},
};
use crate::tool::file_search::ranking;
use crate::{
    storage::file_search::StoredChunk,
    types::file_search::{FileSearchError, SearchRequest, SearchResult},
};

const MAX_CORPUS_BYTES: i64 = 64 * 1024 * 1024;
const MAX_CORPUS_CHUNKS: i64 = 10_000;

impl SqliteVecRepository {
    pub(super) async fn semantic_search(
        &self,
        store_ids: &[String],
        queries: &[String],
        query_embeddings: &[Vec<f64>],
        expected_dimensions: Option<usize>,
        request: &SearchRequest,
    ) -> Result<Vec<SearchResult>, FileSearchError> {
        if queries.len() != query_embeddings.len() {
            return Err(FileSearchError::Unavailable(
                "Indexed content changed during search; retry the request".into(),
            ));
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
        let mut matches = Vec::new();
        let mut tx = self.index.begin().await?;
        for vector in query_embeddings {
            if expected_dimensions.is_some_and(|dimensions| dimensions != vector.len()) {
                return Err(FileSearchError::ProviderProtocol);
            }
            if vector.iter().all(|value| *value == 0.0) {
                continue;
            }
            let encoded = normalized_vector(vector)?;
            for store_id in store_ids {
                let table = table_name(store_id)?;
                let published_rows: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM file_search_chunks WHERE store_id = $1")
                        .bind(store_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let indexed_published_rows: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM file_search_vec_map AS indexed \
                     JOIN file_search_chunks AS chunks ON chunks.store_id = indexed.store_id \
                       AND chunks.file_id = indexed.file_id AND chunks.chunk_index = indexed.chunk_index \
                     WHERE indexed.store_id = $1",
                )
                .bind(store_id)
                .fetch_one(&mut *tx)
                .await?;
                if indexed_published_rows != published_rows {
                    return Err(FileSearchError::Unavailable(
                        "SQLite vector index is incomplete; recreate this vector store".into(),
                    ));
                }
                let index_rows: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM file_search_vec_map WHERE store_id = $1")
                        .bind(store_id)
                        .fetch_one(&mut *tx)
                        .await?;
                if index_rows == 0 {
                    continue;
                }
                let sql = format!(
                    "SELECT chunks.data, nearest.distance FROM \
                     (SELECT rowid, distance FROM {table} WHERE embedding MATCH $1 AND k = $2) AS nearest \
                     JOIN file_search_vec_map AS indexed ON indexed.id = nearest.rowid \
                     JOIN file_search_chunks AS chunks ON chunks.store_id = indexed.store_id \
                       AND chunks.file_id = indexed.file_id AND chunks.chunk_index = indexed.chunk_index \
                     WHERE indexed.store_id = $3 ORDER BY nearest.distance"
                );
                let rows: Vec<(String, Option<f64>)> = sqlx::query_as(&sql)
                    .bind(&encoded)
                    .bind(index_rows)
                    .bind(store_id)
                    .fetch_all(&mut *tx)
                    .await?;
                if i64::try_from(rows.len()).unwrap_or(i64::MAX) != published_rows {
                    return Err(FileSearchError::Unavailable(
                        "SQLite vector index is incomplete; recreate this vector store".into(),
                    ));
                }
                for (data, distance) in rows {
                    if let Some(distance) = distance.filter(|distance| distance.is_finite()) {
                        matches.push((
                            serde_json::from_str::<StoredChunk>(&data)?,
                            (1.0 - distance).clamp(0.0, 1.0),
                        ));
                    }
                }
            }
        }
        tx.commit().await?;
        Ok(ranking::rank_semantic_candidates(matches, request))
    }
}
