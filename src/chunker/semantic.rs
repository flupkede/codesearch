#![allow(dead_code)]

use super::{Chunk, ChunkKind, Chunker, DEFAULT_CONTEXT_LINES};
use crate::chunker::extractor::{get_extractor, LanguageExtractor};
use crate::chunker::parser::CodeParser;
use crate::file::Language;
use anyhow::Result;
use std::path::Path;
use tree_sitter::Node;

/// Smart semantic chunker using tree-sitter and language-specific extractors
pub struct SemanticChunker {
    parser: CodeParser,
    max_chunk_lines: usize,
    max_chunk_chars: usize,
    overlap_lines: usize,
    context_lines: usize,
}

impl SemanticChunker {
    pub fn new(max_chunk_lines: usize, max_chunk_chars: usize, overlap_lines: usize) -> Self {
        Self {
            parser: CodeParser::new(),
            max_chunk_lines,
            max_chunk_chars,
            overlap_lines,
            context_lines: DEFAULT_CONTEXT_LINES,
        }
    }

    /// Set the number of context lines to extract before/after each chunk
    pub fn with_context_lines(mut self, lines: usize) -> Self {
        self.context_lines = lines;
        self
    }

    /// Chunk a file using semantic analysis
    pub fn chunk_semantic(
        &mut self,
        language: Language,
        path: &Path,
        content: &str,
    ) -> Result<Vec<Chunk>> {
        // 1. Check if we have an extractor for this language
        let extractor = match get_extractor(language) {
            Some(ext) => ext,
            None => {
                // Fall back to simple chunking for unsupported languages
                return Ok(self.fallback_chunk(path, content));
            }
        };

        // 2. Parse the code
        let parsed = self.parser.parse(language, content)?;

        // 3. Visit AST and extract chunks
        let mut definition_chunks = Vec::new();
        let mut gap_tracker = GapTracker::new(content);

        let file_context = format!("File: {}", path.display());
        self.visit_node(
            parsed.root_node(),
            parsed.source().as_bytes(),
            &*extractor,
            &[file_context],
            &mut definition_chunks,
            &mut gap_tracker,
        );

        // 4. Extract gap chunks (code between definitions)
        let gap_chunks = gap_tracker.extract_gaps(path);

        // 5. Combine and sort all chunks by position
        let mut all_chunks = definition_chunks;
        all_chunks.extend(gap_chunks);
        all_chunks.sort_by_key(|c| c.start_line);

        // 6. Populate context windows (lines before/after each chunk)
        let source_lines: Vec<&str> = content.lines().collect();
        self.populate_context_windows(&mut all_chunks, &source_lines);

        // 7. Split oversized chunks
        let final_chunks = all_chunks
            .into_iter()
            .flat_map(|c| self.split_if_needed(c))
            .collect();

        Ok(final_chunks)
    }

    /// Populate context_prev and context_next for each chunk
    fn populate_context_windows(&self, chunks: &mut [Chunk], source_lines: &[&str]) {
        let total_lines = source_lines.len();

        for chunk in chunks.iter_mut() {
            // Extract context_prev (N lines before start_line)
            if chunk.start_line > 0 && self.context_lines > 0 {
                let prev_start = chunk.start_line.saturating_sub(self.context_lines);
                let prev_end = chunk.start_line;
                if prev_start < prev_end && prev_end <= total_lines {
                    let prev_lines = &source_lines[prev_start..prev_end];
                    let prev_content = prev_lines.join("\n");
                    if !prev_content.trim().is_empty() {
                        chunk.context_prev = Some(prev_content);
                    }
                }
            }

            // Extract context_next (N lines after end_line)
            if chunk.end_line < total_lines && self.context_lines > 0 {
                let next_start = chunk.end_line;
                let next_end = (chunk.end_line + self.context_lines).min(total_lines);
                if next_start < next_end {
                    let next_lines = &source_lines[next_start..next_end];
                    let next_content = next_lines.join("\n");
                    if !next_content.trim().is_empty() {
                        chunk.context_next = Some(next_content);
                    }
                }
            }
        }
    }

