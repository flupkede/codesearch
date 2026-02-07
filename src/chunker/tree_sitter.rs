#![allow(dead_code)]

use super::{Chunk, ChunkKind, Chunker};
use anyhow::Result;
use std::path::Path;

/// Smart chunker using tree-sitter for semantic boundaries
pub struct TreeSitterChunker {
    max_chunk_lines: usize,
    #[allow(dead_code)]
    max_chunk_chars: usize,
    overlap_lines: usize,
}

impl TreeSitterChunker {
    pub fn new(max_chunk_lines: usize, max_chunk_chars: usize, overlap_lines: usize) -> Self {
        Self {
            max_chunk_lines,
            max_chunk_chars,
            overlap_lines,
        }
    }
}

impl Chunker for TreeSitterChunker {
    fn chunk_file(&self, path: &Path, content: &str) -> Result<Vec<Chunk>> {
        // TODO: Implement tree-sitter based chunking
        // For now, use fallback chunking
        Ok(fallback_chunk(
            path,
            content,
            self.max_chunk_lines,
            self.overlap_lines,
        ))
    }
}

/// Fallback chunking strategy (sliding window)
fn fallback_chunk(
    path: &Path,
    content: &str,
    max_chunk_lines: usize,
    overlap_lines: usize,
) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    let mut chunks = Vec::new();
    let stride = (max_chunk_lines - overlap_lines).max(1);

    let path_str = path.to_string_lossy().to_string();
    let context = vec![format!("File: {}", path_str)];

    let mut i = 0;
    while i < lines.len() {
        let end = (i + max_chunk_lines).min(lines.len());
        let chunk_lines = &lines[i..end];

        if !chunk_lines.is_empty() {
            let content = chunk_lines.join("\n");
            let mut chunk = Chunk::new(content, i, end, ChunkKind::Block, path_str.clone());
            chunk.context = context.clone();
            chunks.push(chunk);
        }

        i += stride;
    }

    chunks
}
