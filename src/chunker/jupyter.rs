//! Jupyter notebook (.ipynb) cell extraction for the semantic chunker.
//!
//! Parses `.ipynb` files (JSON) and extracts code/markdown cells as
//! individually searchable chunks.  Adjacent cells of the same type are
//! merged when their combined line count stays under 50 lines, keeping
//! the index compact without losing granularity.
//!
//! CAVEAT — line numbers are synthetic. `start_line`/`end_line` on the
//! emitted chunks are a running cursor over *cell content lines*, NOT the
//! real line/byte offset of the cell inside the `.ipynb` JSON (where each
//! cell's source is an array element surrounded by metadata/outputs). They
//! are display/ordering metadata only. Any feature that needs to map a chunk
//! back to a precise position in the raw `.ipynb` (jump-to-line, snippet
//! re-extraction, symbol references) must NOT trust these numbers — it would
//! have to compute the real JSON offsets during parsing.
//!
//! NOTE — all code cells are labelled generically (kernel language is not read
//! from `metadata.kernelspec`); search relevance for non-Python notebooks may
//! be slightly degraded. Follow-up if multi-kernel notebooks become common.

use super::{Chunk, ChunkKind};
use crate::cache::normalize_path;
use serde_json::Value;
use std::path::Path;
use tracing::warn;

