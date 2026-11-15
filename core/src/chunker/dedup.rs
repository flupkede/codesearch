use super::Chunk;
use dashmap::DashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Deduplicates chunks based on content hash
///
/// This is useful for:
/// - License headers that appear in every file
/// - Generated code (protobuf, GraphQL schemas)
/// - Common boilerplate patterns
/// - Repeated utility functions
///
/// By deduplicating, we avoid embedding the same content multiple times,
/// which can save 20-30% of embedding costs on typical codebases.
pub struct ChunkDeduplicator {
    /// Map from hash to first occurrence index
    seen: DashMap<String, usize>,

    /// Count of unique chunks
    unique_count: AtomicUsize,

    /// Count of duplicate chunks skipped
    duplicate_count: AtomicUsize,
}

impl ChunkDeduplicator {
    /// Create a new deduplicator
    pub fn new() -> Self {
        Self {
            seen: DashMap::new(),
            unique_count: AtomicUsize::new(0),
            duplicate_count: AtomicUsize::new(0),
        }
    }

    /// Deduplicate a list of chunks, keeping only the first occurrence of each unique content
    ///
    /// The order of chunks is preserved.
    pub fn deduplicate(&self, chunks: Vec<Chunk>) -> Vec<Chunk> {
        chunks
            .into_iter()
            .enumerate()
            .filter_map(|(idx, chunk)| {
                let hash = chunk.hash.clone();

                // Check if we've seen this hash before
                if self.seen.insert(hash.clone(), idx).is_none() {
                    // First occurrence: keep it
                    self.unique_count.fetch_add(1, Ordering::Relaxed);
                    Some(chunk)
                } else {
                    // Duplicate: skip it
                    self.duplicate_count.fetch_add(1, Ordering::Relaxed);
                    None
                }
            })
            .collect()
    }

    /// Check if a chunk is a duplicate without consuming it
    pub fn is_duplicate(&self, chunk: &Chunk) -> bool {
        self.seen.contains_key(&chunk.hash)
    }

    /// Mark a chunk as seen without deduplicating
    pub fn mark_seen(&self, chunk: &Chunk, index: usize) {
        self.seen.insert(chunk.hash.clone(), index);
    }

    /// Get statistics about deduplication
    pub fn stats(&self) -> DedupStats {
        let unique = self.unique_count.load(Ordering::Relaxed);
        let duplicates = self.duplicate_count.load(Ordering::Relaxed);
        let total = unique + duplicates;

        DedupStats {
            unique_chunks: unique,
            duplicate_chunks: duplicates,
            total_chunks: total,
            dedup_ratio: if total > 0 {
                duplicates as f64 / total as f64
            } else {
                0.0
            },
        }
    }

    /// Reset the deduplicator, clearing all state
    pub fn reset(&self) {
        self.seen.clear();
        self.unique_count.store(0, Ordering::Relaxed);
        self.duplicate_count.store(0, Ordering::Relaxed);
    }

    /// Get the number of unique hashes seen
    pub fn unique_count(&self) -> usize {
        self.seen.len()
    }
}

impl Default for ChunkDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics about chunk deduplication
#[derive(Debug, Clone)]
pub struct DedupStats {
    /// Number of unique chunks kept
    pub unique_chunks: usize,

    /// Number of duplicate chunks removed
    pub duplicate_chunks: usize,

    /// Total chunks processed
    pub total_chunks: usize,

    /// Ratio of duplicates (0.0 to 1.0)
    pub dedup_ratio: f64,
}

impl DedupStats {
    /// Get percentage of chunks that were duplicates
    pub fn dedup_percentage(&self) -> f64 {
        self.dedup_ratio * 100.0
    }

