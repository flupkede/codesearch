//! Tantivy-based full-text search store
//!
//! Provides BM25 full-text search for hybrid search with RRF fusion.
//!
//! # Architecture Note
//! Always use `FtsStore::new()` which opens in R/W mode. This ensures only one
//! connection type exists, avoiding Windows file locking issues between readers
//! and writers. The writer is lazy-initialized on first write operation.

use anyhow::{anyhow, Result};
use std::path::Path;
use tantivy::{
    collector::TopDocs,
    directory::MmapDirectory,
    merge_policy::NoMergePolicy,
    query::QueryParser,
    schema::{Field, NumericOptions, Schema, Value, STORED, STRING, TEXT},
    Index, IndexReader, IndexSettings, IndexWriter, TantivyDocument, Term,
};

/// Result from FTS search
#[derive(Debug, Clone)]
pub struct FtsResult {
    /// Chunk ID that matches
    pub chunk_id: u32,
    /// BM25 score from Tantivy
    pub score: f32,
}

/// Full-text search store using Tantivy
///
/// Single connection type that supports both read and write operations.
/// Writer is lazy-initialized on first write to avoid unnecessary locks.
pub struct FtsStore {
    index: Index,
    reader: IndexReader,
    writer: Option<IndexWriter>,
    #[allow(dead_code)]
    schema: Schema,
    // Field handles
    chunk_id_field: Field,
    content_field: Field,
    path_field: Field,
    signature_field: Field,
    kind_field: Field,
}

impl FtsStore {
    /// Create or open an FTS index at the given path.
    ///
    /// Opens in a mode that supports both reading and writing.
    /// Writer is lazy-initialized on first write operation.
    pub fn new(db_path: &Path) -> Result<Self> {
        let fts_path = db_path.join("fts");
        std::fs::create_dir_all(&fts_path)?;

        // Build schema
        let mut schema_builder = Schema::builder();

        // Chunk ID - stored and indexed for retrieval and deletion
        let chunk_id_field = schema_builder.add_u64_field(
            "chunk_id",
            NumericOptions::default().set_indexed().set_stored(),
        );

        // Content - full text indexed for BM25 search
        let content_field = schema_builder.add_text_field("content", TEXT);

        // Path - stored and string indexed for filtering
        let path_field = schema_builder.add_text_field("path", STRING | STORED);

        // Signature - indexed for function/method name search
        let signature_field = schema_builder.add_text_field("signature", TEXT);

        // Kind - stored for filtering (function, class, etc)
        let kind_field = schema_builder.add_text_field("kind", STRING | STORED);

        let schema = schema_builder.build();

        // Open or create index with retry logic for Windows file locking
        let index = Self::open_or_create_index_with_retry(&fts_path, &schema)?;

        // Create reader for searching
        let reader = index.reader()?;

        Ok(Self {
            index,
            reader,
            writer: None, // Lazy-initialized on first write
            schema,
            chunk_id_field,
            content_field,
            path_field,
            signature_field,
            kind_field,
        })
    }

    /// Create or open an FTS index with writer ready for indexing.
    ///
    /// Use this when you know you'll be writing immediately (e.g., during indexing).
    /// For search-only or mixed workloads, use `new()` instead.
    pub fn new_with_writer(db_path: &Path) -> Result<Self> {
        let mut store = Self::new(db_path)?;
        store.ensure_writer()?;
        Ok(store)
    }

    /// Open or create index with retry logic for Windows file locking issues
    fn open_or_create_index_with_retry(fts_path: &Path, schema: &Schema) -> Result<Index> {
        let max_retries = 3;
        let mut last_error: Option<String> = None;

        for attempt in 0..max_retries {
            if attempt > 0 {
                // Wait before retry (exponential backoff)
                std::thread::sleep(std::time::Duration::from_millis(100 * (1 << attempt)));
            }

            let result: Result<Index, _> = if fts_path.join("meta.json").exists() {
                Index::open_in_dir(fts_path).map_err(|e| e.to_string())
            } else {
                MmapDirectory::open(fts_path)
                    .map_err(|e| e.to_string())
                    .and_then(|dir| {
                        Index::create(dir, schema.clone(), IndexSettings::default())
                            .map_err(|e| e.to_string())
                    })
            };

            match result {
                Ok(index) => return Ok(index),
                Err(e) => {
                    last_error = Some(e);
                    // On Windows, try to clear lock files if permission denied
                    if attempt < max_retries - 1 {
                        Self::try_clear_lock_files(fts_path);
                    }
                }
            }
        }

        Err(anyhow!(
            "Failed to open FTS index after {} retries: {}",
            max_retries,
            last_error.unwrap_or_default()
        ))
    }

