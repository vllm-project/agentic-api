//! Exact cosine, BM25, and normalized reciprocal-rank fusion over a bounded corpus.

use std::collections::{HashMap, HashSet};

use crate::{
    storage::file_search::{ChunkOrigin, RetrievedChunk, StoredChunk},
    types::file_search::{ChunkRetrievalParams, Ranker, SearchContent, SearchMode, SearchRequest, SearchResult},
};

/// Keeps all matching origins of a deduplicated passage until final visibility.
pub(super) struct RankedCandidate {
    pub result: SearchResult,
    pub origins: Vec<ScoredOrigin>,
}

pub(super) struct ScoredOrigin {
    pub origin: ChunkOrigin,
    pub score: f64,
}

pub(super) fn rank(
    chunks: Vec<RetrievedChunk>,
    queries: &[String],
    query_embeddings: &[Vec<f64>],
    mode: SearchMode,
    request: &SearchRequest,
    ranker: Ranker,
    defaults: &ChunkRetrievalParams,
) -> Vec<RankedCandidate> {
    let chunks: Vec<_> = chunks
        .into_iter()
        .filter(|chunk| {
            request
                .filters
                .as_ref()
                .is_none_or(|filter| filter.matches(&chunk.chunk.attributes))
        })
        .collect();
    let (chunks, origins): (Vec<_>, Vec<_>) = chunks
        .into_iter()
        .map(|retrieved| (retrieved.chunk, retrieved.origin))
        .unzip();
    let mut origin_scores = vec![None::<f64>; chunks.len()];
    let mut best: HashMap<(&str, &str), (usize, f64)> = HashMap::new();
    let threshold = request
        .ranking_options
        .as_ref()
        .and_then(|options| options.score_threshold)
        .unwrap_or(0.0);
    for (query_index, query) in queries.iter().enumerate() {
        let keyword = if mode == SearchMode::Semantic {
            vec![0.0; chunks.len()]
        } else {
            bm25(&chunks, query)
        };
        let semantic: Vec<f64> = if mode == SearchMode::Keyword {
            vec![0.0; chunks.len()]
        } else {
            chunks
                .iter()
                .map(|chunk| {
                    chunk
                        .embedding
                        .as_ref()
                        .map_or(0.0, |embedding| cosine(embedding, &query_embeddings[query_index]))
                })
                .collect()
        };
        let scores = match mode {
            SearchMode::Keyword => keyword,
            SearchMode::Semantic => semantic,
            SearchMode::Hybrid => hybrid_scores(&semantic, &keyword, request, ranker, defaults),
        };
        for (index, score) in scores.into_iter().enumerate() {
            if !ranker.uses_model() && (score <= 0.0 || score < threshold) {
                continue;
            }
            origin_scores[index] = Some(origin_scores[index].map_or(score, |previous| previous.max(score)));
            let key = (chunks[index].file_id.as_str(), chunks[index].text.as_str());
            let entry = best.entry(key).or_insert((index, score));
            if score > entry.1 {
                *entry = (index, score);
            }
        }
    }
    let mut origin_groups: HashMap<(&str, &str), Vec<ScoredOrigin>> = HashMap::new();
    for ((chunk, origin), score) in chunks.iter().zip(origins).zip(origin_scores) {
        if let Some(score) = score {
            origin_groups
                .entry((&chunk.file_id, &chunk.text))
                .or_default()
                .push(ScoredOrigin { origin, score });
        }
    }
    let mut matches: Vec<_> = best.into_values().collect();
    matches.sort_by(|(left, left_score), (right, right_score)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| chunks[*left].file_id.cmp(&chunks[*right].file_id))
            .then_with(|| chunks[*left].chunk_index.cmp(&chunks[*right].chunk_index))
    });
    matches.truncate(request.max_num_results.unwrap_or(10));
    matches
        .into_iter()
        .map(|(index, score)| {
            let chunk = &chunks[index];
            RankedCandidate {
                origins: origin_groups
                    .remove(&(chunk.file_id.as_str(), chunk.text.as_str()))
                    .unwrap_or_default(),
                result: SearchResult {
                    file_id: chunk.file_id.clone(),
                    filename: chunk.filename.clone(),
                    score,
                    attributes: chunk.attributes.clone(),
                    content: vec![SearchContent {
                        type_: "text".into(),
                        text: chunk.text.clone(),
                    }],
                },
            }
        })
        .collect()
}

