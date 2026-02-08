use super::batch::EmbeddedChunk;
use crate::chunker::Chunk;
use anyhow::Result;
use moka::sync::Cache;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Cache for embeddings keyed by chunk hash
///
/// Uses Moka for high-performance caching with automatic memory management.
/// Automatically evicts entries when memory limit is reached using LRU policy.
/// Chunks are identified by their SHA-256 content hash.
pub struct EmbeddingCache {
    cache: Cache<String, Arc<Vec<f32>>>,
    hits: AtomicU64,
    misses: AtomicU64,
    max_memory_mb: usize,
}

impl EmbeddingCache {
    /// Create a new empty cache with default memory limit
    pub fn new() -> Self {
        Self::with_memory_limit_mb(crate::constants::DEFAULT_CACHE_MAX_MEMORY_MB)
    }

    /// Create a new cache with specified memory limit in MB
    pub fn with_memory_limit_mb(max_memory_mb: usize) -> Self {
        // max_capacity is used as MAX WEIGHT when weigher is provided
        let max_weight = (max_memory_mb * 1024 * 1024) as u64;

        let cache = Cache::builder()
            .max_capacity(max_weight)
            .weigher(|_key: &String, value: &Arc<Vec<f32>>| {
                (value.len() * std::mem::size_of::<f32>()) as u32
            })
            .build();

        Self {
            cache,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            max_memory_mb,
        }
    }

    /// Get embedding from cache if available
    pub fn get(&self, chunk: &Chunk) -> Option<Vec<f32>> {
        if let Some(embedding) = self.cache.get(&chunk.hash) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(embedding.as_ref().clone())
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// Store embedding in cache (with automatic eviction if needed)
    #[allow(dead_code)] // Reserved for direct cache access
    pub fn put(&self, chunk: &Chunk, embedding: Vec<f32>) {
        self.cache.insert(chunk.hash.clone(), Arc::new(embedding));
    }

    /// Store an embedded chunk (with automatic eviction if needed)
    pub fn put_embedded(&self, embedded: &EmbeddedChunk) {
        self.cache.insert(
            embedded.chunk.hash.clone(),
            Arc::new(embedded.embedding.clone()),
        );
    }

    /// Check if cache contains embedding for chunk
    #[allow(dead_code)] // Reserved for cache probing
    pub fn contains(&self, chunk: &Chunk) -> bool {
        self.cache.contains_key(&chunk.hash)
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            size: self.cache.entry_count() as usize,
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            max_memory_mb: self.max_memory_mb,
            max_entries: (self.max_memory_mb * 1024 * 1024) / (384 * std::mem::size_of::<f32>()),
        }
    }

    /// Clear cache
    #[allow(dead_code)] // Reserved for cache management
    pub fn clear(&self) {
        self.cache.invalidate_all();
        self.cache.run_pending_tasks();
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
    }

    /// Get cache size (note: Moka cache is eventually consistent)
    #[allow(dead_code)] // Reserved for cache stats
    pub fn len(&self) -> usize {
        self.cache.run_pending_tasks();
        self.cache.entry_count() as usize
    }

    /// Check if cache is empty
    #[allow(dead_code)] // Reserved for cache stats
    pub fn is_empty(&self) -> bool {
        self.cache.run_pending_tasks();
        self.cache.entry_count() == 0
    }

    /// Get current memory usage estimate (in bytes)
    pub fn memory_usage_bytes(&self) -> usize {
        self.cache.run_pending_tasks();
        self.cache.weighted_size() as usize
    }

    /// Get current memory usage estimate (in MB)
    pub fn memory_usage_mb(&self) -> f64 {
        self.memory_usage_bytes() as f64 / (1024.0 * 1024.0)
    }
}

impl Default for EmbeddingCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub size: usize,
    pub hits: u64,
    pub misses: u64,
    pub max_memory_mb: usize,
    pub max_entries: usize,
}

impl CacheStats {
    pub fn hit_rate(&self) -> f32 {
        let total = self.hits + self.misses;
        if total == 0 {
            return 0.0;
        }
        self.hits as f32 / total as f32
    }