    /// Recursively visit AST nodes and extract chunks
    fn visit_node(
        &self,
        node: Node,
        source: &[u8],
        extractor: &dyn LanguageExtractor,
        context_stack: &[String],
        chunks: &mut Vec<Chunk>,
        gap_tracker: &mut GapTracker,
    ) {
        // Check if this node is a definition
        let is_definition = extractor.definition_types().contains(&node.kind());

        if is_definition {
            // Mark this range as covered (not a gap)
            gap_tracker.mark_covered(node.start_position().row, node.end_position().row);

            // Extract metadata using the language extractor
            let kind = extractor.classify(node);
            let name = extractor.extract_name(node, source);
            let signature = extractor.extract_signature(node, source);
            let docstring = extractor.extract_docstring(node, source);

            // Build label for context breadcrumb
            let label = extractor
                .build_label(node, source)
                .or_else(|| name.as_ref().map(|n| format!("{:?}: {}", kind, n)))
                .unwrap_or_else(|| format!("{:?}", kind));

            // Build new context stack
            let mut new_context = context_stack.to_vec();
            new_context.push(label);

            // Extract content (without docstring if we have it separate)
            let content = match node.utf8_text(source) {
                Ok(text) => text.to_string(),
                Err(_) => return, // Skip if we can't extract text
            };

            // Create chunk
            let path_str = context_stack
                .first()
                .map(|s| s.strip_prefix("File: ").unwrap_or(s))
                .unwrap_or("")
                .to_string();

            let mut chunk = Chunk::new(
                content,
                node.start_position().row,
                node.end_position().row + 1, // tree-sitter uses 0-based, we use line count
                kind,
                path_str,
            );
            chunk.context = new_context.clone();
            chunk.signature = signature;
            chunk.docstring = docstring;

            chunks.push(chunk);

            // Visit children with updated context
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                self.visit_node(child, source, extractor, &new_context, chunks, gap_tracker);
            }
        } else {
            // Not a definition, just visit children with same context
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                self.visit_node(child, source, extractor, context_stack, chunks, gap_tracker);
            }
        }
    }

    /// Fallback chunking for unsupported languages
    fn fallback_chunk(&self, path: &Path, content: &str) -> Vec<Chunk> {
        let lines: Vec<&str> = content.lines().collect();
        let mut chunks = Vec::new();
        let stride = (self.max_chunk_lines - self.overlap_lines).max(1);

        let path_str = path.to_string_lossy().to_string();
        let context = vec![format!("File: {}", path_str)];

        let mut i = 0;
        while i < lines.len() {
            let end = (i + self.max_chunk_lines).min(lines.len());
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

    /// Split a chunk if it exceeds size limits
    fn split_if_needed(&self, chunk: Chunk) -> Vec<Chunk> {
        let line_count = chunk.line_count();
        let char_count = chunk.size_bytes();

        // Check if splitting is needed
        if line_count <= self.max_chunk_lines && char_count <= self.max_chunk_chars {
            return vec![chunk];
        }

        // Need to split
        let lines: Vec<&str> = chunk.content.lines().collect();
        let mut split_chunks = Vec::new();
        let stride = (self.max_chunk_lines - self.overlap_lines).max(1);

        let mut i = 0;
        let mut split_index = 0;

        while i < lines.len() {
            let end = (i + self.max_chunk_lines).min(lines.len());
            let chunk_lines = &lines[i..end];

            if !chunk_lines.is_empty() {
                let content = chunk_lines.join("\n");
                let mut split_chunk = Chunk::new(
                    content,
                    chunk.start_line + i,
                    chunk.start_line + end,
                    chunk.kind,
                    chunk.path.clone(),
                );

                // Preserve metadata
                split_chunk.context = chunk.context.clone();
                split_chunk.signature = chunk.signature.clone();
                split_chunk.docstring = if split_index == 0 {
                    chunk.docstring.clone() // Only first chunk gets docstring
                } else {
                    None
                };
                split_chunk.is_complete = false;
                split_chunk.split_index = Some(split_index);

                split_chunks.push(split_chunk);
                split_index += 1;
            }

            i += stride;
        }

        // Add header to split chunks to indicate they're partial
        let total_parts = split_chunks.len();
        for chunk in &mut split_chunks {
            if let Some(idx) = chunk.split_index {
                let header = format!(
                    "// [Part {}/{}] {}\n",
                    idx + 1,
                    total_parts,
                    chunk
                        .signature
                        .as_ref()
                        .unwrap_or(&"(continued)".to_string())
                );
                chunk.content = header + &chunk.content;
            }
        }

        split_chunks
    }
}

impl Chunker for SemanticChunker {
    fn chunk_file(&self, path: &Path, content: &str) -> Result<Vec<Chunk>> {
        // Detect language from path
        let language = Language::from_path(path);

        // Can't use &mut self in trait method, so we need a workaround
        // Create a temporary parser for this call
        let mut temp_chunker = SemanticChunker::new(
            self.max_chunk_lines,
            self.max_chunk_chars,
            self.overlap_lines,
        );

        temp_chunker.chunk_semantic(language, path, content)
    }
}

/// Helper to track gaps (code between definitions)
struct GapTracker<'a> {
    #[allow(dead_code)]
    content: &'a str,
    lines: Vec<&'a str>,
    covered: Vec<bool>, // covered[i] = true if line i is part of a definition
}