    /// Create writer with retry logic for Windows file locking issues
    fn create_writer_with_retry(index: &Index) -> Result<IndexWriter> {
        let max_retries = 3;
        let mut last_error: Option<String> = None;

        for attempt in 0..max_retries {
            if attempt > 0 {
                // Wait before retry (exponential backoff)
                std::thread::sleep(std::time::Duration::from_millis(100 * (1 << attempt)));
            }

            // 50MB writer heap (tantivy default).
            //
            // CRITICAL: Set NoMergePolicy to prevent tantivy from spawning background
            // merge threads. On Windows, these threads encounter I/O errors (antivirus
            // interference, file locking on mmap'd segment files) which panic the merge
            // thread and kill the IndexWriter — causing the intermittent
            // "An index writer was killed" error (~1/5 indexing runs).
            //
            // With NoMergePolicy, all segment management is explicit: we accumulate
            // segments during indexing and they're consolidated at commit points.
            // This trades slightly more segments for 100% reliability.
            match index.writer(50_000_000) {
                Ok(writer) => {
                    writer.set_merge_policy(Box::new(NoMergePolicy));
                    return Ok(writer);
                }
                Err(e) => {
                    last_error = Some(e.to_string());
                }
            }
        }

        Err(anyhow!(
            "Failed to create FTS writer after {} retries: {}",
            max_retries,
            last_error.unwrap_or_default()
        ))
    }

    /// Try to clear stale lock files on Windows
    fn try_clear_lock_files(fts_path: &Path) {
        // Try to remove stale lock files
        let lock_files = [".tantivy-writer.lock", ".tantivy-meta.lock"];
        for lock_file in &lock_files {
            let lock_path = fts_path.join(lock_file);
            if lock_path.exists() {
                let _ = std::fs::remove_file(&lock_path);
            }
        }
    }

    /// Ensure writer is initialized for indexing
    fn ensure_writer(&mut self) -> Result<()> {
        if self.writer.is_none() {
            // Use retry logic for Windows file locking issues
            let writer = Self::create_writer_with_retry(&self.index)?;
            self.writer = Some(writer);
        }
        Ok(())
    }

    /// Add a chunk to the FTS index
    ///
    /// Includes writer recovery: if the writer was killed (e.g., by a background
    /// merge thread panic), it will be recreated and the operation retried once.
    pub fn add_chunk(
        &mut self,
        chunk_id: u32,
        content: &str,
        path: &str,
        signature: Option<&str>,
        kind: &str,
    ) -> Result<()> {
        self.ensure_writer()?;

        // Copy field handles before mutable borrow
        let chunk_id_field = self.chunk_id_field;
        let content_field = self.content_field;
        let path_field = self.path_field;
        let signature_field = self.signature_field;
        let kind_field = self.kind_field;

        let mut doc = TantivyDocument::new();
        doc.add_u64(chunk_id_field, chunk_id as u64);
        doc.add_text(content_field, content);
        doc.add_text(path_field, path);
        doc.add_text(kind_field, kind);
        if let Some(sig) = signature {
            doc.add_text(signature_field, sig);
        }

        let writer = self.writer.as_mut().unwrap();
        match writer.add_document(doc) {
            Ok(_) => Ok(()),
            Err(e) => {
                let error_str = e.to_string();
                if error_str.contains("writer was killed")
                    || error_str.contains("index writer was killed")
                {
                    tracing::debug!(
                        "FTS writer was killed, recreating and retrying add_chunk for chunk {}",
                        chunk_id
                    );

                    // Drop the dead writer and recreate
                    self.writer = None;
                    self.ensure_writer()?;

                    // Rebuild the document for retry
                    let mut retry_doc = TantivyDocument::new();
                    retry_doc.add_u64(chunk_id_field, chunk_id as u64);
                    retry_doc.add_text(content_field, content);
                    retry_doc.add_text(path_field, path);
                    retry_doc.add_text(kind_field, kind);
                    if let Some(sig) = signature {
                        retry_doc.add_text(signature_field, sig);
                    }

                    let writer = self.writer.as_mut().unwrap();
                    writer.add_document(retry_doc).map_err(|e| {
                        anyhow!("FTS add_document failed after writer recovery: {}", e)
                    })?;
                    Ok(())
                } else {
                    Err(anyhow!("FTS add_document failed: {}", error_str))
                }
            }
        }
    }

