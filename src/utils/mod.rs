//! Utility functions and helpers for codesearch
//!
//! This module contains reusable utility functions used across the codebase.

use crate::chunker::Chunk;
use std::collections::HashMap;

/// Group chunks by their file path
///
/// This is a common pattern used in indexing and search operations.
/// It takes an iterator of (chunk, value) pairs and groups them by the chunk's path.
///
/// # Arguments
/// * `items` - Iterator of (chunk, value) pairs to group
///
/// # Returns
/// * HashMap mapping file paths (as strings) to vectors of values
pub fn group_chunks_by_path<T>(items: impl Iterator<Item = (Chunk, T)>) -> HashMap<String, Vec<T>>
where
    T: Clone,
{
    items.fold(HashMap::new(), |mut acc, (chunk, value)| {
        acc.entry(chunk.path).or_default().push(value);
        acc
    })
}

/// Group chunks by their file path with pre-allocated capacity
///
/// Same as `group_chunks_by_path` but allows pre-allocating HashMap capacity
/// for better performance when the number of files is known.
///
/// # Arguments
/// * `items` - Iterator of (chunk, value) pairs to group
/// * `capacity` - Expected number of unique file paths
///
/// # Returns
/// * HashMap mapping file paths (as strings) to vectors of values
pub fn group_chunks_by_path_with_capacity<T>(
    items: impl Iterator<Item = (Chunk, T)>,
    capacity: usize,
) -> HashMap<String, Vec<T>>
where
    T: Clone,
{
    let mut map: HashMap<String, Vec<T>> = HashMap::with_capacity(capacity);
    for (chunk, value) in items {
        map.entry(chunk.path).or_default().push(value);
    }
    map
}

/// Group embedded chunks by their file path
///
/// Specialized version for embedded chunks (which already contain the chunk data).
///
/// # Arguments
/// * `embedded_chunks` - Slice of embedded chunks to group
/// * `chunk_ids` - Slice of chunk IDs corresponding to the embedded chunks
///
/// # Returns
/// * HashMap mapping file paths (as strings) to vectors of chunk IDs
pub fn group_embedded_chunks_by_path(
    embedded_chunks: &[crate::embed::EmbeddedChunk],
    chunk_ids: &[u32],
) -> HashMap<String, Vec<u32>> {
    let capacity = embedded_chunks.len() / 10; // Estimate: ~10 chunks per file
    let mut map: HashMap<String, Vec<u32>> = HashMap::with_capacity(capacity.max(1));

    for (chunk, chunk_id) in embedded_chunks.iter().zip(chunk_ids.iter()) {
        map.entry(chunk.chunk.path.clone())
            .or_default()
            .push(*chunk_id);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::ChunkKind;

    #[test]
    fn test_group_chunks_by_path() {
        let chunk1 = Chunk::new(
            "content1".to_string(),
            1,
            1,
            ChunkKind::Other,
            "path1.rs".to_string(),
        );
        let chunk2 = Chunk::new(
            "content2".to_string(),
            2,
            2,
            ChunkKind::Other,
            "path1.rs".to_string(),
        );
        let chunk3 = Chunk::new(
            "content3".to_string(),
            3,
            3,
            ChunkKind::Other,
            "path2.rs".to_string(),
        );

        let items = vec![(chunk1, 1), (chunk2, 2), (chunk3, 3)];

        let grouped = group_chunks_by_path(items.into_iter());

        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped.get("path1.rs"), Some(&vec![1, 2]));
        assert_eq!(grouped.get("path2.rs"), Some(&vec![3]));
    }

    #[test]
    fn test_group_chunks_by_path_with_capacity() {
        let chunk1 = Chunk::new(
            "content1".to_string(),
            1,
            1,
            ChunkKind::Other,
            "path1.rs".to_string(),
        );

        let chunk2 = Chunk::new(
            "content2".to_string(),
            2,
            2,
            ChunkKind::Other,
            "path2.rs".to_string(),
        );

        let items = vec![(chunk1, 1), (chunk2, 2)];

        let grouped = group_chunks_by_path_with_capacity(items.into_iter(), 2);

        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped.get("path1.rs"), Some(&vec![1]));
        assert_eq!(grouped.get("path2.rs"), Some(&vec![2]));
    }
}
