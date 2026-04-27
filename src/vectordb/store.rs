use crate::constants::MAX_LMDB_MAP_SIZE_MB;
use crate::embed::EmbeddedChunk;
use crate::info_print;
use anyhow::{anyhow, Result};
use arroy::distances::Cosine;
use arroy::{Database as ArroyDatabase, ItemId, Reader, Writer};
use heed::byteorder::BigEndian;
use heed::types::*;
use heed::{Database, EnvFlags, EnvOpenOptions};
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use tracing::warn;

/// Read the persisted LMDB map size from metadata.json in the database directory.
/// Returns DEFAULT_LMDB_MAP_SIZE_MB if no persisted value is found.
fn read_persisted_map_size(db_path: &Path) -> usize {
    let metadata_path = db_path.join("metadata.json");
    if let Ok(content) = fs::read_to_string(&metadata_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(mb) = json.get("lmdb_map_size_mb").and_then(|v| v.as_u64()) {
                return mb as usize;
            }
        }
    }
    crate::constants::DEFAULT_LMDB_MAP_SIZE_MB
}

/// Resolve the effective LMDB map size using the max of persisted, env-var, and default.
/// This ensures consistency across multiple repo opens in the same process.
fn resolve_map_size(db_path: &Path) -> usize {
    let persisted = read_persisted_map_size(db_path);
    let from_env = std::env::var("CODESEARCH_LMDB_MAP_SIZE_MB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok());
    from_env
        .unwrap_or(persisted)
        .max(persisted)
        .max(crate::constants::DEFAULT_LMDB_MAP_SIZE_MB)
}

/// Persist the current LMDB map size into metadata.json so that subsequent
/// opens in the same process use the same value (avoids "already opened with
/// different options" errors from LMDB when multiple repos have been resized).
fn persist_map_size(db_path: &Path, map_size_mb: usize) -> Result<()> {
    let metadata_path = db_path.join("metadata.json");
    let mut json: serde_json::Value = if metadata_path.exists() {
        let content = fs::read_to_string(&metadata_path)?;
        serde_json::from_str(&content).unwrap_or(serde_json::Value::Object(Default::default()))
    } else {
        serde_json::Value::Object(Default::default())
    };

    if let Some(obj) = json.as_object_mut() {
        obj.insert(
            "lmdb_map_size_mb".to_string(),
            serde_json::Value::Number(map_size_mb.into()),
        );
    }

    fs::write(&metadata_path, serde_json::to_string_pretty(&json)?)?;
    Ok(())
}

/// Chunk metadata stored in the database
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkMetadata {
    pub content: String,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: String,
    pub signature: Option<String>,
    pub docstring: Option<String>,
    pub context: Option<String>,
    pub hash: String,
    /// Lines of code immediately before this chunk (for context)
    #[serde(default)]
    pub context_prev: Option<String>,
    /// Lines of code immediately after this chunk (for context)
    #[serde(default)]
    pub context_next: Option<String>,
    /// Searchable text combining signature, name, and content for better searchability
    #[serde(default)]
    pub searchable_text: String,
}

impl ChunkMetadata {
    fn from_embedded_chunk(chunk: &EmbeddedChunk) -> Self {
        // Build searchable text from signature, docstring, and content
        let searchable_text = {
            let mut parts = Vec::new();

            // Add signature if available (e.g., "fn handle_file_modified(path: PathBuf)")
            if let Some(sig) = &chunk.chunk.signature {
                parts.push(sig.clone());
            }

            // Add docstring if available
            if let Some(doc) = &chunk.chunk.docstring {
                parts.push(doc.clone());
            }

            // Add kind (e.g., "Function", "Struct", "Impl")
            parts.push(format!("{:?}", chunk.chunk.kind));

            // Add content
            parts.push(chunk.chunk.content.clone());

            parts.join("\n")
        };

        Self {
            content: chunk.chunk.content.clone(),
            path: chunk.chunk.path.clone(),
            start_line: chunk.chunk.start_line,
            end_line: chunk.chunk.end_line,
            kind: format!("{:?}", chunk.chunk.kind),
            signature: chunk.chunk.signature.clone(),
            docstring: chunk.chunk.docstring.clone(),
            context: if chunk.chunk.context.is_empty() {
                None
            } else {
                Some(chunk.chunk.context.join(" > "))
            },
            hash: chunk.chunk.hash.clone(),
            context_prev: chunk.chunk.context_prev.clone(),
            context_next: chunk.chunk.context_next.clone(),
            searchable_text,
        }
    }
}

/// Vector database using arroy + heed (LMDB)
///
/// Single-file database with:
/// - Vector search via arroy (ANN with random projections)
/// - Metadata storage via heed (LMDB)
/// - ACID transactions
/// - Memory-mapped for performance
pub struct VectorStore {
    env: heed::Env,
    vectors: ArroyDatabase<Cosine>,
    chunks: Database<U32<BigEndian>, SerdeBincode<ChunkMetadata>>,
    next_id: u32,
    dimensions: usize,
    indexed: bool,
    pub map_size_mb: usize,
}

/// Lightweight chunk metadata used for file-outline style navigation.
#[derive(Debug, Clone)]
pub struct ChunkMeta {
    pub id: u32,
    pub kind: String,
    pub signature: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
}

impl VectorStore {
    /// Create or open a vector store
    ///
    /// # Arguments
    /// * `db_path` - Path to the database directory (e.g., ".codesearch.db")
    /// * `dimensions` - Dimensionality of embeddings (e.g., 384, 768)
    pub fn new(db_path: &Path, dimensions: usize) -> Result<Self> {
        info_print!("📦 Opening vector database at: {}", db_path.display());

        // Create database directory (LMDB expects a directory, not a file)
        std::fs::create_dir_all(db_path)?;

        // Clean up any stale .del files from previous crashed runs
        cleanup_stale_del_files(db_path)?;

        // Open LMDB environment
        // Read persisted map_size from metadata.json if available, so that
        // multiple repos in the same process use a consistent map size after
        // one repo has been resized.  Use the max of persisted, env-var, and
        // default to never shrink below what was previously allocated.
        let map_size_mb = resolve_map_size(db_path);
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(map_size_mb * 1024 * 1024)
                .max_dbs(10)
                .open(db_path)?
        };

        // Open or create databases
        let mut wtxn = env.write_txn()?;

        let vectors: ArroyDatabase<Cosine> = env.create_database(&mut wtxn, Some("vectors"))?;
        let chunks: Database<U32<BigEndian>, SerdeBincode<ChunkMetadata>> =
            env.create_database(&mut wtxn, Some("chunks"))?;

        // Get the next ID from the maximum existing key + 1
        // Using len() is wrong after delete+insert cycles: deleted IDs create gaps
        // so len() < max_key + 1, causing ID collisions on re-open
        let next_id = match chunks.last(&wtxn)? {
            Some((max_key, _)) => max_key + 1,
            None => 0,
        };

        wtxn.commit()?;

        // Check if database is already indexed by trying to open a reader
        let indexed = if next_id > 0 {
            let rtxn = env.read_txn()?;
            match Reader::open(&rtxn, 0, vectors) {
                Ok(_) => {
                    tracing::debug!("Index detected: Reader::open succeeded");
                    true
                }
                Err(e) => {
                    tracing::debug!("Index not detected: Reader::open failed: {:?}", e);
                    false
                }
            }
        } else {
            false
        };

        info_print!("✅ Database opened (next_id: {})", next_id);

        Ok(Self {
            env,
            vectors,
            chunks,
            next_id,
            dimensions,
            indexed,
            map_size_mb,
        })
    }

    /// Open a vector store in read-only mode (for searches while another process writes)
    ///
    /// # Arguments
    /// * `db_path` - Path to the database directory (e.g., ".codesearch.db")
    /// * `dimensions` - Dimensionality of embeddings (e.g., 384, 768)
    pub fn open_readonly(db_path: &Path, dimensions: usize) -> Result<Self> {
        tracing::debug!(
            "📦 Opening vector database (read-only) at: {}",
            db_path.display()
        );

        if !db_path.exists() {
            return Err(anyhow::anyhow!(
                "Database does not exist at: {}",
                db_path.display()
            ));
        }

        // Open LMDB environment in read-only mode
        // Use same map-size resolution as new() for consistency
        let map_size_mb = resolve_map_size(db_path);
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(map_size_mb * 1024 * 1024)
                .max_dbs(10)
                .flags(EnvFlags::READ_ONLY)
                .open(db_path)?
        };

        // Open databases (read-only, no create)
        let rtxn = env.read_txn()?;

        let vectors: ArroyDatabase<Cosine> = env
            .open_database(&rtxn, Some("vectors"))?
            .ok_or_else(|| anyhow::anyhow!("vectors database not found"))?;
        let chunks: Database<U32<BigEndian>, SerdeBincode<ChunkMetadata>> = env
            .open_database(&rtxn, Some("chunks"))?
            .ok_or_else(|| anyhow::anyhow!("chunks database not found"))?;

        // Get the next ID from the maximum existing key + 1
        // Using len() is wrong after delete+insert cycles: deleted IDs create gaps
        let next_id = match chunks.last(&rtxn)? {
            Some((max_key, _)) => max_key + 1,
            None => 0,
        };

        // Check if database is already indexed
        let indexed = if next_id > 0 {
            Reader::open(&rtxn, 0, vectors).is_ok()
        } else {
            false
        };

        drop(rtxn);

        tracing::debug!(
            "✅ Database opened read-only (next_id: {}, indexed: {})",
            next_id,
            indexed
        );

        Ok(Self {
            env,
            vectors,
            chunks,
            next_id,
            dimensions,
            indexed,
            map_size_mb,
        })
    }

    /// Check if an error is an MDB_MAP_FULL error
    /// MDB_MAP_FULL error code is -28
    fn is_map_full_error(&self, error: &dyn std::error::Error) -> bool {
        // MDB_MAP_FULL error code is -28 (0xFFFFFFE4)
        error.to_string().contains("MDB_MAP_FULL") || error.to_string().contains("map full")
    }

    /// Resize the LMDB environment to a new map size.
    ///
    /// Uses `heed::Env::resize()` which calls `mdb_env_set_mapsize()` under the
    /// hood.  This resizes in-place without closing and reopening the
    /// environment, which avoids the "an environment is already opened with
    /// different options" error when a live serve process needs to grow the map.
    fn resize_environment(&mut self, new_size_mb: usize) -> Result<()> {
        if new_size_mb > MAX_LMDB_MAP_SIZE_MB {
            return Err(anyhow::anyhow!(
                "Requested map size {}MB exceeds MAX_LMDB_MAP_SIZE_MB {}MB",
                new_size_mb,
                MAX_LMDB_MAP_SIZE_MB
            ));
        }

        let new_size_bytes = new_size_mb * 1024 * 1024;

        tracing::warn!("🔧 Resizing LMDB environment to {}MB", new_size_mb);

        // SAFETY: mdb_env_set_mapsize() is safe to call when no transaction is
        // active.  Our caller holds &mut self and just got MDB_MAP_FULL on an
        // insert — any write transaction that triggered the error has already
        // been dropped by the caller before invoking the retry logic.
        unsafe {
            self.env.resize(new_size_bytes)?;
        }

        self.map_size_mb = new_size_mb;

        // Persist the new map size so subsequent opens in the same process
        // use the same value (avoids "already opened with different options").
        if let Err(e) = persist_map_size(self.env.path(), new_size_mb) {
            tracing::warn!("Failed to persist LMDB map size: {}", e);
        }

        tracing::info!(
            "✅ LMDB environment resized to {}MB (in-place, no reopen)",
            new_size_mb
        );

        Ok(())
    }

    /// Insert embedded chunks into the database
    ///
    /// Returns the number of chunks inserted
    #[allow(dead_code)] // Reserved for batch insert operations
    pub fn insert_chunks(&mut self, chunks: Vec<EmbeddedChunk>) -> Result<usize> {
        if chunks.is_empty() {
            return Ok(0);
        }

        eprintln!("📊 Inserting {} chunks...", chunks.len());

        let mut wtxn = self.env.write_txn()?;
        let writer = Writer::new(self.vectors, 0, self.dimensions);

        for chunk in &chunks {
            let id = self.next_id;

            // Check embedding dimensions
            if chunk.embedding.len() != self.dimensions {
                return Err(anyhow!(
                    "Embedding dimension mismatch: expected {}, got {}",
                    self.dimensions,
                    chunk.embedding.len()
                ));
            }

            // Add vector to arroy
            writer.add_item(&mut wtxn, id, &chunk.embedding)?;

            // Store metadata
            let metadata = ChunkMetadata::from_embedded_chunk(chunk);
            self.chunks.put(&mut wtxn, &id, &metadata)?;

            self.next_id += 1;
        }

        wtxn.commit()?;

        // Mark as not indexed (need to rebuild index after inserts)
        self.indexed = false;

        eprintln!(
            "✅ Inserted {} chunks (IDs: {}-{})",
            chunks.len(),
            self.next_id - chunks.len() as u32,
            self.next_id - 1
        );

        Ok(chunks.len())
    }

    /// Build the vector index with auto-resize on MDB_MAP_FULL
    ///
    /// Must be called after inserting chunks and before searching.
    /// This is the heaviest LMDB write operation (arroy tree build),
    /// so it includes retry logic for MDB_MAP_FULL errors.
    pub fn build_index(&mut self) -> Result<()> {
        let mut attempts = 0;
        let max_attempts = 3;

        loop {
            attempts += 1;

            let result = self.build_index_impl();

            match &result {
                Ok(_) => return result,
                Err(e) => {
                    if attempts >= max_attempts || !self.is_map_full_error(e.as_ref()) {
                        return result;
                    }

                    let new_size = self.map_size_mb * 2;
                    if new_size <= MAX_LMDB_MAP_SIZE_MB {
                        warn!(
                            "MDB_MAP_FULL error in build_index(), resizing to {}MB (attempt {}/{})",
                            new_size, attempts, max_attempts
                        );
                        self.resize_environment(new_size)?;
                    } else {
                        warn!(
                            "MDB_MAP_FULL error in build_index(), already at max size {}MB",
                            self.map_size_mb
                        );
                        return result;
                    }
                }
            }
        }
    }

    /// Implementation of build_index without retry logic
    fn build_index_impl(&mut self) -> Result<()> {
        let mut wtxn = self.env.write_txn()?;
        let writer = Writer::new(self.vectors, 0, self.dimensions);
        let mut rng = StdRng::seed_from_u64(rand::random());
        writer.builder(&mut rng).build(&mut wtxn)?;
        wtxn.commit()?;
        self.indexed = true;
        Ok(())
    }
    pub fn search(&self, query_embedding: &[f32], limit: usize) -> Result<Vec<SearchResult>> {
        if query_embedding.len() != self.dimensions {
            return Err(anyhow!(
                "Query embedding dimension mismatch: expected {}, got {}",
                self.dimensions,
                query_embedding.len()
            ));
        }

        if !self.indexed {
            return Err(anyhow!(
                "Index not built. Call build_index() after inserting chunks."
            ));
        }

        let rtxn = self.env.read_txn()?;
        let reader = Reader::open(&rtxn, 0, self.vectors)?;

        // Perform ANN search with quality boost
        let mut query = reader.nns(limit);

        // Improve search quality by exploring more candidates
        if let Some(n_trees) = NonZeroUsize::new(reader.n_trees()) {
            if let Some(search_k) = NonZeroUsize::new(limit * n_trees.get() * 15) {
                query.search_k(search_k);
            }
        }

        let results = query.by_vector(&rtxn, query_embedding)?;

        // Fetch metadata for each result
        let mut search_results = Vec::new();

        for (id, distance) in results {
            if let Some(metadata) = self.chunks.get(&rtxn, &id)? {
                search_results.push(SearchResult {
                    id,
                    content: metadata.content,
                    path: metadata.path,
                    start_line: metadata.start_line,
                    end_line: metadata.end_line,
                    kind: metadata.kind,
                    signature: metadata.signature,
                    docstring: metadata.docstring,
                    context: metadata.context,
                    hash: metadata.hash,
                    distance,
                    score: 1.0 - distance, // Convert distance to similarity score
                    context_prev: metadata.context_prev,
                    context_next: metadata.context_next,
                });
            }
        }

        Ok(search_results)
    }

    /// Returns real LMDB page-level stats for accurate bloat detection.
    ///
    /// Uses `env.non_free_pages_size()` (bytes in use) vs `env.real_disk_size()`
    /// (actual file size on disk) to compute the bloat ratio. No guessing needed.
    pub fn lmdb_page_stats(&self) -> Result<LmdbPageStats> {
        let used_bytes = self.env.non_free_pages_size()?;
        let disk_size = self.env.real_disk_size()?;
        Ok(LmdbPageStats {
            used_bytes,
            disk_size,
        })
    }

    pub fn stats(&self) -> Result<StoreStats> {
        let rtxn = self.env.read_txn()?;

        let total_chunks = self.chunks.len(&rtxn)?;

        // Count unique files
        let mut unique_files = std::collections::HashSet::new();
        for result in self.chunks.iter(&rtxn)? {
            let (_, metadata) = result?;
            unique_files.insert(metadata.path.clone());
        }

        // Get max chunk ID from the last key in LMDB (sorted)
        let max_chunk_id = self.chunks.last(&rtxn)?.map(|(k, _)| k).unwrap_or(0);

        Ok(StoreStats {
            total_chunks: total_chunks as usize,
            total_files: unique_files.len(),
            indexed: self.indexed,
            dimensions: self.dimensions,
            max_chunk_id,
        })
    }

    /// Get all chunks grouped by file path
    ///
    /// Returns a map of file_path -> Vec<chunk_id> for every chunk in the store.
    /// Used by branch refresh to find orphaned chunks not tracked by FileMetaStore.
    /// Iterate all chunks in the store, returning (chunk_id, metadata) pairs.
    /// Used by the scan-path fallback for tokenless regex queries where BM25
    /// cannot produce useful candidates.
    pub fn iter_all_chunks(&self) -> Result<Vec<(u32, ChunkMetadata)>> {
        let rtxn = self.env.read_txn()?;
        let mut all = Vec::new();
        for result in self.chunks.iter(&rtxn)? {
            all.push(result?);
        }
        Ok(all)
    }

    pub fn get_chunks_by_file(&self) -> Result<std::collections::HashMap<String, Vec<u32>>> {
        let rtxn = self.env.read_txn()?;
        let mut file_chunks: std::collections::HashMap<String, Vec<u32>> =
            std::collections::HashMap::new();

        for result in self.chunks.iter(&rtxn)? {
            let (chunk_id, metadata) = result?;
            file_chunks
                .entry(metadata.path.clone())
                .or_default()
                .push(chunk_id);
        }

        Ok(file_chunks)
    }

    /// Delete chunks by their IDs
    ///
    /// Returns the number of chunks deleted
    pub fn delete_chunks(&mut self, chunk_ids: &[u32]) -> Result<usize> {
        // Auto-resize retry logic for MDB_MAP_FULL errors
        let mut attempts = 0;
        let max_attempts = 3;

        loop {
            attempts += 1;

            let result = self.delete_chunks_impl(chunk_ids);

            match &result {
                Ok(_) => return result,
                Err(e) => {
                    if attempts >= max_attempts || !self.is_map_full_error(e.as_ref()) {
                        return result;
                    }

                    // Double map size and retry
                    let new_size = self.map_size_mb * 2;
                    if new_size <= MAX_LMDB_MAP_SIZE_MB {
                        warn!("MDB_MAP_FULL error in delete_chunks(), resizing to {}MB (attempt {}/{})",
                              new_size, attempts, max_attempts);
                        self.resize_environment(new_size)?;
                    } else {
                        warn!(
                            "MDB_MAP_FULL error, already at max size {}MB",
                            self.map_size_mb
                        );
                        return result;
                    }
                }
            }
        }
    }

    /// Implementation of delete_chunks without retry logic
    fn delete_chunks_impl(&mut self, chunk_ids: &[u32]) -> Result<usize> {
        if chunk_ids.is_empty() {
            return Ok(0);
        }

        let mut wtxn = self.env.write_txn()?;
        let writer = Writer::new(self.vectors, 0, self.dimensions);

        let mut deleted = 0;
        for &id in chunk_ids {
            // Delete from vector database
            if writer.del_item(&mut wtxn, id).is_ok() {
                deleted += 1;
            }
            // Delete from metadata
            self.chunks.delete(&mut wtxn, &id)?;
        }

        wtxn.commit()?;

        // Mark as needing re-index
        if deleted > 0 {
            self.indexed = false;
        }

        Ok(deleted)
    }

    /// Delete all chunks from a specific file
    ///
    /// Returns the IDs of deleted chunks
    /// Insert chunks and return their assigned IDs
    ///
    /// Useful for tracking which chunks belong to which file
    pub fn insert_chunks_with_ids(&mut self, chunks: Vec<EmbeddedChunk>) -> Result<Vec<u32>> {
        // Auto-resize retry logic for MDB_MAP_FULL errors
        let mut attempts = 0;
        let max_attempts = 3;

        loop {
            attempts += 1;

            let result = self.insert_chunks_with_ids_impl(&chunks);

            match &result {
                Ok(_) => return result,
                Err(e) => {
                    if attempts >= max_attempts || !self.is_map_full_error(e.as_ref()) {
                        return result;
                    }

                    // Double map size and retry
                    let new_size = self.map_size_mb * 2;
                    if new_size <= MAX_LMDB_MAP_SIZE_MB {
                        warn!("MDB_MAP_FULL error in insert_chunks_with_ids(), resizing to {}MB (attempt {}/{})",
                              new_size, attempts, max_attempts);
                        self.resize_environment(new_size)?;
                    } else {
                        warn!(
                            "MDB_MAP_FULL error, already at max size {}MB",
                            self.map_size_mb
                        );
                        return result;
                    }
                }
            }
        }
    }

    /// Implementation of insert_chunks_with_ids without retry logic
    fn insert_chunks_with_ids_impl(&mut self, chunks: &[EmbeddedChunk]) -> Result<Vec<u32>> {
        if chunks.is_empty() {
            return Ok(vec![]);
        }

        let start_id = self.next_id;
        let mut wtxn = self.env.write_txn()?;
        let writer = Writer::new(self.vectors, 0, self.dimensions);

        for chunk in chunks {
            let id = self.next_id;

            if chunk.embedding.len() != self.dimensions {
                return Err(anyhow!(
                    "Embedding dimension mismatch: expected {}, got {}",
                    self.dimensions,
                    chunk.embedding.len()
                ));
            }

            writer.add_item(&mut wtxn, id, &chunk.embedding)?;
            let metadata = ChunkMetadata::from_embedded_chunk(chunk);
            self.chunks.put(&mut wtxn, &id, &metadata)?;

            self.next_id += 1;
        }

        wtxn.commit()?;
        self.indexed = false;

        let ids: Vec<u32> = (start_id..self.next_id).collect();
        Ok(ids)
    }

    /// Clear all data from the database
    #[allow(dead_code)] // Reserved for database reset operations
    pub fn clear(&mut self) -> Result<()> {
        eprintln!("🗑️  Clearing database...");

        let mut wtxn = self.env.write_txn()?;

        // Clear both databases
        self.chunks.clear(&mut wtxn)?;
        self.vectors.clear(&mut wtxn)?;

        wtxn.commit()?;

        self.next_id = 0;
        self.indexed = false;

        eprintln!("✅ Database cleared");
        Ok(())
    }

    /// Get a chunk by ID
    pub fn get_chunk(&self, id: u32) -> Result<Option<ChunkMetadata>> {
        let rtxn = self.env.read_txn()?;
        Ok(self.chunks.get(&rtxn, &id)?)
    }

    /// Get lightweight metadata for all chunks in a file.
    ///
    /// Path matching uses normalized path strings to avoid Windows path-format
    /// mismatches (`\\?\` prefix, slash direction differences).
    ///
    /// Deduplicates historical snapshot duplicates, keeping the highest
    /// chunk_id (most recent) per unique logical chunk:
    /// - When `signature` is present: dedup by `(kind, signature)` — the
    ///   signature identifies the logical entity regardless of line drift.
    /// - When `signature` is `None`: dedup by `(kind, start_line, end_line)`
    ///   — positional identity is the best we have for unnamed blocks.
    ///
    /// This is a defensive measure — the indexer should delete stale chunks
    /// before re-inserting, but incremental indexing bugs can leave orphans.
    ///
    /// TODO: For large indexes (100k+ chunks), the linear scan is O(n).
    /// Consider adding a path-based secondary index for production use at scale.
    pub fn chunks_for_file(&self, path: &str) -> Result<Vec<ChunkMeta>> {
        use std::collections::HashMap;

        let rtxn = self.env.read_txn()?;
        let needle = crate::cache::normalize_path_str(path);
        // Two maps: one keyed by signature (for named chunks), one by line
        // range (for unnamed blocks).  This avoids cross-contamination where
        // a null-sig entry could collide with a named entry.
        let mut by_sig: HashMap<(String, String), ChunkMeta> = HashMap::new();
        let mut by_range: HashMap<(String, usize, usize), ChunkMeta> = HashMap::new();

        for result in self.chunks.iter(&rtxn)? {
            let (id, meta) = result?;
            let chunk_path = crate::cache::normalize_path_str(&meta.path);
            if chunk_path == needle {
                let meta = ChunkMeta {
                    id,
                    kind: meta.kind,
                    signature: meta.signature,
                    start_line: meta.start_line,
                    end_line: meta.end_line,
                };
                if let Some(ref sig) = meta.signature {
                    let key = (meta.kind.clone(), sig.clone());
                    by_sig
                        .entry(key)
                        .and_modify(|existing| {
                            if meta.id > existing.id {
                                existing.id = meta.id;
                            }
                        })
                        .or_insert(meta);
                } else {
                    let key = (meta.kind.clone(), meta.start_line, meta.end_line);
                    by_range
                        .entry(key)
                        .and_modify(|existing| {
                            if meta.id > existing.id {
                                existing.id = meta.id;
                            }
                        })
                        .or_insert(meta);
                }
            }
        }

        let mut out: Vec<ChunkMeta> = by_sig
            .into_values()
            .chain(by_range.into_values())
            .collect();
        out.sort_by_key(|c| c.start_line);
        Ok(out)
    }

    /// Get the stored embedding vector for a chunk id.
    pub fn get_embedding(&self, id: u32) -> Result<Option<Vec<f32>>> {
        let rtxn = self.env.read_txn()?;
        let reader = Reader::open(&rtxn, 0, self.vectors)?;
        let vector = reader.item_vector(&rtxn, id)?;
        Ok(vector.map(|v| v.to_vec()))
    }

    /// Get a chunk as SearchResult (for hybrid search)
    pub fn get_chunk_as_result(&self, id: u32) -> Result<Option<SearchResult>> {
        let rtxn = self.env.read_txn()?;
        if let Some(meta) = self.chunks.get(&rtxn, &id)? {
            Ok(Some(SearchResult {
                id,
                content: meta.content,
                path: meta.path,
                start_line: meta.start_line,
                end_line: meta.end_line,
                kind: meta.kind,
                signature: meta.signature,
                docstring: meta.docstring,
                context: meta.context,
                hash: meta.hash,
                distance: 0.0,
                score: 0.0, // Will be set by caller
                context_prev: meta.context_prev,
                context_next: meta.context_next,
            }))
        } else {
            Ok(None)
        }
    }

    /// Get the database file size in bytes
    #[allow(dead_code)] // Reserved for stats display
    pub fn db_size(&self) -> Result<u64> {
        let info = self.env.info();
        Ok(info.map_size as u64)
    }

    /// Check if the index is built
    #[allow(dead_code)]
    pub fn is_indexed(&self) -> bool {
        self.indexed
    }
}

