//! Neural reranking using cross-encoder models
//!
//! Provides second-pass reranking using fastembed's TextRerank
//! with the Jina Reranker v1 Turbo model for improved accuracy.

use crate::info_print;
use anyhow::Result;
use fastembed::{RerankInitOptions, RerankerModel, TextRerank};

/// Score blending weights (per osgrep pattern)
/// 57.5% rerank + 42.5% RRF
pub const RERANK_WEIGHT: f32 = 0.575;
pub const RRF_WEIGHT: f32 = 0.425;

/// Neural reranker using cross-encoder model
#[allow(dead_code)] // model_name used for diagnostics
pub struct NeuralReranker {
    reranker: TextRerank,
    model_name: String,
}

impl NeuralReranker {
    /// Create a new neural reranker with the default Jina model
    pub fn new() -> Result<Self> {
        Self::with_model(RerankerModel::JINARerankerV1TurboEn)
    }

    /// Create a neural reranker with a specific model
    pub fn with_model(model: RerankerModel) -> Result<Self> {
        let model_name = model.to_string();
        info_print!("Loading reranker model: {}", model_name);

        let mut options = RerankInitOptions::default();
        options.model_name = model;
        options.show_download_progress = false;

        let reranker = TextRerank::try_new(options)?;

        info_print!("Reranker model loaded successfully!");

        Ok(Self {
            reranker,
            model_name,
        })
    }

    /// Get the model name
    #[allow(dead_code)] // Reserved for diagnostics
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    /// Rerank documents given a query
    ///
    /// Returns Vec of (original_index, rerank_score) sorted by score descending
    pub fn rerank(&mut self, query: &str, documents: &[String]) -> Result<Vec<(usize, f32)>> {
        if documents.is_empty() {
            return Ok(vec![]);
        }

        // Convert to &str references for fastembed API
        let doc_refs: Vec<&str> = documents.iter().map(|s| s.as_str()).collect();

        // Rerank using the cross-encoder
        let results = self.reranker.rerank(
            query, doc_refs, false, // Don't return documents (we have them)
            None,  // Use default batch size
        )?;

        // Convert to (index, score) pairs
        Ok(results.into_iter().map(|r| (r.index, r.score)).collect())
    }

    /// Rerank and blend scores with existing RRF scores
    ///
    /// Uses weighted blending: final_score = RERANK_WEIGHT * rerank_score + RRF_WEIGHT * rrf_score
    pub fn rerank_and_blend(
        &mut self,
        query: &str,
        documents: &[String],
        rrf_scores: &[f32],
    ) -> Result<Vec<(usize, f32)>> {
        if documents.is_empty() {
            return Ok(vec![]);
        }

        assert_eq!(
            documents.len(),
            rrf_scores.len(),
            "Documents and RRF scores must have same length"
        );

        // Get rerank scores
        let rerank_results = self.rerank(query, documents)?;

        // Normalize rerank scores to [0, 1] using sigmoid (scores can be negative)
        let normalized: Vec<(usize, f32)> = rerank_results
            .iter()
            .map(|(idx, score)| (*idx, sigmoid(*score)))
            .collect();

        // Normalize RRF scores to [0, 1] (they're already positive, just need min-max)
        let rrf_min = rrf_scores.iter().cloned().fold(f32::INFINITY, f32::min);
        let rrf_max = rrf_scores.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let rrf_range = (rrf_max - rrf_min).max(0.0001); // Avoid division by zero

        // Blend scores
        let mut blended: Vec<(usize, f32)> = normalized
            .into_iter()
            .map(|(idx, rerank_norm)| {
                let rrf_norm = (rrf_scores[idx] - rrf_min) / rrf_range;
                let blended_score = RERANK_WEIGHT * rerank_norm + RRF_WEIGHT * rrf_norm;
                (idx, blended_score)
            })
            .collect();

        // Sort by blended score descending
        blended.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        Ok(blended)
    }
}

/// Sigmoid function to normalize scores to [0, 1]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sigmoid() {
        assert!((sigmoid(0.0) - 0.5).abs() < 0.0001);
        assert!(sigmoid(10.0) > 0.99);
        assert!(sigmoid(-10.0) < 0.01);
    }

    #[test]
    #[ignore] // Requires model download
    fn test_reranker_creation() {
        let reranker = NeuralReranker::new();
        assert!(reranker.is_ok());
    }

    #[test]
    #[ignore] // Requires model download
    fn test_rerank_basic() {
        let mut reranker = NeuralReranker::new().unwrap();

        let query = "How do I authenticate users?";
        let documents = vec![
            "fn authenticate(user: &str, password: &str) -> bool { ... }".to_string(),
            "fn calculate_sum(a: i32, b: i32) -> i32 { a + b }".to_string(),
            "impl UserAuth for App { fn login(&self, credentials: Credentials) -> Result<Token> }"
                .to_string(),
        ];

        let results = reranker.rerank(query, &documents).unwrap();

        // Should return all documents
        assert_eq!(results.len(), 3);

        // Results should be sorted by score descending
        for i in 0..results.len() - 1 {
            assert!(results[i].1 >= results[i + 1].1);
        }
    }
}
