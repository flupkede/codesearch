mod batch;
mod cache;
mod embedder;

pub use batch::{BatchEmbedder, EmbeddedChunk};
pub use cache::{
    CacheStats, CachedBatchEmbedder, PersistentCacheStats, PersistentEmbeddingCache, QueryCache,
    QueryCacheStats,
};
pub use embedder::{FastEmbedder, ModelType};

use anyhow::Result;
use std::env;
use std::sync::{Arc, Mutex};

/// High-level embedding service that combines all features
pub struct EmbeddingService {
    cached_embedder: CachedBatchEmbedder,
    model_type: ModelType,
    query_cache: QueryCache,
    persistent_cache: Option<PersistentEmbeddingCache>,
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

        // Initialize persistent embedding cache (disk-backed, survives restarts)
        // This is critical for fast branch switches: embeddings for previously-seen
        // content are looked up by content hash instead of recomputed via ONNX.
        let persistent_cache = match PersistentEmbeddingCache::open(model_type.short_name()) {
            Ok(cache) => {
                tracing::debug!("📦 Persistent embedding cache opened");
                Some(cache)
            }
            Err(e) => {
                tracing::warn!(
                    "⚠️  Failed to open persistent embedding cache: {} (continuing without)",
                    e
                );
                None
            }
        };

        Ok(Self {
            cached_embedder,
            model_type,
            query_cache,
            persistent_cache,
        })
    }

    /// Embed a batch of chunks with caching.
    ///
    /// When persistent cache is available, checks it first by content hash.
    /// Only chunks not found in the persistent cache go through ONNX inference.
    /// Newly computed embeddings are stored back in the persistent cache.
    pub fn embed_chunks(
        &mut self,
        chunks: Vec<crate::chunker::Chunk>,
    ) -> Result<Vec<EmbeddedChunk>> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }

        let persistent_cache = self.persistent_cache.as_ref();
        if persistent_cache.is_none() {
            // No persistent cache — use in-memory only path
            return self.cached_embedder.embed_chunks(chunks);
        }
        let cache = persistent_cache.unwrap();

        // Phase 1: Check persistent cache for each chunk by content hash
        let mut results: Vec<(usize, EmbeddedChunk)> = Vec::with_capacity(chunks.len());
        let mut misses: Vec<(usize, crate::chunker::Chunk)> = Vec::new();

        for (i, chunk) in chunks.iter().enumerate() {
            match cache.get(&chunk.hash) {
                Ok(Some(embedding)) => {
                    results.push((i, EmbeddedChunk::new(chunk.clone(), embedding)));
                }
                _ => {
                    misses.push((i, chunk.clone()));
                }
            }
        }

        let cache_hits = results.len();
        let cache_misses = misses.len();

        // Phase 2: Embed cache misses via the normal pipeline (ONNX inference)
        if !misses.is_empty() {
            let miss_chunks: Vec<crate::chunker::Chunk> =
                misses.iter().map(|(_, c)| c.clone()).collect();
            let embedded = self.cached_embedder.embed_chunks(miss_chunks)?;

            // Phase 3: Store newly computed embeddings in persistent cache
            let entries: Vec<(&str, &[f32])> = embedded
                .iter()
                .map(|ec| (ec.chunk.hash.as_str(), ec.embedding.as_slice()))
                .collect();
            if let Err(e) = cache.put_batch(&entries) {
                tracing::warn!("⚠️  Failed to write to persistent embedding cache: {}", e);
            }

            // Evict old entries if cache exceeds size limit
            let max_entries = std::env::var("CODESEARCH_EMBEDDING_CACHE_MAX_ENTRIES")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(crate::constants::DEFAULT_EMBEDDING_CACHE_MAX_ENTRIES);
            if let Err(e) = cache.evict_if_needed(max_entries) {
                tracing::warn!("⚠️  Embedding cache eviction failed: {}", e);
            }

            // Merge with cache hits, preserving original order
            for ((original_idx, _), embedded_chunk) in misses.iter().zip(embedded.into_iter()) {
                results.push((*original_idx, embedded_chunk));
            }
        }

        if cache_hits > 0 {
            tracing::debug!(
                "📦 Embedded {} chunks ({} cache hits, {} computed)",
                results.len(),
                cache_hits,
                cache_misses
            );
        }

        // Sort by original index to maintain order
        results.sort_by_key(|(i, _)| *i);
        Ok(results.into_iter().map(|(_, ec)| ec).collect())
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

    /// Re-initialize persistent cache for the current model.
    ///
    /// The persistent cache is auto-initialized in the constructor.
    /// This method is only needed if the cache was explicitly cleared
    /// or failed to open during construction.
    #[allow(dead_code)]
    pub fn with_persistent_cache(&mut self) -> Result<()> {
        if self.persistent_cache.is_none() {
            let cache = PersistentEmbeddingCache::open(self.model_short_name())?;
            self.persistent_cache = Some(cache);
        }
        Ok(())
    }

    #[allow(dead_code)]
    /// Get persistent cache statistics
    pub fn persistent_cache_stats(&self) -> Option<PersistentCacheStats> {
        self.persistent_cache
            .as_ref()
            .and_then(|c| c.stats().ok())
    }
    #[allow(dead_code)]
    /// Clear the persistent cache
    pub fn clear_persistent_cache(&mut self) -> Result<()> {
        if let Some(cache) = &mut self.persistent_cache {
            cache.clear()?;
        }
        Ok(())
    }
    #[allow(dead_code)]
    /// Get reference to persistent cache (if initialized)
    pub fn persistent_cache(&self) -> Option<&PersistentEmbeddingCache> {
        self.persistent_cache.as_ref()
    }
    #[allow(dead_code)]
    /// Get mutable reference to persistent cache (if initialized)
    pub fn persistent_cache_mut(&mut self) -> Option<&mut PersistentEmbeddingCache> {
        self.persistent_cache.as_mut()
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

    fn test_cache_dir() -> std::path::PathBuf {
        crate::constants::get_global_models_cache_dir().unwrap()
    }

    #[test]
    #[ignore] // Requires model
    fn test_embed_query() {
        let mut service =
            EmbeddingService::with_cache_dir(ModelType::default(), Some(&test_cache_dir()))
                .unwrap();
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