    #[allow(dead_code)] // Reserved for stats display
    pub fn total_requests(&self) -> u64 {
        self.hits + self.misses
    }
}

/// Cached batch embedder that uses an embedding cache with memory limits
pub struct CachedBatchEmbedder {
    pub batch_embedder: super::batch::BatchEmbedder,
    cache: EmbeddingCache,
}

impl CachedBatchEmbedder {
    /// Create a new cached batch embedder with default memory limit
    #[allow(dead_code)] // Reserved for cached embedding mode
    pub fn new(batch_embedder: super::batch::BatchEmbedder) -> Self {
        Self {
            batch_embedder,
            cache: EmbeddingCache::new(),
        }
    }

    /// Create with custom memory limit (in MB)
    pub fn with_memory_limit(
        batch_embedder: super::batch::BatchEmbedder,
        max_memory_mb: usize,
    ) -> Self {
        Self {
            batch_embedder,
            cache: EmbeddingCache::with_memory_limit_mb(max_memory_mb),
        }
    }

    /// Embed chunks using cache when possible
    pub fn embed_chunks(&mut self, chunks: Vec<Chunk>) -> Result<Vec<EmbeddedChunk>> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }

        let total = chunks.len();
        let mut embedded_chunks = Vec::with_capacity(total);
        let mut chunks_to_embed = Vec::new();
        let mut cache_indices = Vec::new();

        // Check cache first (silent - no verbose output)
        for (idx, chunk) in chunks.iter().enumerate() {
            if let Some(embedding) = self.cache.get(chunk) {
                embedded_chunks.push(EmbeddedChunk::new(chunk.clone(), embedding));
            } else {
                chunks_to_embed.push(chunk.clone());
                cache_indices.push(idx);
            }
        }

        // Embed remaining chunks
        if !chunks_to_embed.is_empty() {
            let newly_embedded = self.batch_embedder.embed_chunks(chunks_to_embed)?;

            // Store in cache (automatic eviction if memory limit reached)
            for embedded in &newly_embedded {
                self.cache.put_embedded(embedded);
            }

            embedded_chunks.extend(newly_embedded);
        }

        Ok(embedded_chunks)
    }

    /// Embed a single chunk with caching
    #[allow(dead_code)] // Reserved for single-chunk caching
    pub fn embed_chunk(&mut self, chunk: Chunk) -> Result<EmbeddedChunk> {
        if let Some(embedding) = self.cache.get(&chunk) {
            return Ok(EmbeddedChunk::new(chunk, embedding));
        }

        let embedded = self.batch_embedder.embed_chunk(chunk)?;
        self.cache.put_embedded(&embedded);

        Ok(embedded)
    }

    /// Get cache statistics
    pub fn cache_stats(&self) -> CacheStats {
        self.cache.stats()
    }

    /// Clear cache
    #[allow(dead_code)] // Reserved for cache reset
    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    /// Get embedding dimensions
    pub fn dimensions(&self) -> usize {
        self.batch_embedder.dimensions()
    }

    /// Get cache reference
    pub fn cache(&self) -> &EmbeddingCache {
        &self.cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::ChunkKind;

    #[test]
    fn test_cache_creation() {
        let cache = EmbeddingCache::new();
        assert_eq!(
            cache.max_memory_mb,
            crate::constants::DEFAULT_CACHE_MAX_MEMORY_MB
        );
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }

    #[test]
    fn test_cache_with_memory_limit() {
        let cache = EmbeddingCache::with_memory_limit_mb(100);
        assert_eq!(cache.max_memory_mb, 100);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_cache_put_get() {
        let cache = EmbeddingCache::new();

        let chunk = Chunk::new(
            "fn test() {}".to_string(),
            0,
            1,
            ChunkKind::Function,
            "test.rs".to_string(),
        );

        let embedding = vec![1.0, 2.0, 3.0];

        // Initially not in cache
        assert!(cache.get(&chunk).is_none());

        // Put in cache
        cache.put(&chunk, embedding.clone());

        // Now should be in cache
        assert!(cache.contains(&chunk));
        let retrieved = cache.get(&chunk).unwrap();
        assert_eq!(retrieved, embedding);

        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_cache_stats() {
        let cache = EmbeddingCache::new();

        let chunk1 = Chunk::new(
            "fn test1() {}".to_string(),
            0,
            1,
            ChunkKind::Function,
            "test.rs".to_string(),
        );

        let chunk2 = Chunk::new(
            "fn test2() {}".to_string(),
            2,
            3,
            ChunkKind::Function,
            "test.rs".to_string(),
        );

        cache.put(&chunk1, vec![1.0, 2.0, 3.0]);

        // Hit
        cache.get(&chunk1);

        // Miss
        cache.get(&chunk2);

        // Hit
        cache.get(&chunk1);

        let stats = cache.stats();
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.total_requests(), 3);
        assert!((stats.hit_rate() - 0.666).abs() < 0.01);
    }

    #[test]
    fn test_cache_clear() {
        let cache = EmbeddingCache::new();

        let chunk = Chunk::new(
            "fn test() {}".to_string(),
            0,
            1,
            ChunkKind::Function,
            "test.rs".to_string(),
        );

        cache.put(&chunk, vec![1.0, 2.0, 3.0]);
        assert_eq!(cache.len(), 1);

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }

    #[test]
    fn test_embedded_chunk_put() {
        let cache = EmbeddingCache::new();

        let chunk = Chunk::new(
            "fn test() {}".to_string(),
            0,
            1,
            ChunkKind::Function,
            "test.rs".to_string(),
        );

        let embedded = EmbeddedChunk::new(chunk.clone(), vec![1.0, 2.0, 3.0]);

        cache.put_embedded(&embedded);

        assert!(cache.contains(&chunk));
        let retrieved = cache.get(&chunk).unwrap();
        assert_eq!(retrieved, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_cache_deduplication() {
        let cache = EmbeddingCache::new();

        // Same content = same hash
        let chunk1 = Chunk::new(
            "fn test() {}".to_string(),
            0,
            1,
            ChunkKind::Function,
            "test.rs".to_string(),
        );

        let chunk2 = Chunk::new(
            "fn test() {}".to_string(),
            10,
            11,
            ChunkKind::Function,
            "other.rs".to_string(),
        );

        // Both should have same hash
        assert_eq!(chunk1.hash, chunk2.hash);

        // Put with chunk1
        cache.put(&chunk1, vec![1.0, 2.0, 3.0]);

        // Should be able to retrieve with chunk2 (same content hash)
        assert!(cache.contains(&chunk2));
        let retrieved = cache.get(&chunk2).unwrap();
        assert_eq!(retrieved, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_memory_usage_tracking() {
        let cache = EmbeddingCache::new();

        let chunk = Chunk::new(
            "fn test() {}".to_string(),
            0,
            1,
            ChunkKind::Function,
            "test.rs".to_string(),
        );

        // Add embedding with 3 floats = 12 bytes
        cache.put(&chunk, vec![1.0, 2.0, 3.0]);

        let bytes = cache.memory_usage_bytes();
        assert!(bytes > 0);

        let mb = cache.memory_usage_mb();
        assert!(mb > 0.0 && mb < 1.0); // Should be < 1 MB
    }

    #[test]
    fn test_cache_with_memory_limit_eviction() {
        // Create a very small cache (1KB)
        let cache = EmbeddingCache::with_memory_limit_mb(1);

        // This can fit at most ~1-2 embeddings (each ~1536 bytes for 384-dim)
        for i in 0..10 {
            let chunk = Chunk::new(
                format!("fn test{}() {{}}", i),
                0,
                1,
                ChunkKind::Function,
                "test.rs".to_string(),
            );

            // Create a 384-dim embedding
            let embedding: Vec<f32> = (0..384).map(|x| x as f32).collect();
            cache.put(&chunk, embedding);
        }

        // Cache should have automatically evicted old entries to stay within limit
        let stats = cache.stats();
        assert!(stats.size < 10, "Cache should have evicted entries");
    }
}
