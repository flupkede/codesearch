use super::batch::EmbeddedChunk;
use crate::chunker::Chunk;
use crate::lmdb_registry::TrackedEnv;
use anyhow::Result;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use heed::types::*;
use heed::{Database, EnvOpenOptions};
use moka::sync::Cache;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// Cache for embeddings keyed by chunk hash
///
/// Uses Moka for high-performance caching with automatic memory management.
/// Automatically evicts entries when memory limit is reached using LRU policy.
/// Chunks are identified by their SHA-256 content hash.
pub struct EmbeddingCache {
    cache: Cache<String, Arc<Vec<f32>>>,
    hits: AtomicU64,
    misses: AtomicU64,
    #[allow(dead_code)] // Used in stats()
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
    #[allow(dead_code)] // Part of public API for debugging/monitoring
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
    #[allow(dead_code)] // Part of public API for debugging/monitoring
    pub fn memory_usage_bytes(&self) -> usize {
        self.cache.run_pending_tasks();
        self.cache.weighted_size() as usize
    }

    /// Get current memory usage estimate (in MB)
    #[allow(dead_code)] // Part of public API for debugging/monitoring
    pub fn memory_usage_mb(&self) -> f64 {
        self.memory_usage_bytes() as f64 / (1024.0 * 1024.0)
    }
}

impl Default for EmbeddingCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Query embedding cache for fast repeated searches
///
/// Caches query embeddings to avoid re-embedding the same queries.
/// Query reuse is very high in interactive sessions (e.g., "authentication",
/// "handle_file_modified"). Uses Moka LRU cache with automatic eviction.
pub struct QueryCache {
    cache: Cache<String, Arc<Vec<f32>>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl QueryCache {
    /// Create a new query cache with default limit (50MB)
    pub fn new() -> Self {
        Self::with_memory_limit_mb(50)
    }

    /// Create a query cache with specified memory limit in MB
    pub fn with_memory_limit_mb(max_memory_mb: usize) -> Self {
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
        }
    }

    /// Get query embedding from cache
    pub fn get(&self, query: &str) -> Option<Vec<f32>> {
        if let Some(embedding) = self.cache.get(query) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(embedding.as_ref().clone())
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// Store query embedding in cache
    pub fn put(&self, query: &str, embedding: Vec<f32>) {
        self.cache.insert(query.to_string(), Arc::new(embedding));
    }

    /// Check if cache contains query embedding
    #[allow(dead_code)]
    pub fn contains(&self, query: &str) -> bool {
        self.cache.contains_key(query)
    }

    /// Get cache statistics
    pub fn stats(&self) -> QueryCacheStats {
        QueryCacheStats {
            size: self.cache.entry_count() as usize,
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
        }
    }

    /// Clear cache
    #[allow(dead_code)]
    pub fn clear(&self) {
        self.cache.invalidate_all();
        self.cache.run_pending_tasks();
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
    }

    /// Get cache size
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.cache.run_pending_tasks();
        self.cache.entry_count() as usize
    }

    /// Check if cache is empty
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.cache.run_pending_tasks();
        self.cache.entry_count() == 0
    }

    /// Get memory usage in bytes
    #[allow(dead_code)]
    pub fn memory_usage_bytes(&self) -> usize {
        self.cache.run_pending_tasks();
        self.cache.weighted_size() as usize
    }

    /// Get memory usage in MB
    #[allow(dead_code)]
    pub fn memory_usage_mb(&self) -> f64 {
        self.memory_usage_bytes() as f64 / (1024.0 * 1024.0)
    }
}

impl Default for QueryCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Query cache statistics
#[derive(Debug, Clone)]
#[allow(dead_code)] // Reserved for debugging/monitoring API
pub struct QueryCacheStats {
    pub size: usize,
    pub hits: u64,
    pub misses: u64,
}

