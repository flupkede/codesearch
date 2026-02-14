mod batch;
mod cache;
mod embedder;

pub use batch::{BatchEmbedder, EmbeddedChunk};
pub use cache::{CacheStats, CachedBatchEmbedder, QueryCache, QueryCacheStats};
pub use embedder::{FastEmbedder, ModelType};

use anyhow::Result;
use std::env;
use std::sync::{Arc, Mutex};

/// High-level embedding service that combines all features
pub struct EmbeddingService {
    cached_embedder: CachedBatchEmbedder,
    model_type: ModelType,
    query_cache: QueryCache,
}

impl EmbeddingService {
    /// Create a new embedding service with default model
    pub fn new() -> Result<Self> {
        Self::with_model(ModelType::default())
    }

    /// Create a new embedding service with specified model
    pub fn with_model(model_type: ModelType) -> Result<Self> {
        Self::with_cache_dir(model_type, None)
    }

    /// Create a new embedding service with specified model and cache directory
    pub fn with_cache_dir(
        model_type: ModelType,
        cache_dir: Option<&std::path::Path>,
    ) -> Result<Self> {
        let embedder = FastEmbedder::with_cache_dir(model_type, cache_dir)?;
        let arc_embedder = Arc::new(Mutex::new(embedder));
        let batch_embedder = BatchEmbedder::new(arc_embedder);

        // Get cache memory limit from environment variable
        let cache_limit_mb = env::var("CODESEARCH_CACHE_MAX_MEMORY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(crate::constants::DEFAULT_CACHE_MAX_MEMORY_MB);

        let cached_embedder =
            CachedBatchEmbedder::with_memory_limit(batch_embedder, cache_limit_mb);

        // Initialize query cache (separate from chunk cache)
        let query_cache = QueryCache::new();

        Ok(Self {
            cached_embedder,
            model_type,
            query_cache,
        })
    }

    /// Embed a batch of chunks with caching
    pub fn embed_chunks(
        &mut self,
        chunks: Vec<crate::chunker::Chunk>,
    ) -> Result<Vec<EmbeddedChunk>> {
        self.cached_embedder.embed_chunks(chunks)
    }

    /// Embed query text (with caching)
    pub fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        // Check query cache first
        if let Some(cached) = self.query_cache.get(query) {
            return Ok(cached);
        }

        // Cache miss - embed the query
        let embedder_arc = &self.cached_embedder.batch_embedder.embedder;
        let embedding = embedder_arc
            .lock()
            .map_err(|e| anyhow::anyhow!("Embedder mutex poisoned: {}", e))?
            .embed_one(query)?;

        // Store in cache
        self.query_cache.put(query, embedding.clone());

        Ok(embedding)
    }

    /// Batch embed multiple query texts with caching (single ONNX call for misses)
    pub fn embed_queries_batch(&mut self, queries: &[String]) -> Result<Vec<Vec<f32>>> {
        if queries.is_empty() {
            return Ok(Vec::new());
        }

        let total = queries.len();
        let mut results = Vec::with_capacity(total);
        let mut queries_to_embed = Vec::new();
        let mut cache_indices = Vec::new();

        // Check cache first
        for (idx, query) in queries.iter().enumerate() {
            if let Some(cached) = self.query_cache.get(query) {
                results.push(cached);
            } else {
                queries_to_embed.push(query.clone());
                cache_indices.push(idx);
            }
        }

        // Batch embed remaining queries (single ONNX call)
        if !queries_to_embed.is_empty() {
            // Clone once before passing to embed_batch (which takes ownership)
            let queries_for_caching = queries_to_embed.clone();
            let embedder_arc = &self.cached_embedder.batch_embedder.embedder;
            let mut embedder = embedder_arc
                .lock()
                .map_err(|e| anyhow::anyhow!("Embedder mutex poisoned: {}", e))?;

            let new_embeddings = embedder.embed_batch(queries_to_embed)?;

            // Store in cache and add to results
            for (i, embedding) in new_embeddings.into_iter().enumerate() {
                self.query_cache
                    .put(&queries_for_caching[i], embedding.clone());

                // Place at correct position
                results.insert(cache_indices[i], embedding);
            }
        }

        Ok(results)
    }

    /// Get embedding dimensions
    pub fn dimensions(&self) -> usize {
        self.cached_embedder.dimensions()
    }

    /// Get model information
    pub fn model_name(&self) -> &str {
        self.model_type.name()
    }

    /// Get model short name (for storage)
    pub fn model_short_name(&self) -> &str {
        self.model_type.short_name()
    }

    /// Get cache statistics
    #[allow(dead_code)] // Part of public API for debugging/monitoring
    pub fn cache_stats(&self) -> CacheStats {
        self.cached_embedder.cache_stats()
    }

    /// Get query cache statistics
    #[allow(dead_code)] // Part of public API for debugging/monitoring
    pub fn query_cache_stats(&self) -> QueryCacheStats {
        self.query_cache.stats()
    }
}

impl Default for EmbeddingService {
    fn default() -> Self {
        Self::new().expect("Failed to create default embedding service")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_type_default() {
        let model = ModelType::default();
        assert_eq!(model.dimensions(), 384);
    }

    #[test]
    #[ignore] // Requires model download
    fn test_embedding_service_creation() {
        let service = EmbeddingService::new();
        assert!(service.is_ok());

        let service = service.unwrap();
        assert_eq!(service.dimensions(), 384);
    }

    #[test]
    #[ignore] // Requires model
    fn test_embed_query() {
        let mut service = EmbeddingService::new().unwrap();
        let query_embedding = service.embed_query("find authentication code").unwrap();

        assert_eq!(query_embedding.len(), 384);
    }

    #[test]
    #[ignore] // search method not implemented - uses VectorStore instead
    fn test_embed_and_search() {
        // EmbeddingService no longer has search - VectorStore handles searching
        // Test kept for documentation purposes
    }

    #[test]
    #[ignore] // search method not implemented - uses VectorStore instead
    fn test_search() {
        // EmbeddingService no longer has search - VectorStore handles searching
        // Test kept for documentation purposes
    }
}