    /// Delete a chunk by ID
    pub fn delete_chunk(&mut self, chunk_id: u32) -> Result<()> {
        self.ensure_writer()?;
        let chunk_id_field = self.chunk_id_field;
        let writer = self.writer.as_mut().unwrap();
        let term = Term::from_field_u64(chunk_id_field, chunk_id as u64);
        writer.delete_term(term);
        Ok(())
    }

    /// Delete all chunks for a file path
    #[allow(dead_code)] // Reserved for file-level deletion
    pub fn delete_by_path(&mut self, path: &str) -> Result<()> {
        self.ensure_writer()?;
        let path_field = self.path_field;
        let writer = self.writer.as_mut().unwrap();
        let term = Term::from_field_text(path_field, path);
        writer.delete_term(term);
        Ok(())
    }

    /// Commit pending changes with retry logic for Windows file locking.
    ///
    /// If the writer was killed (background merge panic), it is recreated.
    /// Data since the last successful commit will be lost in that case, but
    /// indexing can continue rather than aborting entirely.
    pub fn commit(&mut self) -> Result<()> {
        if self.writer.is_none() {
            return Ok(());
        }

        let max_retries = 5;
        let mut last_error: Option<String> = None;

        for attempt in 0..max_retries {
            if attempt > 0 {
                // Wait before retry (exponential backoff: 100ms, 200ms, 400ms, 800ms)
                std::thread::sleep(std::time::Duration::from_millis(100 * (1 << attempt)));
            }

            let writer = self.writer.as_mut().unwrap();
            match writer.commit() {
                Ok(_) => {
                    // Reload reader to see changes
                    if let Err(e) = self.reader.reload() {
                        // Non-fatal: reader will eventually catch up
                        tracing::debug!("Reader reload warning: {}", e);
                    }
                    return Ok(());
                }
                Err(e) => {
                    let error_str = e.to_string();
                    last_error = Some(error_str.clone());

                    // Writer was killed by background thread panic — recreate it
                    if error_str.contains("writer was killed")
                        || error_str.contains("index writer was killed")
                    {
                        tracing::debug!(
                            "FTS writer was killed during commit (attempt {}/{}). \
                             Recreating writer. Data since last commit may be lost.",
                            attempt + 1,
                            max_retries
                        );
                        self.writer = None;
                        self.ensure_writer()?;
                        // After recreating, the pending data is gone, so commit
                        // the new (empty) writer to ensure a clean state
                        if let Some(ref mut w) = self.writer {
                            w.commit()
                                .map_err(|e| anyhow!("FTS commit after recovery failed: {}", e))?;
                        }
                        if let Err(e) = self.reader.reload() {
                            tracing::debug!("Reader reload warning: {}", e);
                        }
                        return Ok(());
                    }

                    // File locking error — retry with backoff
                    if error_str.contains("Access is denied")
                        || error_str.contains("PermissionDenied")
                        || error_str.contains("IoError")
                    {
                        tracing::debug!(
                            "FTS commit retry {}/{}: {}",
                            attempt + 1,
                            max_retries,
                            error_str
                        );
                        // Continue to retry
                    } else {
                        // Non-recoverable error, fail immediately
                        return Err(anyhow!("FTS commit failed: {}", error_str));
                    }
                }
            }
        }

        // All retries exhausted
        Err(anyhow!(
            "FTS commit failed after {} retries: {}",
            max_retries,
            last_error.unwrap_or_default()
        ))
    }