impl QueryCacheStats {
    #[allow(dead_code)] // Part of debugging/monitoring API
    pub fn query_hit_rate(&self) -> f32 {
        let total = self.hits + self.misses;
        if total == 0 {
            return 0.0;
        }
        self.hits as f32 / total as f32
    }

    #[allow(dead_code)] // Reserved for stats display
    pub fn query_total_requests(&self) -> u64 {
        self.hits + self.misses
    }
}

/// Persistent embedding cache for fast branch switches
///
/// Stores embeddings on disk keyed by content hash, allowing embeddings to survive
/// across MCP restarts and be reused when switching between branches. When a file
/// changes, we check if we've already computed embeddings for that content before
/// re-running ONNX inference.
///
/// Cache location: ~/.codesearch/embedding_cache/<model_short_name>/
/// Key: content_hash (SHA256) → Vec<f32> (embedding vector)
///
/// This is separate from the in-memory EmbeddingCache which uses Moka for
/// automatic memory management. The persistent cache provides long-term storage.
pub struct PersistentEmbeddingCache {
    env: TrackedEnv,
    db: Database<Str, SerdeBincode<Vec<f32>>>,
    cache_dir: PathBuf,
    /// Model name — used as the key into [`LIVE_CACHE_STATS`] and needed in
    /// [`Drop`] to unregister the entry.
    model_name: String,
}

// ── Live cache stats registry ──────────────────────────────────
//
// Process-global mirror of every currently-open persistent cache's stats, keyed
// by model name. The `EmbeddingService` opens the cache once and holds it for
// the lifetime of the service; while that handle is alive, any in-process
// caller (notably the `doctor` TUI handler, which runs inside `serve`) can read
// accurate stats WITHOUT opening a second LMDB environment — which would trip
// `TrackedEnv`'s double-open guard.
//
// Entries are written by `refresh_live_stats` (called from `open`, `put`,
// `put_batch`, `clear`, `evict_if_needed`) and removed in `Drop`.
static LIVE_CACHE_STATS: OnceLock<DashMap<String, PersistentCacheStats>> = OnceLock::new();

fn live_cache_stats() -> &'static DashMap<String, PersistentCacheStats> {
    LIVE_CACHE_STATS.get_or_init(DashMap::new)
}

impl PersistentEmbeddingCache {
    /// Resolve the on-disk cache directory for a model — without opening LMDB
    /// and without creating the directory.
    ///
    /// `~/.codesearch/embedding_cache/<model_name>`. Callers that only need to
    /// inspect the cache (existence, file size) or check whether the LMDB env is
    /// already held open should use this instead of [`Self::open`], which would
    /// trip the double-open guard when the serve process already holds the env.
    pub fn cache_dir_for(model_name: &str) -> Result<PathBuf> {
        let models_dir = crate::constants::get_global_models_cache_dir()?;
        let cache_dir = models_dir
            .parent() // ~/.codesearch/
            .ok_or_else(|| anyhow::anyhow!("Could not get parent directory of models cache"))?
            .join("embedding_cache")
            .join(model_name);
        Ok(cache_dir)
    }

    /// Read cache file statistics (`data.mdb` size + last modified) without
    /// opening the LMDB environment.
    ///
    /// Returns `None` when `data.mdb` does not exist (cache never initialised
    /// or wiped). `entries` is always `0` because counting requires an open
    /// LMDB read transaction; callers that need the count must hold an open
    /// handle (e.g. via [`Self::open`] when [`crate::lmdb_registry::is_open`]
    /// reports the env is not already held).
    pub fn file_stats(cache_dir: &Path) -> Option<PersistentCacheStats> {
        let data_mdb = cache_dir.join("data.mdb");
        let meta = std::fs::metadata(&data_mdb).ok()?;
        let last_access = meta.modified().ok().map(DateTime::from);
        Some(PersistentCacheStats {
            entries: 0,
            file_size_bytes: meta.len(),
            last_access,
        })
    }