impl<'a> GapTracker<'a> {
    fn new(content: &'a str) -> Self {
        let lines: Vec<&str> = content.lines().collect();
        let covered = vec![false; lines.len()];

        Self {
            content,
            lines,
            covered,
        }
    }

    /// Mark a range of lines as covered by a definition
    fn mark_covered(&mut self, start_line: usize, end_line: usize) {
        for i in start_line..=end_line.min(self.covered.len().saturating_sub(1)) {
            if i < self.covered.len() {
                self.covered[i] = true;
            }
        }
    }

    /// Extract gap chunks (uncovered regions)
    fn extract_gaps(&self, path: &Path) -> Vec<Chunk> {
        let mut gaps = Vec::new();
        let path_str = path.to_string_lossy().to_string();
        let context = vec![format!("File: {}", path_str)];

        let mut gap_start: Option<usize> = None;

        for (i, &is_covered) in self.covered.iter().enumerate() {
            if !is_covered {
                // Start or continue a gap
                if gap_start.is_none() {
                    gap_start = Some(i);
                }
            } else {
                // End of gap
                if let Some(start) = gap_start {
                    // Extract gap content
                    let gap_lines = &self.lines[start..i];
                    let gap_content = gap_lines.join("\n");

                    // Only create chunk if gap is not empty/whitespace
                    if !gap_content.trim().is_empty() {
                        let kind = Self::classify_gap(&gap_content);
                        let mut chunk = Chunk::new(gap_content, start, i, kind, path_str.clone());
                        chunk.context = context.clone();
                        gaps.push(chunk);
                    }

                    gap_start = None;
                }
            }
        }

        // Handle final gap (if file ends with gap)
        if let Some(start) = gap_start {
            let gap_lines = &self.lines[start..];
            let gap_content = gap_lines.join("\n");

            if !gap_content.trim().is_empty() {
                let kind = Self::classify_gap(&gap_content);
                let mut chunk =
                    Chunk::new(gap_content, start, self.lines.len(), kind, path_str.clone());
                chunk.context = context.clone();
                gaps.push(chunk);
            }
        }

        gaps
    }

    /// Classify what kind of gap this is
    fn classify_gap(content: &str) -> ChunkKind {
        let trimmed = content.trim();

        // Check if it's mostly imports
        let import_count = trimmed
            .lines()
            .filter(|line| {
                let line = line.trim();
                line.starts_with("import ")
                    || line.starts_with("from ")
                    || line.starts_with("use ")
                    || line.starts_with("#include")
            })
            .count();

        if import_count > trimmed.lines().count() / 2 {
            return ChunkKind::Block; // Could add ChunkKind::Imports later
        }

        // Check if it's module-level docs
        if trimmed.starts_with("//!") || trimmed.starts_with("/*!") {
            return ChunkKind::Block; // Could add ChunkKind::ModuleDocs later
        }

        ChunkKind::Block
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_semantic_chunker_creation() {
        let chunker = SemanticChunker::new(100, 2000, 10);
        assert_eq!(chunker.max_chunk_lines, 100);
        assert_eq!(chunker.max_chunk_chars, 2000);
        assert_eq!(chunker.overlap_lines, 10);
    }

    #[test]
    fn test_chunk_rust_code() {
        let mut chunker = SemanticChunker::new(100, 2000, 10);

        let rust_code = r#"
/// This is a doc comment
fn hello_world() {
    println!("Hello, world!");
}

fn add(a: i32, b: i32) -> i32 {
    a + b
}

struct Point {
    x: f64,
    y: f64,
}
"#;

        let path = Path::new("test.rs");
        let chunks = chunker
            .chunk_semantic(Language::Rust, path, rust_code)
            .unwrap();

        // Should have at least 3 definition chunks (2 functions + 1 struct)
        assert!(
            chunks.len() >= 3,
            "Expected at least 3 chunks, got {}",
            chunks.len()
        );

        // Check that we have function chunks
        let function_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Function)
            .collect();
        assert!(
            function_chunks.len() >= 2,
            "Expected at least 2 function chunks"
        );

        // Check that first function has signature
        let hello_chunk = function_chunks
            .iter()
            .find(|c| c.content.contains("hello_world"));
        assert!(hello_chunk.is_some(), "Should find hello_world function");

        if let Some(chunk) = hello_chunk {
            assert!(chunk.signature.is_some(), "Should have signature");
            assert!(chunk.signature.as_ref().unwrap().contains("fn hello_world"));
        }
    }

