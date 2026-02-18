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
        let map_size_mb = std::env::var("CODESEARCH_LMDB_MAP_SIZE_MB")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(crate::constants::DEFAULT_LMDB_MAP_SIZE_MB);
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
        let map_size_mb = std::env::var("CODESEARCH_LMDB_MAP_SIZE_MB")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(crate::constants::DEFAULT_LMDB_MAP_SIZE_MB);
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

    /// Resize the LMDB environment to a new map size
    /// This requires closing and reopening the environment
    fn resize_environment(&mut self, new_size_mb: usize) -> Result<()> {
        if new_size_mb > MAX_LMDB_MAP_SIZE_MB {
            return Err(anyhow::anyhow!(
                "Requested map size {}MB exceeds MAX_LMDB_MAP_SIZE_MB {}MB",
                new_size_mb,
                MAX_LMDB_MAP_SIZE_MB
            ));
        }

        tracing::warn!("🔧 Resizing LMDB environment to {}MB", new_size_mb);

        // Store the current database path
        let db_path = self.env.path().to_path_buf();

        // Note: We don't need to explicitly drop the env/databases
        // They will be dropped when we replace them below

        // Open a new environment with the new map size
        // Note: This is a simple implementation that just increases the map size
        // In production, you might want to handle this more gracefully
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(new_size_mb * 1024 * 1024)
                .max_dbs(10)
                .open(&db_path)?
        };

        // Reopen databases
        let mut wtxn = env.write_txn()?;
        let vectors: ArroyDatabase<Cosine> = env.create_database(&mut wtxn, Some("vectors"))?;
        let chunks: Database<U32<BigEndian>, SerdeBincode<ChunkMetadata>> =
            env.create_database(&mut wtxn, Some("chunks"))?;

        // Get the next ID
        let next_id = match chunks.last(&wtxn)? {
            Some((max_key, _)) => max_key + 1,
            None => 0,
        };

        wtxn.commit()?;

        // Check if database is already indexed
        let indexed = if next_id > 0 {
            let rtxn = env.read_txn()?;
            Reader::open(&rtxn, 0, vectors).is_ok()
        } else {
            false
        };

        // Replace the old environment with the new one
        self.env = env;
        self.vectors = vectors;
        self.chunks = chunks;
        self.next_id = next_id;
        self.indexed = indexed;

        // Update the map size tracking
        self.map_size_mb = new_size_mb;

        tracing::info!(
            "✅ LMDB environment resized to {}MB (next_id: {}, indexed: {})",
            new_size_mb,
            next_id,
            indexed
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
                        warn!("MDB_MAP_FULL error in build_index(), resizing to {}MB (attempt {}/{})",
                              new_size, attempts, max_attempts);
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

            let result = self.insert_chunks_with_ids_impl(chunks.clone());

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
    fn insert_chunks_with_ids_impl(&mut self, chunks: Vec<EmbeddedChunk>) -> Result<Vec<u32>> {
        if chunks.is_empty() {
            return Ok(vec![]);
        }

        let start_id = self.next_id;
        let mut wtxn = self.env.write_txn()?;
        let writer = Writer::new(self.vectors, 0, self.dimensions);

        for chunk in &chunks {
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

    /// Iterate all chunks in the store via LMDB cursor.
    /// Returns (id, metadata) pairs for every chunk, regardless of ID gaps.
    /// This is the correct way to enumerate chunks after delete+insert cycles.
    pub fn all_chunks(&self) -> Result<Vec<(u32, ChunkMetadata)>> {
        let rtxn = self.env.read_txn()?;
        let mut result = Vec::new();
        for entry in self.chunks.iter(&rtxn)? {
            let (id, metadata) = entry?;
            result.push((id, metadata));
        }
        Ok(result)
    }

    /// Get the database file size in bytes
    #[allow(dead_code)] // Reserved for stats display
    pub fn db_size(&self) -> Result<u64> {
        let info = self.env.info();
        Ok(info.map_size as u64)
    }

    /// Check if the index is built
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
}
