//! Reranking and result fusion strategies
//!
//! Provides RRF (Reciprocal Rank Fusion) for combining vector and FTS results,
//! and neural reranking using cross-encoder models for improved accuracy.

mod neural;

use std::collections::HashMap;

use crate::fts::FtsResult;
use crate::vectordb::SearchResult;

pub use neural::NeuralReranker;

/// Default RRF k parameter (per osgrep reference)
pub const DEFAULT_RRF_K: f32 = 20.0;

/// Fused search result combining vector and FTS scores
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields used for debugging/diagnostics
pub struct FusedResult {
    /// Chunk ID
    pub chunk_id: u32,
    /// Combined RRF score
    pub rrf_score: f32,
    /// Original vector similarity score (if present)
    pub vector_score: Option<f32>,
    /// Original FTS/BM25 score (if present)
    pub fts_score: Option<f32>,
    /// Vector rank (1-indexed, None if not in vector results)
    pub vector_rank: Option<usize>,
    /// FTS rank (1-indexed, None if not in FTS results)
    pub fts_rank: Option<usize>,
}

/// Reciprocal Rank Fusion (RRF) for combining search results
///
/// RRF formula: score = sum(1 / (k + rank)) for each ranking list
/// where k is a constant (default 20) and rank is 1-indexed position.
///
/// This is a proven technique for combining multiple ranking signals
/// without needing to normalize scores across different systems.
pub fn rrf_fusion(
    vector_results: &[SearchResult],
    fts_results: &[FtsResult],
    k: f32,
) -> Vec<FusedResult> {
    // Maps chunk_id -> (rrf_score, vector_score, fts_score, vector_rank, fts_rank)
    let mut scores: HashMap<u32, (f32, Option<f32>, Option<f32>, Option<usize>, Option<usize>)> =
        HashMap::new();

    // Process vector results
    for (rank, result) in vector_results.iter().enumerate() {
        let chunk_id = result.id;
        let rrf_score = 1.0 / (k + rank as f32 + 1.0);

        let entry = scores
            .entry(chunk_id)
            .or_insert((0.0, None, None, None, None));
        entry.0 += rrf_score;
        entry.1 = Some(result.score);
        entry.3 = Some(rank + 1);
    }

    // Process FTS results
    for (rank, result) in fts_results.iter().enumerate() {
        let chunk_id = result.chunk_id;
        let rrf_score = 1.0 / (k + rank as f32 + 1.0);

        let entry = scores
            .entry(chunk_id)
            .or_insert((0.0, None, None, None, None));
        entry.0 += rrf_score;
        entry.2 = Some(result.score);
        entry.4 = Some(rank + 1);
    }

    // Convert to FusedResult and sort by RRF score
    let mut results: Vec<FusedResult> = scores
        .into_iter()
        .map(
            |(chunk_id, (rrf_score, vector_score, fts_score, vector_rank, fts_rank))| FusedResult {
                chunk_id,
                rrf_score,
                vector_score,
                fts_score,
                vector_rank,
                fts_rank,
            },
        )
        .collect();

    // Sort by RRF score descending
    results.sort_by(|a, b| {
        b.rrf_score
            .partial_cmp(&a.rrf_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    results
}

/// Simple vector-only pass-through (no fusion)
pub fn vector_only(vector_results: &[SearchResult]) -> Vec<FusedResult> {
    vector_results
        .iter()
        .enumerate()
        .map(|(rank, result)| FusedResult {
            chunk_id: result.id,
            rrf_score: result.score,
            vector_score: Some(result.score),
            fts_score: None,
            vector_rank: Some(rank + 1),
            fts_rank: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_vector_result(id: u32, score: f32) -> SearchResult {
        SearchResult {
            id,
            score,
            path: format!("file_{}.rs", id),
            content: format!("content {}", id),
            start_line: 1,
            end_line: 10,
            kind: "function".to_string(),
            signature: None,
            context_prev: None,
            context_next: None,
            distance: 0.0,
            context: None,
            docstring: None,
            hash: String::new(),
        }
    }

    fn make_fts_result(id: u32, score: f32) -> FtsResult {
        FtsResult {
            chunk_id: id,
            score,
        }
    }

    #[test]
    fn test_rrf_fusion_basic() {
        let vector_results = vec![
            make_vector_result(1, 0.9),
            make_vector_result(2, 0.8),
            make_vector_result(3, 0.7),
        ];

        let fts_results = vec![
            make_fts_result(2, 10.0), // ID 2 is top in FTS
            make_fts_result(1, 8.0),
            make_fts_result(4, 6.0), // ID 4 only in FTS
        ];

        let fused = rrf_fusion(&vector_results, &fts_results, 20.0);

        // ID 2 should be top (rank 1 in FTS, rank 2 in vector)
        // ID 1 should be second (rank 1 in vector, rank 2 in FTS)
        assert!(!fused.is_empty());

        // Find IDs 1 and 2
        let id1 = fused.iter().find(|r| r.chunk_id == 1).unwrap();
        let id2 = fused.iter().find(|r| r.chunk_id == 2).unwrap();

        // Both should have contributions from both sources
        assert!(id1.vector_rank.is_some());
        assert!(id1.fts_rank.is_some());
        assert!(id2.vector_rank.is_some());
        assert!(id2.fts_rank.is_some());

        // ID 4 should only be in FTS
        let id4 = fused.iter().find(|r| r.chunk_id == 4).unwrap();
        assert!(id4.vector_rank.is_none());
        assert!(id4.fts_rank.is_some());
    }

    #[test]
    fn test_rrf_score_calculation() {
        // With k=20:
        // Rank 1: 1/(20+1) = 0.0476
        // Rank 2: 1/(20+2) = 0.0454
        let vector_results = vec![make_vector_result(1, 0.9)];
        let fts_results = vec![make_fts_result(1, 10.0)];

        let fused = rrf_fusion(&vector_results, &fts_results, 20.0);

        assert_eq!(fused.len(), 1);
        let result = &fused[0];

        // Should be sum of both contributions
        let expected = 1.0 / 21.0 + 1.0 / 21.0;
        assert!((result.rrf_score - expected).abs() < 0.0001);
    }

    #[test]
    fn test_vector_only() {
        let vector_results = vec![make_vector_result(1, 0.9), make_vector_result(2, 0.8)];

        let results = vector_only(&vector_results);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].chunk_id, 1);
        assert_eq!(results[0].rrf_score, 0.9);
        assert!(results[0].fts_score.is_none());
    }
}