    /// Open persistent cache for a specific model
    ///
    /// Creates the cache directory if it doesn't exist and opens an LMDB
    /// environment for storing embeddings. Each model has its own cache to avoid
    /// mixing incompatible embeddings.
    pub fn open(model_name: &str) -> Result<Self> {
        let cache_dir = Self::cache_dir_for(model_name)?;

        std::fs::create_dir_all(&cache_dir).map_err(|e| {
            anyhow::anyhow!(
                "Failed to create embedding cache directory {}: {}",
                cache_dir.display(),
                e
            )
        })?;

        // SAFETY: heed's `EnvOpenOptions::open` is unsafe because the caller must
        // ensure no other process maps this LMDB environment with incompatible options
        // (different map_size or flags) at the same time. The cache directory is
        // process-private under the user's codesearch state directory, and we open it
        // exactly once per process via this constructor.
        // TrackedEnv additionally prevents double-open within the same process.
        let mut opts = EnvOpenOptions::new();
        opts.map_size(512 * 1024 * 1024).max_dbs(1); // 512MB — plenty for cache
                                                     // SAFETY: `NO_TLS` only changes reader-slot tracking. See `BASE_ENV_FLAGS`.
        unsafe { opts.flags(crate::lmdb_registry::BASE_ENV_FLAGS) };
        let env = unsafe {
            TrackedEnv::open(
                &opts,
                &cache_dir,
                &format!("PersistentEmbeddingCache({})", model_name),
            )?
        };

        let mut wtxn = env.write_txn()?;
        let db = env.create_database(&mut wtxn, Some("embeddings"))?;
        wtxn.commit()?;

        let cache = Self {
            env,
            db,
            cache_dir,
            model_name: model_name.to_string(),
        };
        // Publish initial stats so any in-process reader (e.g. `doctor` running
        // inside `serve`) sees them without having to open its own LMDB env.
        cache.refresh_live_stats();
        Ok(cache)
    }

    /// Refresh this cache's entry in [`LIVE_CACHE_STATS`] by reading live stats
    /// via a fresh read transaction.
    ///
    /// Called after every write (`put`, `put_batch`, `clear`, `evict_if_needed`)
    /// and after `open`. Cheap: one read txn + one `metadata` syscall. Failures
    /// (e.g. concurrent resize) are silently ignored — the previous stats remain.
    fn refresh_live_stats(&self) {
        match self.stats() {
            Ok(s) => {
                live_cache_stats().insert(self.model_name.clone(), s);
            }
            Err(_) => {
                // Keep stale entry; better a slightly-old count than none.
            }
        }
    }

    /// Read the live stats for a model WITHOUT opening the LMDB environment.
    ///
    /// Returns `Some` only when a `PersistentEmbeddingCache` for `model_name`
    /// is currently open in this process (i.e. held alive by an
    /// `EmbeddingService`). Returns `None` otherwise — callers should then fall
    /// back to [`Self::file_stats`] (size/mtime only) or [`Self::open`] (when
    /// the cache is known to be free, e.g. standalone CLI).
    ///
    /// This is the safe path for in-process diagnostic callers (`doctor`)
    /// because it never touches the LMDB env and therefore cannot trigger
    /// `TrackedEnv`'s double-open guard.
    pub fn live_stats(model_name: &str) -> Option<PersistentCacheStats> {
        live_cache_stats().get(model_name).map(|r| r.clone())
    }

    /// Get embedding from cache by content hash
    pub fn get(&self, content_hash: &str) -> Result<Option<Vec<f32>>> {
        let rtxn = self.env.read_txn()?;
        Ok(self.db.get(&rtxn, content_hash)?)
    }
    #[allow(dead_code)]
    /// Store embedding in cache
    pub fn put(&self, content_hash: &str, embedding: &[f32]) -> Result<()> {
        let mut wtxn = self.env.write_txn()?;
        self.db.put(&mut wtxn, content_hash, &embedding.to_vec())?;
        wtxn.commit()?;
        self.refresh_live_stats();
        Ok(())
    }

