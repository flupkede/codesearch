mod batch;
mod cache;
mod embedder;

pub use batch::{BatchEmbedder, EmbeddedChunk};
pub use cache::{
    CacheStats, CachedBatchEmbedder, PersistentCacheStats, PersistentEmbeddingCache, QueryCache,
    QueryCacheStats,
};
pub use embedder::{
    is_model_in_cache, load_default_model, save_default_model, FastEmbedder, ModelType,
};
use anyhow::Result;
use std::collections::HashMap;
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
            for ((original_idx, _), embedded_chunk) in misses.iter().zip(embedded) {
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
            .embed_query(query)?;

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

            let new_embeddings = embedder.embed_queries(queries_to_embed)?;

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
    #[allow(dead_code)] // Public info accessor; mirrors model_short_name()
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
        self.persistent_cache.as_ref().and_then(|c| c.stats().ok())
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

/// Lazily-created, per-model cache of [`EmbeddingService`]s.
///
/// Serve mode is multi-repo and different repos may be indexed with different
/// embedding models (an older MiniLM index alongside a rebuilt EmbeddingGemma
/// one), so a single shared service is wrong: the query must be embedded with
/// the same model the target index was built with. The pool loads each model at
/// most once per serve instance and reuses it across MCP sessions and REST
/// handlers. Each model gets its own mutex, so queries against different models
/// do not serialise on one global lock.
#[derive(Default)]
pub struct EmbeddingServicePool {
    services: Mutex<HashMap<ModelType, Arc<Mutex<EmbeddingService>>>>,
    /// Per-model init locks. The `services` map lock is only ever held for a
    /// lookup — loading a model runs under the per-model init lock, so a slow
    /// (or wedged) load of one model cannot block lookups or loads of any
    /// other model.
    init_locks: Mutex<HashMap<ModelType, Arc<Mutex<()>>>>,
    cache_dir: Option<std::path::PathBuf>,
}

impl EmbeddingServicePool {
    /// Create a pool. `cache_dir` overrides the ONNX model cache directory
    /// (`None` = fastembed's configured cache, i.e. the global models dir).
    pub fn new(cache_dir: Option<std::path::PathBuf>) -> Self {
        Self {
            services: Mutex::new(HashMap::new()),
            init_locks: Mutex::new(HashMap::new()),
            cache_dir,
        }
    }

    /// Return the service for `model` if it is already loaded, without ever
    /// triggering a load. Search handlers use this to guarantee that a query
    /// never turns into a network fetch of model files.
    pub fn get_if_cached(&self, model: ModelType) -> Option<Arc<Mutex<EmbeddingService>>> {
        self.services.lock().ok()?.get(&model).cloned()
    }

    /// Return the service for `model`, loading its ONNX model on first use.
    ///
    /// The returned `Arc` is locked independently per model, so a caller can
    /// hold it across an `embed_query` without blocking other models.
    pub fn get(&self, model: ModelType) -> Result<Arc<Mutex<EmbeddingService>>> {
        if let Some(existing) = self.get_if_cached(model) {
            return Ok(existing);
        }
        let init_lock = {
            let mut guard = self
                .init_locks
                .lock()
                .map_err(|e| anyhow::anyhow!("Embedding service pool mutex poisoned: {e}"))?;
            guard.entry(model).or_default().clone()
        };
        // Held across the load: same-model callers wait here instead of
        // racing into duplicate loads. They block their own thread only —
        // no map lock and no other model is involved.
        let _init = init_lock
            .lock()
            .map_err(|e| anyhow::anyhow!("Embedding model init mutex poisoned: {e}"))?;
        // Someone may have finished the load while we waited on the init lock.
        if let Some(existing) = self.get_if_cached(model) {
            return Ok(existing);
        }
        let service = self.load_bounded(model)?;
        let arc = Arc::new(Mutex::new(service));
        let mut guard = self
            .services
            .lock()
            .map_err(|e| anyhow::anyhow!("Embedding service pool mutex poisoned: {e}"))?;
        guard.insert(model, arc.clone());
        Ok(arc)
    }

    /// Load `model` off the async critical path, bounded in time.
    ///
    /// `EmbeddingService::with_cache_dir` resolves files through hf-hub; on a
    /// cold cache that is a network download which can stall indefinitely on
    /// networks that black-hole the model host. Running it on a dedicated
    /// thread behind `recv_timeout` bounds the damage: callers get a
    /// fail-fast error pointing at `codesearch setup` instead of hanging.
    /// On timeout the loader thread is deliberately leaked — there is no safe
    /// way to kill a thread mid-download — but it holds no pool locks, so it
    /// can only waste its own resources until the underlying fetch gives up.
    fn load_bounded(&self, model: ModelType) -> Result<EmbeddingService> {
        let timeout_secs = std::env::var(crate::constants::MODEL_LOAD_TIMEOUT_SECS_ENV)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&s| s > 0)
            .unwrap_or(crate::constants::MODEL_LOAD_TIMEOUT_SECS);
        let (tx, rx) = std::sync::mpsc::channel();
        let cache_dir = self.cache_dir.clone();
        std::thread::spawn(move || {
            // The receiver may already be gone on timeout — nothing to do.
            let _ = tx.send(EmbeddingService::with_cache_dir(model, cache_dir.as_deref()));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(timeout_secs)) {
            Ok(Ok(service)) => Ok(service),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow::anyhow!(
                "embedding model '{model:?}' did not initialise within {timeout_secs}s — likely a \
                 blocked network fetch of an uncached model; pre-populate the \
                 model cache via `codesearch setup` and retry"
            )),
        }
    }

    /// Embed `chunks`, releasing the service lock between ONNX mini-batches.
    ///
    /// The per-model `Mutex<EmbeddingService>` is what serialises embedding
    /// between repos and queries. Holding it for a whole refresh batch (one
    /// `CODESEARCH_INCREMENTAL_BATCH_SIZE` window can hold thousands of
    /// chunks) made an interactive query embedding queue behind minutes of
    /// background inference. Each lock acquisition here covers exactly one
    /// ONNX mini-batch ([`FastEmbedder::effective_batch_size`]), followed by
    /// the optional `pause` duty cycle, so a long background pass yields the
    /// model between mini-batches.
    pub fn embed_chunks_yielding(
        &self,
        model: ModelType,
        chunks: Vec<crate::chunker::Chunk>,
        pause: std::time::Duration,
    ) -> Result<Vec<EmbeddedChunk>> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }
        let service = self.get(model)?;
        let group_size = FastEmbedder::effective_batch_size(model).max(1);
        let mut out: Vec<EmbeddedChunk> = Vec::with_capacity(chunks.len());
        for group in chunks.chunks(group_size) {
            {
                // Recover from poisoning: a panicked embed service still yields
                // a valid guard; the batch result carries correctness, and a
                // poison here must not hard-fail every later batch for the
                // process lifetime.
                let mut guard = service
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut embedded = guard.embed_chunks(group.to_vec())?;
                out.append(&mut embedded);
            }
            if !pause.is_zero() {
                std::thread::sleep(pause);
            }
        }
        Ok(out)
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

    /// Mini-batch size drives how long one lock acquisition lasts; pin the
    /// dimension-adaptive default and the env override.
    #[test]
    #[serial_test::serial]
    fn effective_batch_size_defaults_by_dimensions_and_env_overrides() {
        let _unset = crate::testing::EnvRestore::remove(&["CODESEARCH_BATCH_SIZE"]);
        assert_eq!(
            FastEmbedder::effective_batch_size(ModelType::AllMiniLML6V2Q),
            256,
            "384-dim models use 256-text mini-batches"
        );
        assert_eq!(
            FastEmbedder::effective_batch_size(ModelType::EmbeddingGemma300MQ4),
            128,
            "768-dim models use 128-text mini-batches"
        );
        assert_eq!(
            FastEmbedder::effective_batch_size(ModelType::BGELargeENV15),
            64,
            ">768-dim models use 64-text mini-batches"
        );

        let _override = crate::testing::EnvRestore::set(&[("CODESEARCH_BATCH_SIZE", "7")]);
        assert_eq!(
            FastEmbedder::effective_batch_size(ModelType::EmbeddingGemma300MQ4),
            7,
            "CODESEARCH_BATCH_SIZE must win over the adaptive default"
        );
    }

    /// The interleaved embed path must not even load the model for an empty
    /// batch; this is its no-op arm and the only one testable without ONNX.
    #[test]
    fn embed_chunks_yielding_empty_is_a_noop() {
        let pool = EmbeddingServicePool::new(None);
        let out = pool
            .embed_chunks_yielding(ModelType::default(), Vec::new(), std::time::Duration::ZERO)
            .expect("empty embed must succeed");
        assert!(out.is_empty());
    }

    /// `get_if_cached` is the search-path gate: it must answer "not loaded"
    /// without ever triggering a load. A lookup that could initialise a
    /// model would be useless as a "can I embed without a download?"
    /// preflight — the whole point is that search never fetches.
    #[test]
    fn get_if_cached_reports_fresh_pool_as_unloaded() {
        let pool = EmbeddingServicePool::new(None);
        assert!(pool.get_if_cached(ModelType::default()).is_none());
        assert!(pool.get_if_cached(ModelType::EmbeddingGemma300MQ4).is_none());
    }

    #[test]
    #[ignore] // Requires EmbeddingGemma300MQ4 in the local models cache
    fn get_publishes_the_service_to_get_if_cached() {
        let pool = EmbeddingServicePool::new(Some(test_cache_dir()));
        let model = ModelType::EmbeddingGemma300MQ4;
        assert!(
            pool.get_if_cached(model).is_none(),
            "fresh pool must report unloaded"
        );
        pool.get(model).expect("model load");
        assert!(
            pool.get_if_cached(model).is_some(),
            "get must publish the loaded service for later no-load lookups"
        );
    }

    /// Double-checked init: threads racing `get()` on the same model must all
    /// receive THE SAME service instance — a duplicate load would mean two
    /// ONNX runtimes for one model (double memory) and `Arc::ptr_eq` is the
    /// directly observable contract.
    #[test]
    #[ignore] // Requires EmbeddingGemma300MQ4 in the local models cache
    fn concurrent_get_returns_one_shared_service_per_model() {
        let pool = std::sync::Arc::new(EmbeddingServicePool::new(Some(test_cache_dir())));
        let model = ModelType::EmbeddingGemma300MQ4;
        let mut handles = Vec::new();
        for _ in 0..4 {
            let pool = pool.clone();
            handles.push(std::thread::spawn(move || {
                pool.get(model).expect("model load")
            }));
        }
        let first = handles.remove(0).join().expect("loader thread");
        for handle in handles {
            let service = handle.join().expect("loader thread");
            assert!(
                Arc::ptr_eq(&first, &service),
                "concurrent get() must share one service instance per model"
            );
        }
    }

    /// The index-metadata reader must invert `write_metadata_fields`, and must
    /// report "no answer" (None) for unknown/missing names rather than silently
    /// claiming the default — callers decide the fallback.
    #[test]
    fn test_model_type_round_trips_through_index_metadata() {
        for model in [
            ModelType::AllMiniLML6V2Q,
            ModelType::EmbeddingGemma300MQ4,
            ModelType::BGEBaseENV15,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut obj = serde_json::Map::new();
            model.write_metadata_fields(&mut obj);
            std::fs::write(
                dir.path().join("metadata.json"),
                serde_json::to_string(&obj).unwrap(),
            )
            .unwrap();
            assert_eq!(
                ModelType::from_index_metadata(dir.path()),
                Some(model),
                "reader must invert the writer for '{:?}'",
                model
            );
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("metadata.json"),
            r#"{"model_short_name":"not-a-real-model"}"#,
        )
        .unwrap();
        assert_eq!(ModelType::from_index_metadata(dir.path()), None);

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("metadata.json"), "{}").unwrap();
        assert_eq!(ModelType::from_index_metadata(dir.path()), None);

        assert_eq!(
            ModelType::from_index_metadata(std::path::Path::new("/nonexistent-db-dir")),
            None
        );
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