/// Maximum number of lines a merged group of adjacent same-type cells may
/// reach before we stop merging and emit a chunk.
const MERGE_LINE_LIMIT: usize = 50;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Chunk a Jupyter notebook file into searchable units.
///
/// * `path`   – file path (used in context breadcrumbs).
/// * `content` – raw `.ipynb` file text (JSON).
///
/// Returns an empty `Vec` on malformed JSON (logs a warning).
pub fn chunk_jupyter(path: &Path, content: &str) -> Vec<Chunk> {
    let notebook: Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to parse Jupyter notebook {}: {e}", path.display());
            return Vec::new();
        }
    };

    let cells = match notebook["cells"].as_array() {
        Some(c) => c,
        None => {
            warn!(
                "No 'cells' array found in Jupyter notebook {}",
                path.display()
            );
            return Vec::new();
        }
    };

    let file_name = normalize_path(path);
    let file_breadcrumb = format!(
        "File: {}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );

    // Phase 1: extract structured cell data.
    let raw_cells: Vec<RawCell> = cells.iter().filter_map(extract_cell).collect();

    if raw_cells.is_empty() {
        return Vec::new();
    }

    // Phase 2: merge adjacent cells of the same type within the line limit.
    let groups = merge_adjacent_cells(&raw_cells);

    // Phase 3: build output chunks.
    groups
        .into_iter()
        .map(|g| build_chunk(&g, &file_name, &file_breadcrumb))
        .collect()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Lightweight representation of a single notebook cell.
struct RawCell {
    cell_type: String, // "code" or "markdown"
    content: String,
    line_count: usize,
}

/// A group of one or more adjacent cells of the same type that will form
/// a single output chunk.
struct CellGroup {
    cell_type: String,
    content: String,
    start_line: usize,
    end_line: usize,
}

/// Parse a single cell JSON value into a `RawCell`.
///
/// Returns `None` for unsupported cell types (e.g. "raw") or cells with
/// missing fields.
fn extract_cell(cell: &Value) -> Option<RawCell> {
    let cell_type = cell["cell_type"].as_str()?.to_string();

    // Only index code and markdown cells.
    if cell_type != "code" && cell_type != "markdown" {
        return None;
    }

    // Source can be either an array of strings (standard) or a single string
    // (rare: old IPython notebooks, some programmatic generators).
    let content: String = if let Some(arr) = cell["source"].as_array() {
        arr.iter().filter_map(|v| v.as_str()).collect()
    } else {
        cell["source"].as_str()?.to_string()
    };

    // Normalize to at least 1 line even for an empty cell, so the two passes
    // in merge_adjacent_cells (line numbering and merge accumulation) always
    // agree on a cell's width. (Previously empty cells stored 0, which the
    // numbering pass counted as 1 but the merge accumulator counted as 0.)
    let line_count = content.lines().count().max(1);

    Some(RawCell {
        cell_type,
        content,
        line_count,
    })
}

/// Merge adjacent cells of the same type when their combined line count
/// stays below [`MERGE_LINE_LIMIT`].
///
/// Large individual cells (> 50 lines) are always emitted as standalone
/// chunks — they are never merged with neighbours.
fn merge_adjacent_cells(cells: &[RawCell]) -> Vec<CellGroup> {
    let mut groups: Vec<CellGroup> = Vec::new();

    // First pass: assign global line numbers to each cell.
    let mut numbered: Vec<(usize, usize, &RawCell)> = Vec::with_capacity(cells.len());
    let mut cursor = 0usize;
    for cell in cells {
        let start = cursor;
        // line_count is normalized to >= 1 in extract_cell, so the numbering
        // pass and the merge accumulator below use the identical width.
        cursor += cell.line_count;
        numbered.push((start, cursor, cell));
    }

    // Second pass: merge adjacent same-type cells.
    let mut i = 0;
    while i < numbered.len() {
        let (start, _, cell) = &numbered[i];
        let group_start = *start;
        let cell_type = cell.cell_type.clone();

        // Accumulator for merged content.
        let mut merged_content = cell.content.clone();
        let mut merged_lines = cell.line_count;
        let mut group_end_idx = i;

        // A cell that is already over the limit on its own stays standalone.
        if cell.line_count <= MERGE_LINE_LIMIT {
            let mut j = i + 1;
            while j < numbered.len() {
                let (_, _, next_cell) = &numbered[j];
                if next_cell.cell_type != cell_type {
                    break;
                }
                let combined = merged_lines + next_cell.line_count;
                if combined > MERGE_LINE_LIMIT {
                    break;
                }
                merged_content.push('\n');
                merged_content.push_str(&next_cell.content);
                merged_lines = combined;
                group_end_idx = j;
                j += 1;
            }
        }

        let end = numbered[group_end_idx].1;
        groups.push(CellGroup {
            cell_type,
            content: merged_content,
            start_line: group_start,
            end_line: end,
        });

        i = group_end_idx + 1;
    }

    groups
}

/// Build a [`Chunk`] from a [`CellGroup`].
fn build_chunk(group: &CellGroup, file_name: &str, file_breadcrumb: &str) -> Chunk {
    // Prefix the cell type as a comment so the indexer/embedder can
    // distinguish code from markdown content.
    let prefixed = format!("# [{}]\n{}", group.cell_type, group.content);

    let mut chunk = Chunk::new(
        prefixed,
        group.start_line,
        group.end_line,
        ChunkKind::Block,
        file_name.to_string(),
    );

    chunk.context = vec![
        file_breadcrumb.to_string(),
        format!("[{}]", group.cell_type),
    ];

    chunk
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: minimal valid .ipynb JSON with the given cells array.
    fn make_notebook(cells_json: &str) -> String {
        format!(
            r#"{{"cells": {cells_json}, "metadata": {{}}, "nbformat": 4, "nbformat_minor": 5}}"#
        )
    }

    #[test]
    fn test_empty_notebook_returns_empty() {
        let nb = make_notebook("[]");
        let chunks = chunk_jupyter(Path::new("test.ipynb"), &nb);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_malformed_json_returns_empty() {
        let chunks = chunk_jupyter(Path::new("bad.ipynb"), "this is not json!!!");
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_single_code_cell() {
        let nb =
            make_notebook(r#"[{"cell_type":"code","source":["print('hello')\n"],"metadata":{}}]"#);
        let chunks = chunk_jupyter(Path::new("test.ipynb"), &nb);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.starts_with("# [code]\n"));
        assert!(chunks[0].content.contains("print('hello')"));
        assert_eq!(chunks[0].kind, ChunkKind::Block);
        assert!(chunks[0].context[1].contains("code"));
    }

    #[test]
    fn test_single_markdown_cell() {
        let nb =
            make_notebook(r##"[{"cell_type":"markdown","source":["# Title\n"],"metadata":{}}]"##);
        let chunks = chunk_jupyter(Path::new("note.ipynb"), &nb);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.starts_with("# [markdown]\n"));
    }

    #[test]
    fn test_adjacent_same_type_merged() {
        // Two tiny code cells (< 50 lines each) should merge into one chunk.
        let nb = make_notebook(
            r#"[
                {"cell_type":"code","source":["x = 1\n"],"metadata":{}},
                {"cell_type":"code","source":["y = 2\n"],"metadata":{}}
            ]"#,
        );
        let chunks = chunk_jupyter(Path::new("test.ipynb"), &nb);
        assert_eq!(chunks.len(), 1, "adjacent small code cells should merge");
        assert!(chunks[0].content.contains("x = 1"));
        assert!(chunks[0].content.contains("y = 2"));
    }

    #[test]
    fn test_different_types_not_merged() {
        let nb = make_notebook(concat!(
            r"[",
            r#"{"cell_type":"code","source":["x = 1\n"],"metadata":{}}"#,
            ",",
            r##"{"cell_type":"markdown","source":["# intro\n"],"metadata":{}}"##,
            "]"
        ));
        let chunks = chunk_jupyter(Path::new("test.ipynb"), &nb);
        assert_eq!(chunks.len(), 2, "different cell types should not merge");
        assert!(chunks[0].content.starts_with("# [code]"));
        assert!(chunks[1].content.starts_with("# [markdown]"));
    }

    #[test]
    fn test_large_cell_stays_standalone() {
        // Build a code cell with > 50 lines.
        let lines: Vec<String> = (0..60).map(|i| format!("line_{i}\n")).collect();
        let source_json: Vec<String> = lines
            .iter()
            .map(|l| format!("\"{}\"", l.replace('\n', "\\n")))
            .collect();
        let nb = make_notebook(&format!(
            r#"[{{"cell_type":"code","source":[{source}], "metadata":{{}}}}]"#,
            source = source_json.join(",")
        ));
        let chunks = chunk_jupyter(Path::new("big.ipynb"), &nb);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("line_0"));
        assert!(chunks[0].content.contains("line_59"));
    }

    #[test]
    fn test_raw_cells_filtered_out() {
        let nb = make_notebook(
            r#"[
                {"cell_type":"raw","source":["ignored"],"metadata":{}},
                {"cell_type":"code","source":["x = 1\n"],"metadata":{}}
            ]"#,
        );
        let chunks = chunk_jupyter(Path::new("test.ipynb"), &nb);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("x = 1"));
    }

    #[test]
    fn test_no_cells_array_returns_empty() {
        let nb = r#"{"metadata": {}, "nbformat": 4}"#;
        let chunks = chunk_jupyter(Path::new("empty.ipynb"), nb);
        assert!(chunks.is_empty());
    }
}