    /// Batch insert for efficiency (single transaction)
    pub fn put_batch(&self, entries: &[(&str, &[f32])]) -> Result<()> {
        let mut wtxn = self.env.write_txn()?;
        for (hash, embedding) in entries {
            self.db.put(&mut wtxn, hash, &embedding.to_vec())?;
        }
        wtxn.commit()?;
        self.refresh_live_stats();
        Ok(())
    }

    /// Get cache statistics
    pub fn stats(&self) -> Result<PersistentCacheStats> {
        let rtxn = self.env.read_txn()?;
        let count = self.db.len(&rtxn)?;
        let file_size = std::fs::metadata(self.cache_dir.join("data.mdb"))
            .map(|m| m.len())
            .unwrap_or(0);
        let last_access = std::fs::metadata(self.cache_dir.join("data.mdb"))
            .and_then(|m| m.modified())
            .ok()
            .map(DateTime::from);
        Ok(PersistentCacheStats {
            entries: count as usize,
            file_size_bytes: file_size,
            last_access,
        })
    }

    /// Evict entries when cache exceeds max size
    ///
    #[allow(dead_code)]
    /// Deletes first N entries (by lexicographic key order) to get back under limit.
    /// Returns number of entries deleted. Note: LMDB `Str` keys iterate in
    /// lexicographic order, not insertion order. For SHA256 hashes this means
    /// eviction is effectively random, not LRU — but still correctly bounds size.
    pub fn evict_if_needed(&self, max_entries: usize) -> Result<usize> {
        let rtxn = self.env.read_txn()?;
        let count = self.db.len(&rtxn)? as usize;
        drop(rtxn);

        if count <= max_entries {
            return Ok(0);
        }

        // Delete first entries (LMDB iterates in lexicographic b-tree order, not insertion order)
        let to_delete = count - max_entries;

        // Collect keys first to avoid borrow checker issues with iterator
        let rtxn = self.env.read_txn()?;
        let keys_to_delete: Vec<String> = self
            .db
            .iter(&rtxn)?
            .take(to_delete)
            .map(|result| {
                result
                    .map(|(key, _)| key.to_string())
                    .map_err(|e| anyhow::anyhow!("Failed to collect key: {}", e))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        drop(rtxn);

        // Now delete the collected keys
        let mut wtxn = self.env.write_txn()?;
        for key in &keys_to_delete {
            self.db.delete(&mut wtxn, key)?;
        }

        wtxn.commit()?;
        self.refresh_live_stats();
        Ok(keys_to_delete.len())
    }

    /// Clear all cached embeddings
    pub fn clear(&self) -> Result<()> {
        let mut wtxn = self.env.write_txn()?;
        self.db.clear(&mut wtxn)?;
        wtxn.commit()?;
        self.refresh_live_stats();
        Ok(())
    }
    #[allow(dead_code)]
    /// Get number of entries in cache
    pub fn len(&self) -> Result<usize> {
        let rtxn = self.env.read_txn()?;
        Ok(self.db.len(&rtxn)? as usize)
    }
    #[allow(dead_code)]
    /// Check if cache is empty
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Get cache directory path
    #[allow(dead_code)] // Reserved for debugging
    pub fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }
}

impl Drop for PersistentEmbeddingCache {
    fn drop(&mut self) {
        // Body runs BEFORE field drops (Rust drop order: body, then fields in
        // declaration order). `self.model_name` is still valid here. Remove the
        // live-stats entry so `live_stats` correctly reports `None` once the
        // cache is closed (rather than serving stale numbers forever).
        //
        // Note: `self.env` (TrackedEnv) drops AFTER this body returns. That is
        // fine — LIVE_CACHE_STATS is independent of heed's env tracking, so the
        // brief window where our stats slot is gone but heed's env is still
        // alive cannot cause any inconsistency.
        live_cache_stats().remove(&self.model_name);
    }
}

/// Persistent cache statistics
#[derive(Debug, Clone)]
pub struct PersistentCacheStats {
    pub entries: usize,
    pub file_size_bytes: u64,
    pub last_access: Option<DateTime<Utc>>,
}
#[allow(dead_code)]
impl PersistentCacheStats {
    /// Get file size in MB
    pub fn file_size_mb(&self) -> f64 {
        self.file_size_bytes as f64 / (1024.0 * 1024.0)
    }