    #[test]
    fn test_chunk_python_code() {
        let mut chunker = SemanticChunker::new(100, 2000, 10);

        let python_code = r#"
def hello():
    """Say hello"""
    print("Hello!")

class Calculator:
    """A simple calculator"""

    def add(self, a, b):
        """Add two numbers"""
        return a + b
"#;

        let path = Path::new("test.py");
        let chunks = chunker
            .chunk_semantic(Language::Python, path, python_code)
            .unwrap();

        // Should have at least 2 chunks (function + class)
        assert!(chunks.len() >= 2, "Expected at least 2 chunks");

        // Check for docstrings
        let chunks_with_docs: Vec<_> = chunks.iter().filter(|c| c.docstring.is_some()).collect();
        assert!(
            !chunks_with_docs.is_empty(),
            "Should have chunks with docstrings"
        );
    }

    #[test]
    fn test_chunk_unsupported_language() {
        let mut chunker = SemanticChunker::new(100, 2000, 10);

        let content =
            "Some random text file\nWith multiple lines\nThat should be chunked\nAs fallback";
        let path = Path::new("test.txt");

        let chunks = chunker
            .chunk_semantic(Language::Unknown, path, content)
            .unwrap();

        // Should use fallback chunking
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.kind == ChunkKind::Block));
    }

    #[test]
    fn test_gap_tracking() {
        let content = "line 0\nline 1\nline 2\nline 3\nline 4";
        let mut tracker = GapTracker::new(content);

        // Mark lines 1-2 as covered
        tracker.mark_covered(1, 2);

        // Should have gaps: [0], [3-4]
        let path = Path::new("test.txt");
        let gaps = tracker.extract_gaps(path);

        assert_eq!(gaps.len(), 2, "Should have 2 gaps");
        assert_eq!(gaps[0].start_line, 0);
        assert_eq!(gaps[0].end_line, 1);
        assert_eq!(gaps[1].start_line, 3);
        assert_eq!(gaps[1].end_line, 5);
    }

    #[test]
    fn test_chunk_splitting() {
        let chunker = SemanticChunker::new(5, 100, 1); // Very small limit

        let large_content = (0..20)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let chunk = Chunk::new(
            large_content,
            0,
            20,
            ChunkKind::Function,
            "test.rs".to_string(),
        );

        let splits = chunker.split_if_needed(chunk);

        // Should be split into multiple chunks
        assert!(splits.len() > 1, "Should split large chunk");

        // All splits should be marked as incomplete
        for split in &splits {
            assert!(
                !split.is_complete,
                "Split chunks should be marked incomplete"
            );
            assert!(
                split.split_index.is_some(),
                "Split chunks should have index"
            );
        }
    }

    #[test]
    fn test_context_breadcrumbs() {
        let mut chunker = SemanticChunker::new(100, 2000, 10);

        let rust_code = r#"
impl MyStruct {
    fn method(&self) {
        println!("method");
    }
}
"#;

        let path = Path::new("test.rs");
        let chunks = chunker
            .chunk_semantic(Language::Rust, path, rust_code)
            .unwrap();

        // Find method chunk
        let method_chunk = chunks.iter().find(|c| c.kind == ChunkKind::Method);

        if let Some(chunk) = method_chunk {
            // Should have context: File > Impl > Method
            assert!(chunk.context.len() >= 2, "Should have nested context");
            assert!(chunk.context[0].contains("File:"));
        }
    }
}
