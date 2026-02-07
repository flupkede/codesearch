#![allow(dead_code)]

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::Path;

mod dedup;
mod extractor;
mod fallback;
mod grammar;
mod parser;
mod semantic;
mod tree_sitter;

pub use semantic::SemanticChunker;

/// Default number of context lines before/after a chunk
pub const DEFAULT_CONTEXT_LINES: usize = 3;

/// Represents a chunk of code with metadata
#[derive(Debug, Clone)]
pub struct Chunk {
    /// The actual content of the chunk
    pub content: String,

    /// Starting line number (0-indexed)
    pub start_line: usize,

    /// Ending line number (0-indexed)
    pub end_line: usize,

    /// Type of chunk
    pub kind: ChunkKind,

    /// Context breadcrumbs (e.g., ["File: main.rs", "Class: Server", "Function: handle_request"])
    pub context: Vec<String>,

    /// File path this chunk belongs to
    pub path: String,

    /// Function/method signature (if applicable)
    /// Example: "fn sort<T: Ord>(items: Vec<T>) -> Vec<T>"
    pub signature: Option<String>,

    /// Extracted docstring/documentation comment
    pub docstring: Option<String>,

    /// Whether this chunk is complete (not split)
    pub is_complete: bool,

    /// If this chunk was split, which part is it? (0, 1, 2...)
    pub split_index: Option<usize>,

    /// Content hash for deduplication
    pub hash: String,

    /// Lines of code immediately before this chunk (for context)
    pub context_prev: Option<String>,

    /// Lines of code immediately after this chunk (for context)
    pub context_next: Option<String>,
}

impl Chunk {
    /// Create a new chunk with basic information
    pub fn new(
        content: String,
        start_line: usize,
        end_line: usize,
        kind: ChunkKind,
        path: String,
    ) -> Self {
        let hash = Self::compute_hash(&content);

        Self {
            content,
            start_line,
            end_line,
            kind,
            context: Vec::new(),
            path,
            signature: None,
            docstring: None,
            is_complete: true,
            split_index: None,
            hash,
            context_prev: None,
            context_next: None,
        }
    }

    /// Compute SHA-256 hash of content for deduplication
    pub fn compute_hash(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// TEST METHOD: Estimate memory usage of this chunk in bytes
    pub fn estimate_memory_usage(&self) -> usize {
        let content_size = self.content.len();
        let context_size = self.context.iter().map(|s| s.len()).sum::<usize>();
        let signature_size = self.signature.as_ref().map_or(0, |s| s.len());
        let docstring_size = self.docstring.as_ref().map_or(0, |s| s.len());
        let context_prev_size = self.context_prev.as_ref().map_or(0, |s| s.len());
        let context_next_size = self.context_next.as_ref().map_or(0, |s| s.len());

        content_size
            + context_size
            + signature_size
            + docstring_size
            + context_prev_size
            + context_next_size
    }

    /// TEST METHOD: Check if this chunk contains a specific keyword
    pub fn contains_keyword(&self, keyword: &str) -> bool {
        self.content.contains(keyword)
            || self.signature.as_ref().is_some_and(|s| s.contains(keyword))
            || self.docstring.as_ref().is_some_and(|s| s.contains(keyword))
    }

    /// Check if this chunk is likely a duplicate based on hash
    pub fn is_duplicate_of(&self, other: &Chunk) -> bool {
        self.hash == other.hash
    }

    /// Get the number of lines in this chunk
    pub fn line_count(&self) -> usize {
        self.end_line.saturating_sub(self.start_line)
    }

    /// Get the size of this chunk in bytes
    pub fn size_bytes(&self) -> usize {
        self.content.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    Function,  // Standalone function
    Class,     // Class definition (non-Rust languages)
    Method,    // Method within class/impl
    Struct,    // Struct definition (Rust)
    Enum,      // Enum definition
    Trait,     // Trait definition (Rust)
    Interface, // Interface (TypeScript, Java)
    Impl,      // Impl block (Rust)
    Mod,       // Module definition
    TypeAlias, // Type alias
    Const,     // Constant
    Static,    // Static variable
    Block,     // Gap/unstructured code
    Anchor,    // File-level summary chunk
    Other,     // Catch-all
}

/// Trait for chunking strategies
pub trait Chunker: Send + Sync {
    /// Chunk a file into semantic pieces
    fn chunk_file(&self, path: &Path, content: &str) -> Result<Vec<Chunk>>;
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn test_chunker() {
        // TODO: Add tests
    }
}