    /// Search using BM25
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<FtsResult>> {
        let searcher = self.reader.searcher();

        // Parse query against content and signature fields
        let query_parser =
            QueryParser::for_index(&self.index, vec![self.content_field, self.signature_field]);

        // Parse query, fall back to match-all on error
        let parsed_query = match query_parser.parse_query(query) {
            Ok(q) => q,
            Err(_) => {
                // Try escaping special characters
                let escaped = query.replace(
                    [
                        ':', '(', ')', '[', ']', '{', '}', '^', '"', '~', '*', '?', '\\', '/',
                    ],
                    " ",
                );
                query_parser.parse_query(&escaped)?
            }
        };

        // Execute search
        let top_docs = searcher.search(&parsed_query, &TopDocs::with_limit(limit))?;

        // Convert to results
        let mut results = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let doc: TantivyDocument = searcher.doc(doc_address)?;

            if let Some(chunk_id) = doc.get_first(self.chunk_id_field) {
                if let Some(id) = chunk_id.as_u64() {
                    results.push(FtsResult {
                        chunk_id: id as u32,
                        score,
                    });
                }
            }
        }

        Ok(results)
    }

    /// Get statistics about the index
    pub fn stats(&self) -> Result<FtsStats> {
        let searcher = self.reader.searcher();
        let num_docs = searcher.num_docs() as usize;

        Ok(FtsStats {
            num_documents: num_docs,
        })
    }

    /// Clear the entire index
    #[allow(dead_code)] // Reserved for index reset
    pub fn clear(&mut self) -> Result<()> {
        self.ensure_writer()?;
        let writer = self.writer.as_mut().unwrap();
        writer.delete_all_documents()?;
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }
}

/// Statistics about the FTS index
#[derive(Debug, Clone)]
#[allow(dead_code)] // Part of public API for debugging/monitoring
pub struct FtsStats {
    #[allow(dead_code)] // Part of public API for debugging/monitoring
    pub num_documents: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_fts_basic() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().to_path_buf();

        let mut store = FtsStore::new(&db_path)?;

        // Add some chunks
        store.add_chunk(
            1,
            "fn hello_world() { println!(\"Hello!\"); }",
            "src/main.rs",
            Some("hello_world"),
            "function",
        )?;
        store.add_chunk(
            2,
            "struct UserConfig { name: String, age: u32 }",
            "src/config.rs",
            Some("UserConfig"),
            "struct",
        )?;
        store.add_chunk(
            3,
            "fn process_data(data: Vec<u8>) -> Result<()>",
            "src/processor.rs",
            Some("process_data"),
            "function",
        )?;

        store.commit()?;

        // Search for hello
        let results = store.search("hello", 10)?;
        assert!(!results.is_empty());
        assert_eq!(results[0].chunk_id, 1);

        // Search for UserConfig
        let results = store.search("UserConfig", 10)?;
        assert!(!results.is_empty());
        assert_eq!(results[0].chunk_id, 2);

        // Search for process
        let results = store.search("process data", 10)?;
        assert!(!results.is_empty());
        assert_eq!(results[0].chunk_id, 3);

        Ok(())
    }

    #[test]
    fn test_fts_delete() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().to_path_buf();

        let mut store = FtsStore::new(&db_path)?;

        store.add_chunk(1, "test content one", "file1.rs", None, "block")?;
        store.add_chunk(2, "test content two", "file2.rs", None, "block")?;
        store.commit()?;

        // Should find both
        let results = store.search("test content", 10)?;
        assert_eq!(results.len(), 2);

        // Delete one
        store.delete_chunk(1)?;
        store.commit()?;

        // Should find only one
        let results = store.search("test content", 10)?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_id, 2);

        Ok(())
    }
}
