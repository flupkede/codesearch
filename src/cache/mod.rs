mod file_meta;

pub use file_meta::FileMetaStore;

use moka::sync::Cache;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// High-performance embedding cache with memory awareness
pub struct EmbeddingCache {
    cache: Cache<String, Arc<Vec<f32>>>,
    hits: AtomicU64,
    misses: AtomicU64,
    max_memory_mb: usize,
}

impl EmbeddingCache {
    #[allow(dead_code)] // Reserved for future caching layer
    pub fn new(max_memory_mb: usize) -> Self {
        // Calculate max entries based on memory budget
        let avg_embedding_size = 384 * std::mem::size_of::<f32>(); // 384-dim f32 vector
        let max_entries = (max_memory_mb * 1024 * 1024) / avg_embedding_size;

        let cache = Cache::builder()
            .max_capacity(max_entries as u64)
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

    /// Get or compute an embedding
    #[allow(dead_code)] // Reserved for future caching layer
    pub fn get_or_compute<F>(&self, key: &str, compute: F) -> Arc<Vec<f32>>
    where
        F: FnOnce() -> Vec<f32>,
    {
        if let Some(value) = self.cache.get(key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            value
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            let value = Arc::new(compute());
            self.cache.insert(key.to_string(), value.clone());
            value
        }
    }

    /// Get cache hit rate
    #[allow(dead_code)] // Reserved for future caching layer
    pub fn hit_rate(&self) -> f64 {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let total = hits + misses;

        if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        }
    }

    /// Get cache statistics
    #[allow(dead_code)] // Reserved for future caching layer
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            size: self.cache.entry_count(),
            max_memory_mb: self.max_memory_mb,
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields used via stats() method
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub size: u64,
    pub max_memory_mb: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache() {
        let cache = EmbeddingCache::new(100);

        let result = cache.get_or_compute("test", || vec![1.0, 2.0, 3.0]);
        assert_eq!(*result, vec![1.0, 2.0, 3.0]);

        let stats = cache.stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 0);
    }
}
