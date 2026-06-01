#![allow(dead_code)]

use super::{Chunk, ChunkKind, Chunker, DEFAULT_CONTEXT_LINES};
use crate::cache::normalize_path;
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
        // Markdown/txt are chunked by heading section rather than by definition
        // node, so they take a dedicated path (no LanguageExtractor).
        if language == Language::Markdown {
            return self.chunk_markdown(path, content);
        }

        // 1. Check if we have an extractor for this language
        let extractor = match get_extractor(language) {
            Some(ext) => ext,
            None => {
                // Fall back to simple chunking for unsupported languages.  The
                // line-windowed fallback ignores the char budget, so route its
                // output through split_oversized to enforce max_chunk_chars and
                // avoid pathological huge single chunks (e.g. minified one-line text).
                return Ok(self
                    .fallback_chunk(path, content)
                    .into_iter()
                    .flat_map(|c| self.split_oversized(c))
                    .collect());
            }
        };

        // 2. Parse the code
        let parsed = self.parser.parse(language, content)?;

        // 3. Visit AST and extract chunks
        let mut definition_chunks = Vec::new();
        let mut gap_tracker = GapTracker::new(content);

        let file_context = format!("File: {}", normalize_path(path));
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

    /// Chunk a Markdown/text file by heading section.
    ///
    /// Uses the tree-sitter-md *block* grammar: the document is a tree of nested
    /// `section` nodes (one per heading). Each chunk is a single heading plus its
    /// own prose/code, *excluding* nested subsections (which become their own
    /// chunks). Heading text is carried in the breadcrumb context so the embedding
    /// captures the section's place in the document (e.g. `File: x.md > Title >
    /// Subsection`). Leading document content (YAML front-matter, prose before the
    /// first heading) becomes a single preamble chunk. Oversized sections are
    /// char/line-bounded via `split_oversized`, and a file with no parseable
    /// structure falls back to the line-windowed chunker (also bounded).
    fn chunk_markdown(&mut self, path: &Path, content: &str) -> Result<Vec<Chunk>> {
        let bounded_fallback = |this: &Self| -> Vec<Chunk> {
            this.fallback_chunk(path, content)
                .into_iter()
                .flat_map(|c| this.split_oversized(c))
                .collect()
        };

        let parsed = match self.parser.parse(Language::Markdown, content) {
            Ok(p) => p,
            Err(_) => return Ok(bounded_fallback(self)),
        };

        let source = content.as_bytes();
        let path_str = normalize_path(path);
        let file_context = format!("File: {}", path_str);
        let root = parsed.root_node();

        let mut cursor = root.walk();
        let top: Vec<Node> = root.named_children(&mut cursor).collect();

        let mut chunks: Vec<Chunk> = Vec::new();

        // Leading non-section nodes (front-matter / prose before the first heading)
        // form a single preamble chunk.
        let mut preamble_end = 0;
        while preamble_end < top.len() && top[preamble_end].kind() != "section" {
            preamble_end += 1;
        }
        if preamble_end > 0 {
            let start_byte = top[0].start_byte();
            let end_byte = top[preamble_end - 1].end_byte();
            if let Some(chunk) = Self::md_chunk(
                source,
                start_byte,
                end_byte,
                top[0].start_position().row,
                std::slice::from_ref(&file_context),
                &path_str,
            ) {
                chunks.push(chunk);
            }
        }

        // Each top-level section (and, recursively, its subsections) becomes a chunk.
        for node in top.iter().filter(|n| n.kind() == "section") {
            self.emit_md_section(
                *node,
                source,
                &path_str,
                std::slice::from_ref(&file_context),
                &mut chunks,
            );
        }

        if chunks.is_empty() {
            return Ok(bounded_fallback(self));
        }

        let source_lines: Vec<&str> = content.lines().collect();
        self.populate_context_windows(&mut chunks, &source_lines);

        let final_chunks = chunks
            .into_iter()
            .flat_map(|c| self.split_oversized(c))
            .collect();
        Ok(final_chunks)
    }

    /// Emit a chunk for one `section` node (heading + direct content), then recurse
    /// into nested subsections with an extended breadcrumb.
    fn emit_md_section(
        &self,
        section: Node,
        source: &[u8],
        path_str: &str,
        context_stack: &[String],
        chunks: &mut Vec<Chunk>,
    ) {
        let mut cursor = section.walk();
        let children: Vec<Node> = section.named_children(&mut cursor).collect();

        // Heading text (if the section opens with one) extends the breadcrumb.
        let heading_text = children
            .first()
            .filter(|c| Self::md_is_heading(c.kind()))
            .map(|h| Self::md_heading_text(*h, source))
            .unwrap_or_default();

        let mut new_context = context_stack.to_vec();
        if !heading_text.is_empty() {
            new_context.push(heading_text);
        }

        // Direct content = section start .. first nested subsection (exclusive).
        let first_sub = children.iter().find(|c| c.kind() == "section");
        let end_byte = first_sub.map_or_else(|| section.end_byte(), |s| s.start_byte());
        if let Some(chunk) = Self::md_chunk(
            source,
            section.start_byte(),
            end_byte,
            section.start_position().row,
            &new_context,
            path_str,
        ) {
            chunks.push(chunk);
        }

        for child in children.iter().filter(|c| c.kind() == "section") {
            self.emit_md_section(*child, source, path_str, &new_context, chunks);
        }
    }

    /// Build a Markdown chunk from a byte range, or None if it is blank.
    fn md_chunk(
        source: &[u8],
        start_byte: usize,
        end_byte: usize,
        start_line: usize,
        context: &[String],
        path_str: &str,
    ) -> Option<Chunk> {
        let text = std::str::from_utf8(source.get(start_byte..end_byte)?).ok()?;
        if text.trim().is_empty() {
            return None;
        }
        let line_count = text.lines().count().max(1);
        let mut chunk = Chunk::new(
            text.to_string(),
            start_line,
            start_line + line_count,
            ChunkKind::Block,
            path_str.to_string(),
        );
        chunk.context = context.to_vec();
        Some(chunk)
    }

    /// True if a node kind is a Markdown heading.
    fn md_is_heading(kind: &str) -> bool {
        kind == "atx_heading" || kind == "setext_heading"
    }

    /// Extract clean heading text (no `#` markers / underline).
    fn md_heading_text(node: Node, source: &[u8]) -> String {
        // atx_heading exposes the text via the `heading_content` field.
        if let Some(inline) = node.child_by_field_name("heading_content") {
            if let Ok(t) = inline.utf8_text(source) {
                return t.trim().to_string();
            }
        }
        // Fallback (e.g. setext_heading): first line, stripped of '#'.
        node.utf8_text(source)
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('#')
            .trim()
            .to_string()
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

            // Also mark preceding doc comments and attributes as covered
            // (they belong to this definition, not to a gap)
            let mut prev = node.prev_named_sibling();
            while let Some(sibling) = prev {
                let sib_kind = sibling.kind();
                if sib_kind == "line_comment"
                    || sib_kind == "block_comment"
                    || sib_kind == "attribute_item"
                    || sib_kind == "attribute"
                    || sib_kind == "decorator"
                {
                    if let Ok(text) = sibling.utf8_text(source) {
                        let text = text.trim();
                        // Only mark doc comments (///, //!, /**, /*!), attributes (#[...]),
                        // and decorators (@...) as covered — not regular comments
                        if text.starts_with("///")
                            || text.starts_with("//!")
                            || text.starts_with("/**")
                            || text.starts_with("/*!")
                            || text.starts_with("#[")
                            || text.starts_with("@")
                        {
                            gap_tracker.mark_covered(
                                sibling.start_position().row,
                                sibling.end_position().row,
                            );
                            prev = sibling.prev_named_sibling();
                            continue;
                        }
                    }
                    break;
                }
                break;
            }

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

        let path_str = normalize_path(path);
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

    /// Char- *and* line-aware splitter for unstructured text (Markdown/txt and the
    /// generic fallback). Unlike `split_if_needed`, which windows purely by line
    /// count, this also enforces `max_chunk_chars`: a single physical line longer
    /// than the char budget is hard-split on UTF-8 boundaries. This is what keeps
    /// scraped HTML/markdown — which can be one 80 KB line — from producing a single
    /// enormous chunk. The structured code path keeps using `split_if_needed`, so
    /// code chunking is unchanged.
    fn split_oversized(&self, chunk: Chunk) -> Vec<Chunk> {
        if chunk.line_count() <= self.max_chunk_lines && chunk.size_bytes() <= self.max_chunk_chars
        {
            return vec![chunk];
        }

        // 1. Expand into "units": one per line, but any line over the char budget is
        //    fragmented on char boundaries so no single unit exceeds max_chunk_chars.
        let mut units: Vec<String> = Vec::new();
        for line in chunk.content.lines() {
            if line.len() <= self.max_chunk_chars {
                units.push(line.to_string());
                continue;
            }
            let mut frag = String::new();
            for ch in line.chars() {
                if !frag.is_empty() && frag.len() + ch.len_utf8() > self.max_chunk_chars {
                    units.push(std::mem::take(&mut frag));
                }
                frag.push(ch);
            }
            if !frag.is_empty() {
                units.push(frag);
            }
        }

        if units.is_empty() {
            return vec![chunk];
        }

        // 2. Greedily window units, bounded by both max_chunk_lines and
        //    max_chunk_chars. Windows advance without overlap (context_prev/next
        //    already supply surrounding lines), so no content is duplicated.
        let mut out: Vec<Chunk> = Vec::new();
        let mut i = 0;
        let mut split_index = 0;
        while i < units.len() {
            let mut j = i;
            let mut char_count = 0usize;
            while j < units.len()
                && (j - i) < self.max_chunk_lines
                && (j == i || char_count + units[j].len() < self.max_chunk_chars)
            {
                char_count += units[j].len() + 1;
                j += 1;
            }
            let end = if j > i { j } else { i + 1 };

            let content = units[i..end].join("\n");
            let mut piece = Chunk::new(
                content,
                chunk.start_line + i,
                chunk.start_line + end,
                chunk.kind,
                chunk.path.clone(),
            );
            piece.context = chunk.context.clone();
            piece.signature = chunk.signature.clone();
            piece.is_complete = false;
            piece.split_index = Some(split_index);
            out.push(piece);

            split_index += 1;
            i = end;
        }

        // A single resulting piece means no real split happened — keep it whole.
        if out.len() == 1 {
            out[0].is_complete = true;
            out[0].split_index = None;
        }

        out
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
        let path_str = normalize_path(path);
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
                        let line_count = i - start;
                        let mut chunk = Chunk::new(gap_content, start, i, kind, path_str.clone());
                        chunk.context = context.clone();
                        chunk.signature = Some(Self::gap_signature(kind, line_count));
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
                let line_count = self.lines.len() - start;
                let mut chunk =
                    Chunk::new(gap_content, start, self.lines.len(), kind, path_str.clone());
                chunk.context = context.clone();
                chunk.signature = Some(Self::gap_signature(kind, line_count));
                gaps.push(chunk);
            }
        }

        gaps
    }

    /// Generate a descriptive signature for a gap chunk
    fn gap_signature(kind: ChunkKind, line_count: usize) -> String {
        match kind {
            ChunkKind::Imports => format!("imports ({} lines)", line_count),
            ChunkKind::ModuleDocs => format!("module docs ({} lines)", line_count),
            ChunkKind::Comment => format!("comment block ({} lines)", line_count),
            _ => format!("block ({} lines)", line_count),
        }
    }

    /// Classify what kind of gap this is
    fn classify_gap(content: &str) -> ChunkKind {
        let trimmed = content.trim();
        let total_lines = trimmed.lines().count();

        // Check if it's mostly imports
        let import_count = trimmed
            .lines()
            .filter(|line| {
                let line = line.trim();
                line.starts_with("import ")
                    || line.starts_with("from ")
                    || line.starts_with("use ")
                    || line.starts_with("using ")
                    || line.starts_with("#include")
            })
            .count();

        if total_lines > 0 && import_count > total_lines / 2 {
            return ChunkKind::Imports;
        }

        // Check if it's module-level docs
        if trimmed.starts_with("//!") || trimmed.starts_with("/*!") {
            return ChunkKind::ModuleDocs;
        }

        // Check if it's mostly comments (single-line or block)
        let comment_count = trimmed
            .lines()
            .filter(|line| {
                let line = line.trim();
                line.starts_with("//")
                    || line.starts_with("/*")
                    || line.starts_with("*")
                    || line.starts_with("#")  // Python/Shell comments
                    || line.is_empty() // Blank lines within comment blocks
            })
            .count();

        if total_lines > 0 && comment_count > total_lines / 2 {
            return ChunkKind::Comment;
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
    fn test_chunk_markdown_sections() {
        let mut chunker = SemanticChunker::new(100, 2000, 10);

        let md = "---\nsource: dam_help\ntitle: E-mail ordering\nurl: https://help.example.com/x\npath: dam_help/Ordering/EmailOrd\n---\n\n# E-mail ordering\n\nIntro paragraph about ordering.\n\n## Configure SMTP\n\nSteps to configure the mail server.\n\n## Troubleshooting\n\nFinal section text about errors.\n";

        let path = Path::new("EmailOrd.md");
        let chunks = chunker
            .chunk_semantic(Language::Markdown, path, md)
            .unwrap();

        // Preamble (front-matter) + h1 intro + 2 h2 sections = at least 4 chunks.
        assert!(
            chunks.len() >= 4,
            "Expected >=4 section chunks, got {}",
            chunks.len()
        );

        // No chunk should span the whole page: the "Configure SMTP" body and the
        // "Troubleshooting" body must live in *different* chunks.
        let smtp = chunks
            .iter()
            .find(|c| c.content.contains("Steps to configure"))
            .expect("should have a Configure SMTP chunk");
        assert!(
            !smtp.content.contains("Final section text"),
            "sections must not be merged into a whole-page block"
        );

        // Breadcrumb context must carry the heading path (document title + section).
        assert!(smtp.context.iter().any(|c| c.contains("E-mail ordering")));
        assert!(smtp.context.iter().any(|c| c.contains("Configure SMTP")));

        // Every chunk stays within the char budget.
        assert!(chunks.iter().all(|c| c.content.len() <= 2000));
    }

    #[test]
    fn test_chunk_markdown_nested_breadcrumb() {
        let mut chunker = SemanticChunker::new(100, 2000, 10);
        let md = "# Top\n\nlead\n\n## Middle\n\nmid body\n\n### Deep\n\ndeep body here\n";
        let chunks = chunker
            .chunk_semantic(Language::Markdown, Path::new("n.md"), md)
            .unwrap();

        let deep = chunks
            .iter()
            .find(|c| c.content.contains("deep body here"))
            .expect("should find deep section");
        // File > Top > Middle > Deep
        assert!(deep.context.iter().any(|c| c.contains("Top")));
        assert!(deep.context.iter().any(|c| c.contains("Middle")));
        assert!(deep.context.iter().any(|c| c.contains("Deep")));
        // The deep chunk must not contain its ancestors' bodies.
        assert!(!deep.content.contains("mid body"));
        assert!(!deep.content.contains("lead"));
    }

    #[test]
    fn test_chunk_markdown_oversized_section_split() {
        let mut chunker = SemanticChunker::new(100, 200, 5);
        let big_body = (0..50)
            .map(|i| format!("line of section body number {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let md = format!("# Heading\n\n{}\n", big_body);

        let chunks = chunker
            .chunk_semantic(Language::Markdown, Path::new("big.md"), &md)
            .unwrap();

        // A single >200-char section must be split into multiple bounded parts.
        assert!(chunks.len() > 1, "oversized section should be split");
        assert!(chunks.iter().any(|c| !c.is_complete));
    }

    #[test]
    fn test_chunk_markdown_hard_splits_long_line() {
        // Mirrors real-world scraped docs: a section whose body is ONE huge line
        // (no internal newlines). Line-based splitting can't bound this; the
        // char-aware splitter must.
        let mut chunker = SemanticChunker::new(100, 500, 10);
        let long_line = "word ".repeat(2000); // ~10_000 chars, single line
        let md = format!("# Title\n\n{}\n", long_line);

        let chunks = chunker
            .chunk_semantic(Language::Markdown, Path::new("huge.md"), &md)
            .unwrap();

        assert!(chunks.len() > 1, "a single 10KB line must be hard-split");
        assert!(
            chunks.iter().all(|c| c.content.len() <= 500),
            "every piece must respect the char budget; got max {}",
            chunks.iter().map(|c| c.content.len()).max().unwrap_or(0)
        );
    }

    #[test]
    fn test_chunk_markdown_no_headings_falls_back() {
        let mut chunker = SemanticChunker::new(100, 2000, 10);
        let md = "Just some plain text\nwith a few lines\nand no headings at all.\n";
        let chunks = chunker
            .chunk_semantic(Language::Markdown, Path::new("plain.txt"), md)
            .unwrap();

        assert!(!chunks.is_empty());
        // All content is preserved across chunks.
        let joined: String = chunks.iter().map(|c| c.content.clone()).collect();
        assert!(joined.contains("plain text"));
        assert!(joined.contains("no headings"));
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