fn hybrid_scores(
    semantic: &[f64],
    keyword: &[f64],
    request: &SearchRequest,
    ranker: Ranker,
    defaults: &ChunkRetrievalParams,
) -> Vec<f64> {
    let options = request.ranking_options.as_ref();
    let weights = options.and_then(|options| options.hybrid_search.as_ref());
    let alpha = options
        .and_then(|options| options.alpha)
        .unwrap_or(defaults.weighted_search_alpha);
    let embedding = options
        .and_then(|options| options.weights.as_ref())
        .map(|weights| weights.vector)
        .or_else(|| weights.and_then(|weights| weights.embedding_weight))
        .unwrap_or(if weights.is_some() { 1.0 } else { alpha });
    let text = options
        .and_then(|options| options.weights.as_ref())
        .map(|weights| weights.keyword)
        .or_else(|| weights.and_then(|weights| weights.text_weight))
        .unwrap_or(if weights.is_some() { 1.0 } else { 1.0 - alpha });
    if ranker == Ranker::Weighted {
        weighted(semantic, keyword, embedding, text)
    } else {
        fuse(
            semantic,
            keyword,
            embedding,
            text,
            options
                .and_then(|options| options.impact_factor)
                .unwrap_or(defaults.rrf_impact_factor),
        )
    }
}

fn cosine(left: &[f64], right: &[f64]) -> f64 {
    if left.len() != right.len() {
        return 0.0;
    }
    // Scale before accumulating norms so finite provider vectors cannot overflow
    // or underflow cosine similarity solely because of their magnitude.
    let left_scale = left.iter().map(|value| value.abs()).fold(0.0_f64, f64::max);
    let right_scale = right.iter().map(|value| value.abs()).fold(0.0_f64, f64::max);
    if left_scale == 0.0 || right_scale == 0.0 {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut left_norm = 0.0;
    let mut right_norm = 0.0;
    for (left, right) in left.iter().zip(right) {
        let left = left / left_scale;
        let right = right / right_scale;
        dot += left * right;
        left_norm += left * left;
        right_norm += right * right;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        return 0.0;
    }
    (dot / (left_norm.sqrt() * right_norm.sqrt())).clamp(0.0, 1.0)
}

fn terms(text: &str) -> impl Iterator<Item = &str> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
}

#[allow(
    clippy::cast_precision_loss,
    reason = "corpus and query sizes are bounded far below f64 integer precision"
)]
fn bm25(chunks: &[StoredChunk], query: &str) -> Vec<f64> {
    if chunks.is_empty() {
        return Vec::new();
    }
    let query: HashSet<String> = terms(query).map(str::to_lowercase).collect();
    let mut document_frequencies: HashMap<String, usize> = HashMap::new();
    let mut documents = Vec::with_capacity(chunks.len());
    let mut total_length = 0usize;
    for chunk in chunks {
        let mut frequencies = HashMap::<String, usize>::new();
        let mut length = 0usize;
        // Only query terms are retained; corpus vocabulary cannot amplify memory usage.
        for term in terms(&chunk.text) {
            length += 1;
            let term = term.to_lowercase();
            if query.contains(&term) {
                *frequencies.entry(term).or_default() += 1;
            }
        }
        total_length += length;
        for term in frequencies.keys() {
            *document_frequencies.entry(term.clone()).or_default() += 1;
        }
        documents.push((length, frequencies));
    }
    let average = (total_length as f64 / chunks.len() as f64).max(1.0);
    documents
        .into_iter()
        .map(|(length, frequencies)| {
            let score: f64 = frequencies
                .iter()
                .map(|(term, frequency)| {
                    let df = *document_frequencies.get(term).unwrap_or(&0) as f64;
                    let idf = (1.0 + (chunks.len() as f64 - df + 0.5) / (df + 0.5)).ln();
                    let frequency = *frequency as f64;
                    idf * frequency * 2.2 / (frequency + 1.2 * (0.25 + 0.75 * length as f64 / average))
                })
                .sum();
            score / (1.0 + score)
        })
        .collect()
}