    /// Estimate embedding cost savings
    ///
    /// Assumes each embedding costs roughly the same
    pub fn cost_savings(&self) -> f64 {
        self.dedup_ratio
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::{Chunk, ChunkKind};

    fn create_test_chunk(content: &str, path: &str) -> Chunk {
        Chunk::new(
            content.to_string(),
            0,
            10,
            ChunkKind::Block,
            path.to_string(),
        )
    }

    #[test]
    fn test_no_duplicates() {
        let deduper = ChunkDeduplicator::new();

        let chunks = vec![
            create_test_chunk("content1", "file1.rs"),
            create_test_chunk("content2", "file2.rs"),
            create_test_chunk("content3", "file3.rs"),
        ];

        let original_len = chunks.len();
        let result = deduper.deduplicate(chunks);

        assert_eq!(result.len(), original_len);
        assert_eq!(deduper.stats().unique_chunks, 3);
        assert_eq!(deduper.stats().duplicate_chunks, 0);
        assert_eq!(deduper.stats().dedup_ratio, 0.0);
    }

    #[test]
    fn test_with_duplicates() {
        let deduper = ChunkDeduplicator::new();

        let chunks = vec![
            create_test_chunk("same content", "file1.rs"),
            create_test_chunk("same content", "file2.rs"),
            create_test_chunk("same content", "file3.rs"),
            create_test_chunk("different", "file4.rs"),
        ];

        let result = deduper.deduplicate(chunks);

        // Should keep only first occurrence of "same content" + the different one
        assert_eq!(result.len(), 2);
        assert_eq!(deduper.stats().unique_chunks, 2);
        assert_eq!(deduper.stats().duplicate_chunks, 2);
        assert_eq!(deduper.stats().dedup_ratio, 0.5);
    }

    #[test]
    fn test_license_header_deduplication() {
        let deduper = ChunkDeduplicator::new();

        let license = "// Copyright (c) 2024\n// Licensed under MIT";

        let chunks = vec![
            create_test_chunk(license, "file1.rs"),
            create_test_chunk(license, "file2.rs"),
            create_test_chunk(license, "file3.rs"),
            create_test_chunk(license, "file4.rs"),
            create_test_chunk(license, "file5.rs"),
            create_test_chunk("actual code", "file1.rs"),
        ];

        let result = deduper.deduplicate(chunks);

        // License appears 5 times, but we keep only first + the actual code
        assert_eq!(result.len(), 2);

        let stats = deduper.stats();
        assert_eq!(stats.duplicate_chunks, 4);
        assert!(stats.dedup_percentage() > 60.0); // ~66%
    }

    #[test]
    fn test_is_duplicate() {
        let deduper = ChunkDeduplicator::new();

        let chunk1 = create_test_chunk("content", "file1.rs");
        let chunk2 = create_test_chunk("content", "file2.rs"); // Same content
        let chunk3 = create_test_chunk("different", "file3.rs");

        // Initially nothing is seen
        assert!(!deduper.is_duplicate(&chunk1));

        // Mark first as seen
        deduper.mark_seen(&chunk1, 0);

        // Now chunk2 (same content) should be detected as duplicate
        assert!(deduper.is_duplicate(&chunk2));

        // But chunk3 is not a duplicate
        assert!(!deduper.is_duplicate(&chunk3));
    }

    #[test]
    fn test_reset() {
        let deduper = ChunkDeduplicator::new();

        let chunks = vec![
            create_test_chunk("content", "file1.rs"),
            create_test_chunk("content", "file2.rs"),
        ];

        let result = deduper.deduplicate(chunks);
        assert_eq!(result.len(), 1);

        // Reset the deduplicator
        deduper.reset();

        // Stats should be cleared
        let stats = deduper.stats();
        assert_eq!(stats.total_chunks, 0);
        assert_eq!(deduper.unique_count(), 0);
    }

    #[test]
    fn test_dedup_stats() {
        let deduper = ChunkDeduplicator::new();

        let chunks = vec![
            create_test_chunk("a", "f1"),
            create_test_chunk("a", "f2"),
            create_test_chunk("b", "f3"),
            create_test_chunk("a", "f4"),
            create_test_chunk("c", "f5"),
        ];

        deduper.deduplicate(chunks);

        let stats = deduper.stats();
        assert_eq!(stats.total_chunks, 5);
        assert_eq!(stats.unique_chunks, 3); // a, b, c
        assert_eq!(stats.duplicate_chunks, 2); // 2 more 'a's
        assert_eq!(stats.dedup_ratio, 0.4); // 2/5
        assert_eq!(stats.dedup_percentage(), 40.0);
    }

    #[test]
    fn test_order_preservation() {
        let deduper = ChunkDeduplicator::new();

        let chunks = vec![
            create_test_chunk("z", "f1"),
            create_test_chunk("y", "f2"),
            create_test_chunk("x", "f3"),
            create_test_chunk("y", "f4"), // Duplicate
        ];

        let result = deduper.deduplicate(chunks);

        // Order should be preserved: z, y, x (skipping second y)
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].content, "z");
        assert_eq!(result[1].content, "y");
        assert_eq!(result[2].content, "x");
    }
}