/// Search result with metadata
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields docstring/hash used for completeness
pub struct SearchResult {
    pub id: ItemId,
    pub content: String,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: String,
    pub signature: Option<String>,
    pub docstring: Option<String>,
    pub context: Option<String>,
    pub hash: String,
    pub distance: f32,
    pub score: f32, // 1.0 - distance (higher is better)
    /// Lines of code immediately before this chunk (for context)
    pub context_prev: Option<String>,
    /// Lines of code immediately after this chunk (for context)
    pub context_next: Option<String>,
}

/// Statistics about the vector store
#[derive(Debug, Clone)]
/// Real LMDB page-level statistics for accurate bloat detection.
pub struct LmdbPageStats {
    /// Bytes occupied by non-free (live) pages — from `env.non_free_pages_size()`.
    pub used_bytes: u64,
    /// Actual file size on disk — from `env.real_disk_size()`.
    pub disk_size: u64,
}

pub struct StoreStats {
    pub total_chunks: usize,
    pub total_files: usize,
    pub indexed: bool,
    pub dimensions: usize,
    /// The highest chunk ID in the store (or 0 if empty).
    /// NOTE: This may be > total_chunks when chunks have been deleted.
    pub max_chunk_id: u32,
}

/// Clean up stale .del files from previous crashed runs
///
/// LMDB creates .del files when deleting items, but if the process crashes
/// or is interrupted, these files can be left behind and cause errors on
/// the next run. This function removes any .del files before opening the DB.
fn cleanup_stale_del_files(db_path: &Path) -> Result<()> {
    if !db_path.exists() {
        return Ok(());
    }

    let entries = fs::read_dir(db_path)?;
    let mut cleaned = 0;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Check if file ends with .del
        if path.extension().and_then(|s| s.to_str()) == Some("del") {
            // Remove the .del file
            fs::remove_file(&path)?;
            cleaned += 1;
        }
    }

    if cleaned > 0 {
        tracing::debug!("Cleaned up {} stale .del files", cleaned);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::{Chunk, ChunkKind};
    use crate::embed::EmbeddedChunk;
    use tempfile::tempdir;

    #[test]
    fn test_vector_store_creation() {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let store = VectorStore::new(&db_path, 384);
        assert!(store.is_ok());

        let store = store.unwrap();
        assert_eq!(store.dimensions, 384);
        assert!(!store.is_indexed());
    }

    #[test]
    fn test_insert_and_search() {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let mut store = VectorStore::new(&db_path, 4).unwrap();

        // Create test chunks with different embeddings
        let chunks = vec![
            EmbeddedChunk::new(
                Chunk::new(
                    "fn authenticate() {}".to_string(),
                    0,
                    1,
                    ChunkKind::Function,
                    "auth.rs".to_string(),
                ),
                vec![1.0, 0.0, 0.0, 0.0], // Close to query
            ),
            EmbeddedChunk::new(
                Chunk::new(
                    "fn calculate() {}".to_string(),
                    2,
                    3,
                    ChunkKind::Function,
                    "math.rs".to_string(),
                ),
                vec![0.0, 1.0, 0.0, 0.0], // Far from query
            ),
        ];

        // Insert
        let count = store.insert_chunks(chunks).unwrap();
        assert_eq!(count, 2);

        // Build index
        store.build_index().unwrap();
        assert!(store.is_indexed());

        // Search with query similar to first chunk
        let query = vec![0.9, 0.1, 0.0, 0.0];
        let results = store.search(&query, 2).unwrap();

        assert_eq!(results.len(), 2);
        // First result should be the authenticate function (closer to query)
        assert!(results[0].content.contains("authenticate"));
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn test_stats() {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let mut store = VectorStore::new(&db_path, 4).unwrap();

        let chunks = vec![
            EmbeddedChunk::new(
                Chunk::new(
                    "fn test1() {}".to_string(),
                    0,
                    1,
                    ChunkKind::Function,
                    "file1.rs".to_string(),
                ),
                vec![1.0, 0.0, 0.0, 0.0],
            ),
            EmbeddedChunk::new(
                Chunk::new(
                    "fn test2() {}".to_string(),
                    0,
                    1,
                    ChunkKind::Function,
                    "file2.rs".to_string(),
                ),
                vec![0.0, 1.0, 0.0, 0.0],
            ),
        ];

        store.insert_chunks(chunks).unwrap();
        store.build_index().unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.total_chunks, 2);
        assert_eq!(stats.total_files, 2);
        assert!(stats.indexed);
        assert_eq!(stats.dimensions, 4);
    }

    #[test]
    fn test_clear() {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let mut store = VectorStore::new(&db_path, 4).unwrap();

        let chunks = vec![EmbeddedChunk::new(
            Chunk::new(
                "fn test() {}".to_string(),
                0,
                1,
                ChunkKind::Function,
                "test.rs".to_string(),
            ),
            vec![1.0, 0.0, 0.0, 0.0],
        )];

        store.insert_chunks(chunks).unwrap();
        store.build_index().unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.total_chunks, 1);

        store.clear().unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.total_chunks, 0);
        assert!(!stats.indexed);
    }

    #[test]
    fn test_get_chunk() {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let mut store = VectorStore::new(&db_path, 4).unwrap();

        let chunks = vec![EmbeddedChunk::new(
            Chunk::new(
                "fn test() {}".to_string(),
                0,
                1,
                ChunkKind::Function,
                "test.rs".to_string(),
            ),
            vec![1.0, 0.0, 0.0, 0.0],
        )];

        store.insert_chunks(chunks).unwrap();

        let metadata = store.get_chunk(0).unwrap();
        assert!(metadata.is_some());

        let metadata = metadata.unwrap();
        assert_eq!(metadata.content, "fn test() {}");
        assert_eq!(metadata.path, "test.rs");
    }

    #[test]
    fn test_persistence() {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        // First session: insert and close
        {
            let mut store = VectorStore::new(&db_path, 4).unwrap();

            let chunks = vec![EmbeddedChunk::new(
                Chunk::new(
                    "fn test() {}".to_string(),
                    0,
                    1,
                    ChunkKind::Function,
                    "test.rs".to_string(),
                ),
                vec![1.0, 0.0, 0.0, 0.0],
            )];

            store.insert_chunks(chunks).unwrap();
            store.build_index().unwrap();
        }

        // Second session: reopen and verify
        {
            let store = VectorStore::new(&db_path, 4).unwrap();

            let stats = store.stats().unwrap();
            assert_eq!(stats.total_chunks, 1);

            let metadata = store.get_chunk(0).unwrap();
            assert!(metadata.is_some());
        }
    }

    #[test]
    fn test_chunks_for_file_returns_sorted_candidates_by_filter() {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let mut store = VectorStore::new(&db_path, 4).unwrap();
        let chunks = vec![
            EmbeddedChunk::new(
                Chunk::new(
                    "fn a() {}".to_string(),
                    0,
                    1,
                    ChunkKind::Function,
                    "src/a.rs".to_string(),
                ),
                vec![1.0, 0.0, 0.0, 0.0],
            ),
            EmbeddedChunk::new(
                Chunk::new(
                    "fn b() {}".to_string(),
                    2,
                    3,
                    ChunkKind::Function,
                    "src/a.rs".to_string(),
                ),
                vec![0.9, 0.1, 0.0, 0.0],
            ),
            EmbeddedChunk::new(
                Chunk::new(
                    "fn c() {}".to_string(),
                    0,
                    1,
                    ChunkKind::Function,
                    "src/other.rs".to_string(),
                ),
                vec![0.0, 1.0, 0.0, 0.0],
            ),
        ];

        store.insert_chunks(chunks).unwrap();

        let metas = store.chunks_for_file("src/a.rs").unwrap();
        assert_eq!(metas.len(), 2);
        assert!(metas.iter().all(|m| m.kind == "Function"));
    }

    #[test]
    fn test_get_embedding_returns_vector_after_build() {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let mut store = VectorStore::new(&db_path, 4).unwrap();
        let chunks = vec![EmbeddedChunk::new(
            Chunk::new(
                "fn emb() {}".to_string(),
                0,
                1,
                ChunkKind::Function,
                "src/emb.rs".to_string(),
            ),
            vec![1.0, 0.0, 0.0, 0.0],
        )];

        store.insert_chunks(chunks).unwrap();
        store.build_index().unwrap();

        let emb = store.get_embedding(0).unwrap();
        assert!(emb.is_some());
        assert_eq!(emb.unwrap().len(), 4);
    }
}