#[allow(clippy::cast_precision_loss, reason = "at most 10000 chunks are ranked")]
fn fuse(semantic: &[f64], keyword: &[f64], embedding_weight: f64, text_weight: f64, impact_factor: f64) -> Vec<f64> {
    let mut scores = vec![0.0; semantic.len()];
    for (input, weight) in [(semantic, embedding_weight), (keyword, text_weight)] {
        let mut ranked: Vec<_> = input
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, score)| *score > 0.0)
            .collect();
        ranked.sort_by(|(left, left_score), (right, right_score)| {
            right_score.total_cmp(left_score).then_with(|| left.cmp(right))
        });
        for (rank, (index, _)) in ranked.into_iter().enumerate() {
            scores[index] += (weight / (embedding_weight + text_weight)) * (impact_factor + 1.0)
                / (impact_factor + 1.0 + rank as f64);
        }
    }
    scores
}

#[allow(clippy::float_cmp, reason = "equal extrema mean the score list is exactly constant")]
fn weighted(semantic: &[f64], keyword: &[f64], embedding_weight: f64, text_weight: f64) -> Vec<f64> {
    fn normalize(input: &[f64]) -> Vec<f64> {
        let min = input
            .iter()
            .copied()
            .filter(|score| *score > 0.0)
            .fold(f64::INFINITY, f64::min);
        let max = input.iter().copied().fold(0.0, f64::max);
        input
            .iter()
            .map(|score| {
                if *score <= 0.0 {
                    0.0
                } else if max == min {
                    1.0
                } else {
                    (*score - min) / (max - min)
                }
            })
            .collect()
    }
    normalize(semantic)
        .into_iter()
        .zip(normalize(keyword))
        .map(|(semantic, keyword)| {
            (semantic * embedding_weight + keyword * text_weight) / (embedding_weight + text_weight)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::cosine;

    #[test]
    fn deduplicated_origins_must_independently_meet_the_score_threshold() {
        use super::*;
        let chunks = [vec![1.0, 0.0], vec![0.5, 0.75_f64.sqrt()]]
            .into_iter()
            .enumerate()
            .map(|(index, embedding)| RetrievedChunk {
                origin: ChunkOrigin {
                    store_id: format!("store-{index}"),
                    file_id: "file".into(),
                    chunk_index: 0,
                    generation: "generation".into(),
                },
                chunk: StoredChunk {
                    file_id: "file".into(),
                    filename: "source.txt".into(),
                    chunk_index: 0,
                    text: "same source".into(),
                    embedding_text: None,
                    embedding: Some(embedding),
                    attributes: std::collections::BTreeMap::default(),
                },
            })
            .collect();
        let request: SearchRequest =
            serde_json::from_str(r#"{"query":"source","ranking_options":{"score_threshold":0.8}}"#).unwrap();
        let results = rank(
            chunks,
            &["source".into()],
            &[vec![1.0, 0.0]],
            SearchMode::Semantic,
            &request,
            Ranker::None,
            &ChunkRetrievalParams::default(),
        );
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].origins.len(),
            1,
            "a below-threshold attachment cannot validate another origin's result"
        );
    }

    #[test]
    fn cosine_is_stable_for_finite_vectors_of_extreme_magnitude() {
        for scale in [1e-300, 1.0, 1e300] {
            assert!((cosine(&[scale, 0.0], &[scale, 0.0]) - 1.0).abs() < f64::EPSILON);
            assert!(cosine(&[scale, 0.0], &[0.0, scale]).abs() < f64::EPSILON);
        }
    }
}