    /// Get estimated memory size in MB (entries × 1.5KB)
    pub fn estimated_memory_mb(&self) -> f64 {
        self.entries as f64 * 1.536 / 1024.0
    }
}

impl QueryCacheStats {
    #[allow(dead_code)] // Part of debugging/monitoring API
    pub fn hit_rate(&self) -> f32 {
        let total = self.hits + self.misses;
        if total == 0 {
            return 0.0;
        }
        self.hits as f32 / total as f32
    }

    #[allow(dead_code)] // Part of debugging/monitoring API
    pub fn total_requests(&self) -> u64 {
        self.hits + self.misses
    }
}

/// Cache statistics
#[derive(Debug, Clone)]
#[allow(dead_code)] // Part of public API for debugging/monitoring
pub struct CacheStats {
    #[allow(dead_code)] // Part of public API for debugging/monitoring
    pub size: usize,
    pub hits: u64,
    pub misses: u64,
    #[allow(dead_code)] // Part of public API for debugging/monitoring
    pub max_memory_mb: usize,
    #[allow(dead_code)] // Part of public API for debugging/monitoring
    pub max_entries: usize,
}

impl CacheStats {
    #[allow(dead_code)] // Part of public API for debugging/monitoring
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
    #[allow(dead_code)] // Part of public API for debugging/monitoring
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
    #[allow(dead_code)] // Part of public API for debugging/monitoring
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
    #[allow(dead_code)] // Part of public API for debugging/monitoring
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

    #[test]
    fn test_live_stats_registry_lifecycle() {
        // Use a unique model name so this test never collides with a real cache
        // or with parallel test runs. The cache dir lives under the user's global
        // ~/.codesearch/embedding_cache/ — clean it up at the end.
        let model_name = format!(
            "__test_live_stats_tmp_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        // Before opening: no live stats for this model.
        assert!(
            PersistentEmbeddingCache::live_stats(&model_name).is_none(),
            "live_stats should be None before any cache is opened"
        );

        let cache_dir = PersistentEmbeddingCache::cache_dir_for(&model_name).unwrap();

        // Open populates the registry via refresh_live_stats().
        let cache = PersistentEmbeddingCache::open(&model_name).unwrap();
        let live = PersistentEmbeddingCache::live_stats(&model_name)
            .expect("live_stats should be Some immediately after open");
        assert_eq!(
            live.entries, 0,
            "freshly opened cache should have 0 entries"
        );

        // put_batch updates the registry.
        let emb: Vec<f32> = (0..384).map(|x| x as f32).collect();
        let entries: Vec<(&str, &[f32])> = vec![("hash1", &emb), ("hash2", &emb), ("hash3", &emb)];
        cache.put_batch(&entries).unwrap();
        let live = PersistentEmbeddingCache::live_stats(&model_name).unwrap();
        assert_eq!(
            live.entries, 3,
            "live_stats should reflect entries written via put_batch"
        );

        // clear updates the registry back to zero.
        cache.clear().unwrap();
        let live = PersistentEmbeddingCache::live_stats(&model_name).unwrap();
        assert_eq!(live.entries, 0, "live_stats should be 0 after clear");

        // Dropping the cache removes the registry entry (Drop impl).
        drop(cache);
        assert!(
            PersistentEmbeddingCache::live_stats(&model_name).is_none(),
            "live_stats should be None after the cache is dropped"
        );

        // Clean up the test cache directory.
        let _ = std::fs::remove_dir_all(&cache_dir);
    }
}
