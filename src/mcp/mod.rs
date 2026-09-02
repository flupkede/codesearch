//! MCP (Model Context Protocol) server for Claude Code integration
//!
//! Exposes codesearch's semantic search capabilities via the MCP protocol,
//! allowing AI assistants like Claude to search codebases during conversations.
//!
//! # Important: No Stdout Output
//!
//! The MCP module MUST NOT use `print!` or `println!` macros anywhere in its code.
//! All non-JSON output must go to stderr via `info_print!`, `warn_print!`, or `eprintln!`.
//! This is critical because the MCP protocol communicates over stdout via JSON-RPC,
//! and any stdout pollution will break the protocol.

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

pub mod types;

/// Resolve the serve base URL from env or default host/port.
fn serve_url_from_env() -> String {
    use crate::constants::resolve_serve_host;
    let host = resolve_serve_host();
    let port = std::env::var(crate::constants::SERVE_PORT_ENV)
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(crate::constants::DEFAULT_SERVE_PORT);
    format!("http://{}:{}", host, port)
}

use crate::db_discovery::{find_best_database, load_repos_config};
use crate::embed::{EmbeddingService, ModelType};
use crate::file::Language;
use crate::fts::FtsStore;
use crate::index::SharedStores;
use crate::rerank::{rrf_fusion, rrf_fusion_with_exact, vector_only, EXACT_MATCH_RRF_K};
use crate::search::{adapt_rrf_k, boost_kind, detect_identifiers, detect_structural_intent};
use crate::symbols::SymbolIndexerRegistry;
use crate::vectordb::VectorStore;
use anyhow::{Context, Result};
use regex::Regex;
use rmcp::{
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// Re-export types
pub use types::*;

mod explore;
mod federation_helpers;
mod find;
mod find_impact;
mod get_chunk;
mod graph;
mod helpers;
mod instructions;
mod literal_search;
mod proxy;
mod responses;
mod runtime;
mod search;
mod status;

// Re-export the extracted modules' items so every existing path keeps working:
// `super::X` from sibling test files and `crate::mcp::X` from serve/cli.
pub(crate) use federation_helpers::*;
pub(crate) use helpers::*;
pub(crate) use instructions::*;
pub(crate) use responses::*;
pub use runtime::*;

/// Re-order lexical `usages` hits so source-code paths come before everything
/// else (docs, configs, markdown). Stable: score order is preserved within
/// each group, so this only demotes non-code noise, it never re-ranks code.
fn rank_code_first(items: &mut [ReferenceItem]) {
    items.sort_by_key(|item| !is_source_path(&item.path));
}

/// True when the path looks like source code rather than docs/config. Used
/// only to re-order lexical `find(kind="usages")` hits code-first — never to
/// filter them: a markdown hit is noise the agent can discard, a missing hit
/// would be a silent false negative.
fn is_source_path(path: &str) -> bool {
    const SOURCE_EXTS: &[&str] = &[
        "rs", "py", "go", "cs", "ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs", "java", "kt",
        "kts", "swift", "c", "h", "cpp", "hpp", "cc", "cxx", "hh", "rb", "php", "proto", "scala",
        "dart",
    ];
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| SOURCE_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Agent-facing note for lexical `find(kind="usages")` results.
///
/// Returns `Some(note)` only when BOTH hold: the hits actually include
/// SCIP-backed source files (C#/TypeScript) AND a matching symbol indexer is
/// installed and available — precisely the case where the lexical list is a
/// lossy stand-in for `find_impact`'s precise references. Any other
/// combination returns `None`, keeping the legacy response shape
/// byte-identical (no nagging where the advice cannot be acted on).
fn scip_usages_note(
    registry: &SymbolIndexerRegistry,
    items: &[ReferenceItem],
    symbol: &str,
) -> Option<String> {
    const SCIP_BACKED_EXTS: &[&str] = &["cs", "ts", "tsx", "mts", "cts"];
    let has_backed_source = items.iter().any(|item| {
        Path::new(&item.path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| SCIP_BACKED_EXTS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false)
    });
    if !has_backed_source {
        return None;
    }
    let backend = [
        crate::constants::LANG_CSHARP,
        crate::constants::LANG_TYPESCRIPT,
    ]
    .into_iter()
    .find(|lang| {
        registry
            .get(lang)
            .is_some_and(|indexer| indexer.is_available())
    })?;
    Some(format!(
        "lexical text matching — hits may be docs/comments rather than code references; \
         for precise SCIP call-sites use find_impact (symbol_name='{symbol}', project=...) \
         (backend: {backend})"
    ))
}

/// Codesearch MCP service
pub struct CodesearchService {
    #[allow(dead_code)]
    tool_router: ToolRouter<CodesearchService>,
    db_path: PathBuf,
    project_path: PathBuf,
    model_type: ModelType,
    dimensions: usize,
    // Lazily initialized on first search
    embedding_service: Arc<Mutex<Option<EmbeddingService>>>,
    // Shared stores for concurrent access (optional - only set when running with IndexManager)
    shared_stores: Option<Arc<SharedStores>>,
    // Serve-mode state (set when running inside `codesearch serve`)
    serve_state: Option<Arc<crate::serve::ServeState>>,
    // Shared symbol indexer registry — reused across MCP sessions to preserve
    // helper-detection cache. In serve mode, cloned from ServeState; in
    // standalone mode, a locally owned Arc.
    symbol_registry: Arc<SymbolIndexerRegistry>,
    // True ONLY for services created by the serve-mode MCP session factory,
    // which pairs `session_connected()` (on create) with `session_disconnected()`
    // (in Drop). Per-request REST services built via `make_service` leave this
    // false so `Drop` does NOT decrement `active_sessions` — otherwise that
    // AtomicU64 underflows (0 - 1 wraps to u64::MAX) on every REST request,
    // corrupting the `/status` health signal.
    tracks_session: bool,
}

impl std::fmt::Debug for CodesearchService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodesearchService")
            .field("db_path", &self.db_path)
            .field("model_type", &self.model_type)
            .field("dimensions", &self.dimensions)
            .field("has_shared_stores", &self.shared_stores.is_some())
            .field("serve_mode", &self.serve_state.is_some())
            .finish()
    }
}

impl Drop for CodesearchService {
    fn drop(&mut self) {
        // Only genuine MCP sessions (created by the serve factory, which calls
        // session_connected() + mark_session_tracked()) balance the counter.
        // Per-request REST services (make_service → new_for_serve) never
        // increment it, so must NOT decrement here — otherwise active_sessions
        // underflows (the AtomicU64 wraps to u64::MAX) on every REST request.
        if self.tracks_session {
            if let Some(ref serve_state) = self.serve_state {
                serve_state.session_disconnected();
            }
        }
    }
}

// v1: supports prefix/suffix patterns with `*` and `**` only.
/// Merge exact FTS results into the main result set, deduplicating by chunk_id
/// and keeping the max score for duplicates.
///
/// This is the pure logic extracted from `semantic_search_lexical` for testability.
fn merge_exact_into_fts(
    fts_results: &mut Vec<crate::fts::FtsResult>,
    exact: Vec<crate::fts::FtsResult>,
) {
    let mut positions: std::collections::HashMap<u32, usize> = fts_results
        .iter()
        .enumerate()
        .map(|(idx, r)| (r.chunk_id, idx))
        .collect();

    for r in exact {
        if let Some(&existing_idx) = positions.get(&r.chunk_id) {
            fts_results[existing_idx].score = fts_results[existing_idx].score.max(r.score);
        } else {
            positions.insert(r.chunk_id, fts_results.len());
            fts_results.push(r);
        }
    }
}

/// Compute low-confidence signaling based on the top result's score.
///
/// Returns `(low_confidence, suggested_tool)` where both are `None` when
/// confidence is high (score >= threshold).
fn compute_low_confidence(
    top_score: Option<f32>,
    has_identifiers: bool,
) -> (Option<bool>, Option<String>) {
    match top_score {
        Some(score) if score < LOW_CONFIDENCE_THRESHOLD => {
            let suggestion = if has_identifiers {
                "find_definition"
            } else {
                "literal_search"
            };
            (Some(true), Some(suggestion.to_string()))
        }
        Some(_) => (None, None),
        None => (Some(true), Some("literal_search".to_string())),
    }
}

// Full glob syntax deferred to avoid adding new dependencies.

/// Match a file path against a simple glob pattern.
///
/// Supported patterns:
/// - `src/mcp/**` → any path starting with `src/mcp/`
/// - `**/*.rs` → any path ending with `.rs`
/// - `src/**/*.rs` → path starting with `src/` and ending with `.rs`
/// - `*.rs` → any path ending with `.rs` (single `*` within a segment)
/// - `foo.rs` → exact match
fn simple_glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let path = path.replace('\\', "/");

    if !pattern.contains('*') {
        // Exact match
        return path == pattern;
    }

    if pattern.contains("**") {
        // Split on first ** only
        let parts: Vec<&str> = pattern.splitn(2, "**").collect();
        let prefix = parts[0];
        // Strip leading / from suffix since ** already matches the separator
        let suffix = parts
            .get(1)
            .map(|s| s.strip_prefix('/').unwrap_or(s))
            .unwrap_or("");

        let mut p = path.as_str();
        if !prefix.is_empty() && !p.starts_with(prefix) {
            return false;
        }
        if !prefix.is_empty() {
            p = &p[prefix.len()..];
        }
        // Strip leading / from remaining path (since ** can match empty + /)
        if p.starts_with('/') {
            p = &p[1..];
        }
        if suffix.is_empty() {
            return true;
        }
        // The suffix may contain single * — match against the tail of the path.
        // After **, the suffix describes constraints on the end of the path.
        // For `**/*.rs`, the `*.rs` should match the last segment.
        if suffix.contains('*') {
            // Match suffix against the end of the path using segment-aware logic
            return match_suffix_with_star(suffix, p);
        }
        p.ends_with(suffix)
    } else {
        // Pure single-star pattern (no **)
        simple_glob_match_single_star(&pattern, &path)
    }
}

/// Match a suffix pattern (containing `*`) against the end of a path.
/// The `*` matches within a single segment only.
///
/// E.g., suffix `*.rs` matches `src/main.rs` because the last segment `main.rs` ends with `.rs`.
fn match_suffix_with_star(suffix: &str, path: &str) -> bool {
    // Find the segments in the suffix (split by /)
    let suffix_parts: Vec<&str> = suffix.split('/').collect();
    let path_segments: Vec<&str> = path.split('/').collect();

    // The suffix must match the last N segments of the path
    if suffix_parts.len() > path_segments.len() {
        return false;
    }

    let path_tail = &path_segments[path_segments.len() - suffix_parts.len()..];

    for (sp, pp) in suffix_parts.iter().zip(path_tail.iter()) {
        if sp.contains('*') {
            if !single_segment_match(sp, pp) {
                return false;
            }
        } else if *sp != *pp {
            return false;
        }
    }
    true
}

/// Match a single segment pattern against a single segment path part.
/// `*` matches any characters within the segment.
fn single_segment_match(pattern: &str, segment: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut s = segment;

    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !s.starts_with(part) {
                return false;
            }
            s = &s[part.len()..];
        } else if i == parts.len() - 1 {
            if !s.ends_with(part) {
                return false;
            }
        } else if let Some(pos) = s.find(part) {
            s = &s[pos + part.len()..];
        } else {
            return false;
        }
    }
    true
}

/// Match a single-star glob pattern where `*` matches any characters except `/`.
fn simple_glob_match_single_star(pattern: &str, path: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut p = path;

    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // First part must be a prefix
            if !p.starts_with(part) {
                return false;
            }
            p = &p[part.len()..];
        } else if i == parts.len() - 1 {
            // Last part must be a suffix of the CURRENT segment (after *)
            // * does not cross /, so find the end of the current segment
            let seg_end = p.find('/').unwrap_or(p.len());
            let segment = &p[..seg_end];
            if !segment.ends_with(part) {
                return false;
            }
        } else {
            // Middle parts: find within remaining path but NOT across /
            if let Some(pos) = p.find(part) {
                let before = &p[..pos];
                if before.contains('/') {
                    return false;
                }
                p = &p[pos + part.len()..];
            } else {
                return false;
            }
        }
    }
    true
}

fn is_import_kind(kind: &str) -> bool {
    matches!(kind, "Import" | "Use" | "Require" | "Include" | "Imports")
}

/// Common import-keyword literals used by the FTS fallback when no import-kind
/// chunks are found via vector-store lookup.
const IMPORT_FTS_KEYWORDS: &[&str] = &["import", "use", "using", "from", "require", "include"];

fn truncate_line_around_match(line: &str, match_start_byte: usize, max_chars: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= max_chars {
        return line.to_string();
    }

    let match_char_idx = line[..match_start_byte.min(line.len())].chars().count();
    let half = max_chars / 2;
    let mut start = match_char_idx.saturating_sub(half);
    let end = (start + max_chars).min(chars.len());
    if end - start < max_chars {
        start = end.saturating_sub(max_chars);
    }

    chars[start..end].iter().collect()
}

fn match_line_for_literal(
    content: &str,
    query: &str,
    regex: Option<&Regex>,
) -> Option<(usize, String)> {
    if query.is_empty() {
        return None;
    }

    for (idx, line) in content.lines().enumerate() {
        if let Some(re) = regex {
            if let Some(m) = re.find(line) {
                let snippet = truncate_line_around_match(line, m.start(), 200);
                return Some((idx, snippet));
            }
        } else if let Some(pos) = line.find(query) {
            let snippet = truncate_line_around_match(line, pos, 200);
            return Some((idx, snippet));
        }
    }

    None
}

/// Returns true when a regex pattern contains at least one run of three or more
/// alphanumerics-or-underscore characters. Such a run is enough for Tantivy's
/// analyzer to produce a real BM25 token, which means the BM25 candidate path
/// will work for this query.
///
/// When this returns false, the regex is "tokenless" — it consists only of
/// regex syntax (\b, \s, \w, ^, $, character classes, anchors). BM25 has
/// nothing to match on, so the caller must fall back to a full chunk scan.
///
/// Conservative direction: false positives ("looks anchorable, isn't really")
/// are safe because the BM25 path will return empty candidates and the regex
/// post-filter will return empty results — same outcome as the scan path
/// would on a corpus with no matches. False negatives ("looks tokenless,
/// actually has tokens") are unsafe because they trigger an unnecessary scan.
/// We bias toward false positives.
fn regex_has_anchorable_token(pattern: &str) -> bool {
    let mut run: usize = 0;
    let mut need_separator = false;
    let mut i = 0;
    let bytes = pattern.as_bytes();
    while i < bytes.len() {
        let c = bytes[i] as char;
        // Skip the next char after a backslash — it's an escape, not content.
        // This prevents \w, \s, \b, \d etc. from contributing to the run count.
        if c == '\\' && i + 1 < bytes.len() {
            run = 0;
            need_separator = true; // chars after escape are merged by BM25 tokenizer
            i += 2;
            continue;
        }
        // Character classes [abc] don't anchor BM25 either — the tokens inside
        // are alternatives, not a contiguous string. Skip the whole class.
        if c == '[' {
            run = 0;
            need_separator = true;
            // Find matching ]; tolerate \] inside.
            let mut j = i + 1;
            while j < bytes.len() {
                let cj = bytes[j] as char;
                if cj == '\\' && j + 1 < bytes.len() {
                    j += 2;
                    continue;
                }
                if cj == ']' {
                    break;
                }
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            if need_separator {
                // After \X or [...], BM25 merges the next alphanumeric run with
                // the escape/class content (e.g. \bimpl → "bimpl", not "impl").
                // So we skip these chars — they're not independent tokens.
                i += 1;
                continue;
            }
            run += 1;
            if run >= 3 {
                // Only peek when the run might be ending: check if the next byte
                // is NOT alphanumeric. If it IS, keep building the run.
                let next_idx = i + 1;
                let run_continues = next_idx < bytes.len() && {
                    let nc = bytes[next_idx] as char;
                    nc.is_alphanumeric() || nc == '_'
                };
                if !run_continues {
                    // Run has ended. Check if next byte merges (escape or class).
                    if next_idx < bytes.len() {
                        let next_c = bytes[next_idx] as char;
                        if next_c == '\\' || next_c == '[' {
                            run = 0;
                            need_separator = true;
                            i += 1;
                            continue;
                        }
                    }
                    // Run ended naturally (EOF or non-merge separator) → anchorable
                    return true;
                }
                // Run continues — keep building in next iteration
            }
        } else {
            run = 0;
            need_separator = false;
        }
        i += 1;
    }
    false
}

/// Extracts a clean BM25 query string from a regex pattern.
///
/// When `regex=true` and the BM25 path is used, we can't pass the raw regex
/// (e.g. `class \w+Cache\b`) to Tantivy — it tokenizes poorly on backslashes
/// and metacharacters, producing useless candidates. Instead, this function
/// extracts only the literal alphanumeric runs from the pattern and joins them
/// with spaces, producing a clean BM25 query (e.g. `class Cache`).
///
/// The regex post-filter (`match_line_for_literal`) then correctly filters
/// the BM25 candidates against the actual regex pattern.
fn extract_bm25_query_from_regex(pattern: &str) -> String {
    let mut tokens: Vec<&str> = Vec::new();
    let bytes = pattern.as_bytes();
    let mut i = 0;
    let mut run_start: Option<usize> = None;

    while i < bytes.len() {
        let c = bytes[i] as char;
        // Skip escaped characters (\w, \b, \d, etc.)
        if c == '\\' && i + 1 < bytes.len() {
            if run_start.is_some() {
                let token = &pattern[run_start.unwrap()..i];
                if token.len() >= 2 {
                    tokens.push(token);
                }
                run_start = None;
            }
            i += 2;
            continue;
        }
        // Skip character classes [abc]
        if c == '[' {
            if run_start.is_some() {
                let token = &pattern[run_start.unwrap()..i];
                if token.len() >= 2 {
                    tokens.push(token);
                }
                run_start = None;
            }
            let mut j = i + 1;
            while j < bytes.len() {
                let cj = bytes[j] as char;
                if cj == '\\' && j + 1 < bytes.len() {
                    j += 2;
                    continue;
                }
                if cj == ']' {
                    break;
                }
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else if run_start.is_some() {
            let token = &pattern[run_start.unwrap()..i];
            if token.len() >= 2 {
                tokens.push(token);
            }
            run_start = None;
        }
        i += 1;
    }
    // Flush trailing run
    if let Some(start) = run_start {
        let token = &pattern[start..];
        if token.len() >= 2 {
            tokens.push(token);
        }
    }
    tokens.join(" ")
}

/// Returns true when a regex pattern contains a top-level alternation (`|`)
/// that is NOT inside a group `(...)` or character class `[...]`.
///
/// BM25 treats a query like `TODO|FIXME|HACK` as a conjunction of all tokens
/// (`TODO AND FIXME AND HACK`), which returns 0 results because no single chunk
/// contains all three. The regex post-filter would then discard everything.
/// Detecting top-level `|` lets us fall back to the scan path, which applies the
/// regex correctly (matching any alternative per chunk).
///
/// Escaped pipes (`\|`) are ignored.
fn regex_has_disjunctive_or(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut depth_paren = 0u32; // nesting depth of (...)
    let mut in_bracket = false; // inside [...]
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        // Skip escaped char
        if c == '\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if in_bracket {
            if c == ']' {
                in_bracket = false;
            }
            i += 1;
            continue;
        }
        match c {
            '[' => {
                in_bracket = true;
            }
            '(' => {
                depth_paren += 1;
            }
            ')' => {
                depth_paren = depth_paren.saturating_sub(1);
            }
            '|' => return depth_paren == 0,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Returns true when a literal-search query looks like a code pattern whose
/// punctuation would be destroyed by BM25 tokenization.
///
/// Triggers on:
/// - Multi-char operators: ->, =>, ::, !=, ==, <=, >=, &&, ||, <<, >>
/// - Space-surrounded single operators: " = ", " < ", " > "
/// - Statement endings: trailing `;` or `{`
/// - ≥ 2 angle/square bracket characters: `Vec<T>`, `[0]`
///
/// Does NOT trigger on:
/// - Plain identifiers: "ActivitiesListModelResponse", "foo_bar"
/// - Dotted paths: "foo.bar", "System.Console"
/// - Single parens alone: "(error)" — parens are not in the bracket set
fn looks_like_code_pattern(query: &str) -> bool {
    const MULTI_OPS: &[&str] = &[
        "->", "=>", "::", "!=", "==", "<=", ">=", "&&", "||", "<<", ">>",
    ];
    if MULTI_OPS.iter().any(|op| query.contains(op)) {
        return true;
    }
    const SPACED_OPS: &[&str] = &[" = ", " < ", " > "];
    if SPACED_OPS.iter().any(|op| query.contains(op)) {
        return true;
    }
    let trimmed = query.trim();
    if trimmed.ends_with(';') || trimmed.ends_with('{') {
        return true;
    }
    let bracket_count = query
        .chars()
        .filter(|c| matches!(c, '<' | '>' | '[' | ']'))
        .count();
    bracket_count >= 2
}

/// BM25 score threshold for low-confidence signalling in literal search.
///
/// Scores **below** this threshold trigger `low_confidence: true` in the
/// response. Tantivy BM25 scores in the codesearch corpus typically range
/// from ~5 (weak match) to ~50+ (strong match), so 5.0 is a conservative
/// initial floor — below this, results are likely noise rather than real hits.
///
/// To recalibrate: enable `RUST_LOG=codesearch::literal_confidence=debug`,
/// collect query/score samples, set this to roughly the 25th percentile of
/// real query scores.
const LITERAL_LOW_CONFIDENCE_BM25: f32 = 5.0;

fn compute_literal_low_confidence(
    top_score: Option<f32>,
    query: &str,
) -> (Option<bool>, Option<String>) {
    let word_count = query.split_whitespace().count();
    let has_code_chars = query.chars().any(|c| "{}[]<>=|;:".contains(c));
    let is_natural_language = word_count >= 3 && !has_code_chars;
    // A single identifier with no spaces: trust results even when BM25 score is low.
    // BM25 scores are unreliable for identifiers that tokenise into common sub-words
    // (e.g. `regex_has_disjunctive_or` → `or` has near-zero IDF and drags the score
    // below the floor even when the match is correct).
    let is_single_identifier = word_count == 1 && !has_code_chars;

    let suggest_semantic = "search with mode='semantic'";
    let suggest_regex = "search with mode='literal' and regex=true";
    let suggest_find = "find with kind='definition' or kind='usages'";

    match top_score {
        Some(score) if score < LITERAL_LOW_CONFIDENCE_BM25 => {
            if is_single_identifier {
                // Results exist for a single-word identifier: low BM25 score is an
                // IDF artefact, not a quality signal. Trust the results.
                return (None, None);
            }
            let hint = if is_natural_language {
                suggest_semantic
            } else {
                suggest_find
            };
            (Some(true), Some(hint.to_string()))
        }
        None => {
            let hint = if is_natural_language {
                suggest_semantic
            } else {
                suggest_regex
            };
            (Some(true), Some(hint.to_string()))
        }
        Some(_) => (None, None),
    }
}

/// Parse individual import statements from chunk content.
///
/// Handles: `use`, `import`, `from ... import`, `#include`, `require(...)`.
/// Limitation: multi-line imports (e.g. Python `from X import (\n  a,\n  b\n)`)
/// are only partially captured — the first line is matched, continuation lines
/// are missed. Acceptable for v1; a proper AST-based approach would require
/// changes to the chunker.
fn parse_import_lines(content: &str, start_line: usize) -> Vec<ImportItem> {
    let mut items = Vec::new();

    for (offset, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let parsed = if let Some(rest) = trimmed.strip_prefix("use ") {
            Some((
                "use".to_string(),
                rest.trim().trim_end_matches(';').to_string(),
            ))
        } else if let Some(rest) = trimmed.strip_prefix("using ") {
            // C# using directive — skip `using (...)` statements and `using var` declarations
            if rest.starts_with('(') || rest.starts_with("var ") {
                None
            } else {
                Some((
                    "using".to_string(),
                    rest.trim().trim_end_matches(';').to_string(),
                ))
            }
        } else if let Some(rest) = trimmed.strip_prefix("import ") {
            Some((
                "import".to_string(),
                rest.trim().trim_end_matches(';').to_string(),
            ))
        } else if let Some(rest) = trimmed.strip_prefix("from ") {
            Some((
                "import".to_string(),
                rest.trim().trim_end_matches(';').to_string(),
            ))
        } else if trimmed.starts_with("#include") {
            Some((
                "include".to_string(),
                trimmed
                    .trim_start_matches("#include")
                    .trim()
                    .trim_end_matches(';')
                    .to_string(),
            ))
        } else if trimmed.contains("require(") {
            Some(("require".to_string(), trimmed.to_string()))
        } else {
            None
        };

        if let Some((kind, imported)) = parsed {
            items.push(ImportItem {
                imported,
                line: start_line + offset,
                kind,
            });
        }
    }

    items
}

// === Tool Router Implementation ===

#[tool_router]
impl CodesearchService {
    /// Create a new CodesearchService (standalone mode - opens its own VectorStore)
    #[allow(dead_code)] // Reserved for standalone MCP server mode
    pub fn new(requested_path: Option<PathBuf>) -> Result<Self> {
        Self::new_with_stores(requested_path, None)
    }

    /// Create a new CodesearchService with shared stores (for use with IndexManager)
    pub fn new_with_stores(
        requested_path: Option<PathBuf>,
        shared_stores: Option<Arc<SharedStores>>,
    ) -> Result<Self> {
        // Find the best database to use
        let db_info = find_best_database(requested_path.as_deref())?;

        if db_info.is_none() {
            return Err(anyhow::anyhow!(
                "No database found in current directory, parent directories, or globally tracked repositories. \
                 Run 'codesearch index' first to index the codebase."
            ));
        }

        let db_info = db_info.unwrap();
        let db_path = db_info.db_path;
        let project_path = db_info.project_path;

        // Read model metadata from database
        let metadata_path = db_path.join("metadata.json");
        let (model_type, dimensions) = if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path)?;
            let json: serde_json::Value = serde_json::from_str(&content)?;
            let model_name = json
                .get("model_short_name")
                .and_then(|v| v.as_str())
                .unwrap_or("minilm-l6");
            let dims = json
                .get("dimensions")
                .and_then(|v| v.as_u64())
                .unwrap_or(crate::constants::DEFAULT_EMBEDDING_DIMENSIONS as u64)
                as usize;
            let mt = ModelType::parse(model_name).unwrap_or_default();
            (mt, dims)
        } else {
            (
                ModelType::default(),
                crate::constants::DEFAULT_EMBEDDING_DIMENSIONS,
            )
        };

        Ok(Self {
            tool_router: Self::merged_tool_router(),
            db_path,
            project_path,
            model_type,
            dimensions,
            embedding_service: Arc::new(Mutex::new(None)),
            shared_stores,
            serve_state: None,
            symbol_registry: Arc::new(SymbolIndexerRegistry::new()),
            tracks_session: false,
        })
    }

    /// Create a CodesearchService for use inside `codesearch serve`.
    ///
    /// In serve mode, the service does not have a single local DB; instead
    /// it routes requests to the repo identified by `project`/`group`.
    pub(crate) fn new_for_serve(serve_state: Arc<crate::serve::ServeState>) -> Result<Self> {
        let symbol_registry = serve_state.symbol_registry();
        Ok(Self {
            tool_router: Self::merged_tool_router(),
            db_path: PathBuf::from("serve://multi-repo"),
            project_path: PathBuf::from("serve://multi-repo"),
            model_type: ModelType::default(),
            dimensions: crate::constants::DEFAULT_EMBEDDING_DIMENSIONS,
            embedding_service: serve_state.embedding_service(),
            shared_stores: None,
            serve_state: Some(serve_state),
            symbol_registry,
            tracks_session: false,
        })
    }

    /// Mark this service as owning a session slot so `Drop` will balance the
    /// `session_connected()` the caller already made.
    ///
    /// Only the serve-mode MCP session factory (`run_serve`) should call this:
    /// it calls `session_connected()` and then this. Per-request REST services
    /// built via `make_service` must NOT — they never increment the counter, so
    /// decrementing it on drop would underflow `active_sessions` to `u64::MAX`.
    pub(crate) fn mark_session_tracked(&mut self) {
        self.tracks_session = true;
    }

    /// Get or initialize the embedding service
    fn get_embedding_service(&self) -> Result<std::sync::MutexGuard<'_, Option<EmbeddingService>>> {
        let mut guard = self.embedding_service.lock().unwrap();
        if guard.is_none() {
            let cache_dir = crate::constants::get_global_models_cache_dir()?;
            *guard = Some(EmbeddingService::with_cache_dir(
                self.model_type,
                Some(&cache_dir),
            )?);
        }
        Ok(guard)
    }

    /// Return the current MCP mode as a string for diagnostics.
    fn mcp_mode(&self) -> Option<String> {
        if self.serve_state.is_some() {
            Some("serve_hub".to_string())
        } else {
            Some("stdio".to_string())
        }
    }

    /// Check if database exists and return error if not
    fn ensure_database_exists(&self) -> Result<(), String> {
        if !self.db_path.exists() {
            return Err(format!(
                "❌ No index database found at: {}\n\n\
                 ⚠️  IMPORTANT: This MCP server cannot index the codebase itself. Indexing takes 30-60 seconds and must be done manually.\n\n\
                 To fix this, run the following command in your terminal:\n\
                 $ cd {}\n\
                 $ codesearch index\n\n\
                 For more information about database locations, use the `status` tool with `kind=\"projects\"`.",
                self.db_path.display(),
                self.project_path.display()
            ));
        }
        Ok(())
    }

    /// Resolve project/group parameters to a specific `Arc<SharedStores>`.
    ///
    /// For groups with multiple members, only the first store is returned.
    /// Use `resolve_repo_stores_multi` for full group fan-out.
    ///
    /// Returns:
    /// Resolve project/group parameters to all matching `Arc<SharedStores>`.
    ///
    /// For groups, returns stores for ALL group members (fan-out).
    /// For project, returns a single-element vec.
    ///
    /// Returns:
    /// - `Ok(None)` — no project/group specified, use default local stores
    /// - `Ok(Some(vec))` — one or more stores to query (fan out and merge)
    /// - `Err(msg)` — validation error
    async fn resolve_repo_stores_multi(
        &self,
        project: &Option<String>,
        group: &Option<String>,
        allow_unscoped: bool,
    ) -> std::result::Result<Option<(Vec<Arc<SharedStores>>, Vec<String>)>, String> {
        // No routing params → resolve based on repo count
        if project.is_none() && group.is_none() {
            if let Some(ref serve_state) = self.serve_state {
                let cfg = serve_state.config_snapshot();
                let aliases: Vec<String> = cfg.repos.keys().cloned().collect();
                if aliases.len() > 1 && !allow_unscoped {
                    // Multi-repo: reject fan-out, require explicit scope
                    return Err(self.format_scope_error());
                }
                if !aliases.is_empty() {
                    let mut all_stores = Vec::with_capacity(aliases.len());
                    for alias in &aliases {
                        all_stores.push(serve_state.get_or_open_stores(alias, false).await?);
                    }
                    return Ok(Some((all_stores, aliases)));
                }
                // No repos configured — fall through to local DB
            }
            return Ok(None);
        }

        // Must have serve_state to route
        let serve_state = match self.serve_state.as_ref() {
            Some(ss) => ss,
            None => {
                // Local/stdio mode: only one DB available, project/group are meaningless.
                // Fall through to local DB instead of erroring.
                tracing::warn!(
                    "MCP: project/group ignored in local mode (no serve running). \
                     Using local database."
                );
                return Ok(None);
            }
        };

        // Validate params
        types::validate_project_group(project, group, true)?;

        if let Some(ref alias) = project {
            let stores = serve_state.get_or_open_stores(alias, true).await?;
            return Ok(Some((vec![stores], vec![alias.clone()])));
        }

        if let Some(ref group_name) = group {
            let aliases = serve_state.resolve_group_aliases(group_name)?;
            if aliases.is_empty() {
                return Err(format!("Group '{}' has no members.", group_name));
            }
            let mut all_stores = Vec::with_capacity(aliases.len());
            for alias in &aliases {
                all_stores.push(serve_state.get_or_open_stores(alias, false).await?);
            }
            return Ok(Some((all_stores, aliases)));
        }

        Ok(None)
    }

    /// Resolve project/group params into a ready-to-use routing context.
    ///
    /// Encapsulates the common pattern: resolve multi-stores, extract single override
    /// vs multi-store vec, and determine if local DB check is needed.
    /// Also records the tool call for dashboard tracking when serve_state is active.
    async fn resolve_routing(
        &self,
        project: &Option<String>,
        group: &Option<String>,
        allow_unscoped: bool,
        tool_name: &str,
    ) -> std::result::Result<MultiStoreContext, String> {
        let resolved = self
            .resolve_repo_stores_multi(project, group, allow_unscoped)
            .await?;
        let is_multi = resolved
            .as_ref()
            .is_some_and(|(stores, _)| stores.len() > 1);
        let (stores, stores_vec, store_aliases, project_alias) = match &resolved {
            None => (None, None, None, None),
            Some((store_vec, aliases)) if store_vec.len() == 1 => {
                let alias = aliases.first().cloned();
                (Some(store_vec[0].clone()), None, None, alias)
            }
            Some((store_vec, aliases)) => {
                (None, Some(store_vec.clone()), Some(aliases.clone()), None)
            }
        };

        // Build alias → normalized project root map for path prefixing
        let mut alias_roots = std::collections::HashMap::new();
        if let Some(ref serve_state) = self.serve_state {
            let cfg = serve_state.config_snapshot();
            let all_aliases = store_aliases.as_deref().unwrap_or(&[]);
            for alias in all_aliases.iter() {
                if let Some(path) = cfg.resolve(alias) {
                    let root = crate::cache::normalize_path_str(path.to_string_lossy().as_ref())
                        .trim_end_matches('/')
                        .to_string();
                    alias_roots.insert(alias.clone(), root);
                }
            }
            if let Some(ref alias) = project_alias {
                if let Some(path) = cfg.resolve(alias) {
                    let root = crate::cache::normalize_path_str(path.to_string_lossy().as_ref())
                        .trim_end_matches('/')
                        .to_string();
                    alias_roots.insert(alias.clone(), root);
                }
            }
        }

        let needs_local_db = stores.is_none() && !is_multi;

        // Record tool call for serve dashboard tracking.
        // Skip recording for unscoped multi-store fan-out (allow_unscoped=true means
        // get_chunk or status — get_chunk will record after candidate detection,
        // status doesn't need per-repo recording).
        if let Some(ref serve_state) = self.serve_state {
            if !allow_unscoped || !is_multi {
                if let Some(ref aliases) = store_aliases {
                    for alias in aliases {
                        serve_state.record_tool_call(alias, tool_name);
                        // Explicit multi-repo/group query: treat as access.
                        // (Unscoped multi fan-out is skipped by the outer condition.)
                        serve_state.touch_access(alias);
                    }
                }
                if let Some(ref alias) = project_alias {
                    serve_state.record_tool_call(alias, tool_name);
                }
            }
        }

        Ok(MultiStoreContext {
            stores,
            stores_vec,
            store_aliases,
            project_alias,
            alias_roots,
            is_multi,
            needs_local_db,
        })
    }

    /// Build a structured `scope_required` error JSON for multi-repo mode.
    ///
    /// Returns a JSON string containing `error_code`, `message`, `available_projects`,
    /// `available_groups`, `project_groups`, and `hint_for_agent` so that LLM
    /// agents can programmatically react to the scope requirement. `project_groups`
    /// maps each project to the named group(s) it belongs to, so an agent can tell
    /// that picking a single project would miss sibling repos in the same group
    /// (e.g. a separate config / import-data repo).
    fn format_scope_error(&self) -> String {
        let (projects, mut groups, project_groups) = if let Some(ref serve_state) = self.serve_state
        {
            let cfg = serve_state.config_snapshot();
            let mut projects: Vec<String> = cfg.repos.keys().cloned().collect();
            // Include opt-in mounted remote projects so an agent can discover and
            // route to them by name (they are first-class `project=` targets).
            projects.extend(
                cfg.mounted_remote_projects()
                    .into_iter()
                    .map(|(name, _)| name),
            );
            projects.sort();
            projects.dedup();
            let mut groups: Vec<String> = cfg.groups.keys().cloned().collect();
            groups.sort();
            (projects, groups, cfg.project_groups())
        } else {
            (vec![], vec![], std::collections::HashMap::new())
        };
        // The "all" virtual group is always available when there are projects to
        // search — advertise it so agents discover the cross-repo shortcut.
        // (scope_required only fires when >1 repo is registered, so this is safe.)
        let all = crate::constants::ALL_GROUP_NAME.to_string();
        if !projects.is_empty() && !groups.contains(&all) {
            groups.push(all);
            groups.sort();
        }

        let payload = serde_json::json!({
            "error_code": "scope_required",
            "message": "Specify project= for a single repository or group= for cross-repo search.",
            "available_projects": projects,
            "available_groups": groups,
            "project_groups": project_groups,
            "hint_for_agent": "If the user has not indicated which repository to search, ask them to choose. Show available_projects and available_groups as options. IMPORTANT: project_groups maps each project to the group(s) it belongs to — if the project you would pick is listed there, prefer group= over project= so related repos (e.g. a separate config or import-data repo) are searched too."
        });
        payload.to_string()
    }

    /// Execute a read-only action against the vector store with an explicit store override.
    ///
    /// If `store_override` is provided (from project/group routing), it takes precedence.
    async fn with_vector_store_read_for<R, F>(
        &self,
        mut action: F,
        store_override: Option<Arc<SharedStores>>,
    ) -> Result<R>
    where
        F: FnMut(&VectorStore) -> anyhow::Result<R>,
    {
        // Priority 1: explicit store override (from project/group routing)
        if let Some(stores) = store_override {
            let store = stores.vector_store.read().await;
            return action(&store).context("Error reading from project-routed vector store");
        }

        // Priority 2: shared stores (set during IndexManager init)
        if let Some(ref stores) = self.shared_stores {
            let store = stores.vector_store.read().await;
            match action(&store) {
                Ok(result) => return Ok(result),
                Err(shared_err) => {
                    tracing::error!("Shared vector store read failed: {:?}", shared_err);

                    // In serve mode, do NOT fall back to a standalone VectorStore —
                    // it would open a second LMDB handle on the same .codesearch.db,
                    // which LMDB rejects ("environment already opened with different options").
                    if self.serve_state.is_some() {
                        return Err(shared_err.context(
                            "Shared vector store read failed in serve mode; \
                             standalone fallback disabled to prevent LMDB double-open",
                        ));
                    }
                }
            }

            // Standalone readonly fallback (non-serve mode only).
            // In serve mode the guard above already returned.
            if stores.readonly {
                let ro_store = VectorStore::open_readonly(&self.db_path, self.dimensions)
                    .context("Error opening readonly database for read fallback")?;
                return action(&ro_store)
                    .context("Error reading from readonly fallback vector store");
            }
        }

        // Standalone fallback (non-serve mode only — CLI / stdio MCP).
        // In serve mode, either Priority 1/2 succeeded or the guard above returned Err.
        let store = VectorStore::new(&self.db_path, self.dimensions)
            .context("Error opening database for read fallback")?;
        action(&store).context("Error reading from vector store")
    }

    /// Execute a read-only action against the FTS store with an explicit store override.
    ///
    /// If `store_override` is provided (from project/group routing), it takes precedence.
    async fn with_fts_store_read_for<R, F>(
        &self,
        action: F,
        store_override: Option<Arc<SharedStores>>,
    ) -> Result<R>
    where
        F: Fn(&FtsStore) -> Result<R>,
    {
        // Priority 1: explicit store override (from project/group routing)
        if let Some(stores) = store_override {
            let fts = stores.fts_store.read().await;
            return action(&fts);
        }

        // Priority 2: shared stores
        if let Some(ref stores) = self.shared_stores {
            let fts = stores.fts_store.read().await;
            return action(&fts);
        }

        // Fallback: open a new FtsStore
        let fts_store = FtsStore::new(&self.db_path).context("Error opening FTS store")?;
        action(&fts_store)
    }

    /// Fan-out vector store read across multiple stores, merging results.
    ///
    /// Runs `action` against each store and merges all results into a single vec,
    /// deduplicating by (alias, chunk_id) (keeping highest score) and sorting by score descending.
    ///
    /// A per-store failure does NOT abort the fan-out — one broken repo should
    /// not blind a group query to the healthy ones — but it is reported back in
    /// [`MultiReadOutcome::failures`] so the caller can tell an genuinely empty
    /// result apart from a total failure.
    async fn with_vector_store_read_multi<R, F>(
        &self,
        mut action: F,
        stores: Vec<Arc<SharedStores>>,
        aliases: &[String],
    ) -> Result<MultiReadOutcome<R>>
    where
        F: FnMut(&VectorStore) -> anyhow::Result<Vec<R>>,
        R: Clone + HasChunkId + HasScore,
    {
        let mut failures: Vec<(String, String)> = Vec::new();
        let mut all_results: Vec<R> = Vec::new();
        let mut seen_ids: std::collections::HashMap<(String, u32), usize> =
            std::collections::HashMap::new();

        for (idx, store_arc) in stores.iter().enumerate() {
            let alias = aliases.get(idx).map(|s| s.as_str()).unwrap_or("unknown");
            let store = store_arc.vector_store.read().await;
            match action(&store) {
                Ok(results) => {
                    for r in results {
                        let key = (alias.to_string(), r.chunk_id());
                        if let Some(&existing_idx) = seen_ids.get(&key) {
                            // Keep the one with higher score
                            if r.score() > all_results[existing_idx].score() {
                                all_results[existing_idx] = r;
                            }
                        } else {
                            seen_ids.insert(key, all_results.len());
                            all_results.push(r);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Vector store read failed for multi-store fan-out (alias {}): {:?}",
                        alias,
                        e
                    );
                    // Remembered, not just logged: a caller that only sees an
                    // empty Vec cannot tell "nothing matched" from "every store
                    // failed", and the second one must never be reported as a
                    // successful empty search.
                    failures.push((alias.to_string(), format!("{e:#}")));
                }
            }
        }

        // Sort by score descending
        all_results.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(MultiReadOutcome {
            results: all_results,
            failures,
        })
    }

    /// Fan-out FTS store read across multiple stores, merging results.
    ///
    /// Runs `action` against each store and merges all results into a single vec,
    /// deduplicating by (alias, chunk_id) (keeping highest score) and sorting by score descending.
    ///
    /// Like the vector fan-out, a per-store failure does not abort the query but
    /// IS reported in [`MultiReadOutcome::failures`]. The literal path is not
    /// hypothetical here: during the cloud read-only incident every affected
    /// vendor returned 0 results for literal search too, and it looked clean.
    async fn with_fts_store_read_multi<R, F>(
        &self,
        mut action: F,
        stores: Vec<Arc<SharedStores>>,
        aliases: &[String],
    ) -> Result<MultiReadOutcome<R>>
    where
        F: FnMut(&FtsStore) -> Result<Vec<R>>,
        R: Clone + HasChunkId + HasScore,
    {
        let mut failures: Vec<(String, String)> = Vec::new();
        let mut all_results: Vec<R> = Vec::new();
        let mut seen_ids: std::collections::HashMap<(String, u32), usize> =
            std::collections::HashMap::new();

        for (idx, store_arc) in stores.iter().enumerate() {
            let alias = aliases.get(idx).map(|s| s.as_str()).unwrap_or("unknown");
            let fts = store_arc.fts_store.read().await;
            match action(&fts) {
                Ok(results) => {
                    for r in results {
                        let key = (alias.to_string(), r.chunk_id());
                        if let Some(&existing_idx) = seen_ids.get(&key) {
                            if r.score() > all_results[existing_idx].score() {
                                all_results[existing_idx] = r;
                            }
                        } else {
                            seen_ids.insert(key, all_results.len());
                            all_results.push(r);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "FTS store read failed for multi-store fan-out (alias {}): {:?}",
                        alias,
                        e
                    );
                    // Same contract as the vector fan-out: a swallowed failure
                    // must not reach the caller as an ordinary empty result.
                    failures.push((alias.to_string(), format!("{e:#}")));
                }
            }
        }

        // Sort by score descending
        all_results.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(MultiReadOutcome {
            results: all_results,
            failures,
        })
    }

    // ─────────────────────────────────────────────────────────────────
    // Federation — cross-instance query merging (remote peers in a group).
    // ─────────────────────────────────────────────────────────────────

    /// Load the current repos config: from the live serve state when available,
    /// else straight from disk (stdio mode). A missing disk file yields the
    /// default (empty) config — federation simply has no peers to query.
    fn federation_config(&self) -> crate::db_discovery::repos::ReposConfig {
        if let Some(ref ss) = self.serve_state {
            return ss.config_snapshot();
        }
        crate::db_discovery::repos::ReposConfig::load().unwrap_or_default()
    }

    /// True when the given group fans out to at least one **mounted** remote
    /// project. A group that references `@peer` but where the user has mounted
    /// none of that peer's individual indexes is treated as local-only.
    fn group_has_remotes(cfg: &crate::db_discovery::repos::ReposConfig, group: &str) -> bool {
        !cfg.group_remote_projects(group).is_empty()
    }

    /// Merge local + remote search results for a group that has `@<peer>`
    /// members. Runs the local query (restricted to the group's local repos),
    /// fans out in parallel to each **mounted** remote project of the referenced
    /// peers (`<peer>/<alias>`, opt-in `remote_mounts`), then RRF-interleaves the
    /// disjoint ranked lists. One unreachable project becomes a `warning`, never
    /// a hard failure.
    /// Build the JSON body shipped to a remote peer for a federated search
    /// (group fan-out or single-project fan-out). Both call sites forward the
    /// same fields — `mode` and `limit_value` are passed explicitly because
    /// each caller computes them slightly differently (single lowercased
    /// `mode` string shared across a whole request; a per-call `limit_value`
    /// that may be over-fetched to compensate for client-side `filter_path`
    /// filtering). Extracted so the two bodies can't drift out of sync.
    fn build_remote_search_body(
        request: &SearchRequest,
        mode: &str,
        limit_value: Option<usize>,
    ) -> serde_json::Value {
        serde_json::json!({
            "query": request.query,
            "mode": mode,
            "compact": request.compact,
            "semantic_mode": request.semantic_mode,
            "regex": request.regex,
            "phrase": request.phrase,
            "file_glob": request.file_glob,
            "language": request.language,
            "format": request.format,
            "limit": limit_value,
        })
    }

    async fn federated_search(
        &self,
        request: &SearchRequest,
        cfg: &crate::db_discovery::repos::ReposConfig,
        remote_projects: Vec<(String, crate::db_discovery::repos::RemotePeer, String)>,
    ) -> Result<CallToolResult, McpError> {
        use crate::federation::{FederationClient, Outcome};
        use crate::rerank::DEFAULT_RRF_K;

        let mode = request.mode.as_deref().unwrap_or("semantic").to_lowercase();
        let limit = request.limit.unwrap_or(10);
        let group = request.group.clone().unwrap_or_default();

        // `filter_path` is applied CLIENT-SIDE (retain_by_filter_path) on the
        // namespaced result paths for BOTH the local and remote lists, never
        // forwarded to a peer nor down into the local group search — matching
        // against a store's own paths (wrong project root in serve mode) drops
        // everything. Over-fetch when a filter is set so enough survives.
        let has_filter = is_meaningful_filter(request.filter_path.as_deref());
        let fetch_limit = if has_filter {
            Some(request.limit.map(|l| (l * 10).max(50)).unwrap_or(50))
        } else {
            request.limit
        };

        // 1) Local results — internal handlers ignore `@remote` group members
        //    (they aren't local aliases), so they search only the group's local
        //    repos. Skip entirely when the group has no local repos.
        let (locals, _) = cfg.split_group_targets(&group);
        let mut local_items: Vec<SearchResultItem> = Vec::new();
        if !locals.is_empty() {
            let local_result = match mode.as_str() {
                "semantic" => {
                    let req = SemanticSearchRequest {
                        query: request.query.clone(),
                        limit: fetch_limit,
                        compact: request.compact,
                        filter_path: None,
                        mode: request.semantic_mode.clone(),
                        project: None,
                        group: Some(group.clone()),
                    };
                    self.semantic_search(Parameters(req)).await?
                }
                "literal" => {
                    let req = LiteralSearchRequest {
                        query: request.query.clone(),
                        regex: request.regex,
                        phrase: request.phrase,
                        limit: fetch_limit,
                        file_glob: request.file_glob.clone(),
                        language: request.language.clone(),
                        format: request.format.clone(),
                        project: None,
                        group: Some(group.clone()),
                    };
                    self.literal_search(Parameters(req)).await?
                }
                _ => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Unknown search mode '{}'. Use `semantic` or `literal`.",
                        mode
                    ))]));
                }
            };
            local_items = parse_search_items_from_call_result(&local_result, &mode);
            retain_by_filter_path(&mut local_items, request.filter_path.as_deref());
        }

        // 2) Build the request body shipped to each remote (group forced to the
        //    peer's own scope + project stripped by the federation client).
        //    `filter_path` intentionally omitted — applied client-side below.
        let body = Self::build_remote_search_body(request, &mode, fetch_limit);

        let client = match FederationClient::new() {
            Ok(c) => c,
            Err(e) => {
                // Can't build the HTTP client at all — degrade to local-only.
                return Ok(self.build_federated_response(
                    local_items,
                    vec![format!("federation disabled (http client error): {e}")],
                ));
            }
        };

        // 3) Fan out to each mounted remote project concurrently. Each is a
        //    project-scoped query (`project=<remote_alias>`) to its peer, so a
        //    group only ever searches the indexes the user opted into.
        // Record real activity per targeted peer so the embedded TUI can poke an
        // immediate `/status` refresh for the peer(s) the operator just used —
        // scale-to-zero friendly (no fixed-interval polling needed to learn a
        // peer was active). A peer is already awake the instant it serves this
        // very search, so the follow-up status poll can't keep it pinned.
        if let Some(ref serve_state) = self.serve_state {
            for (peer_name, _, _) in remote_projects.iter() {
                serve_state.record_remote_peer_activity(peer_name);
            }
        }
        let mut join = tokio::task::JoinSet::new();
        for (peer_name, peer, remote_alias) in remote_projects.into_iter() {
            let body = body.clone();
            let client = client.clone();
            join.spawn(async move {
                let outcome = client.search_project(&peer, body, &remote_alias).await;
                (peer_name, remote_alias, outcome)
            });
        }

        let mut warnings: Vec<String> = Vec::new();
        let mut all_lists: Vec<Vec<SearchResultItem>> = vec![local_items];
        while let Some(res) = join.join_next().await {
            match res {
                Ok((peer_name, remote_alias, Outcome::Ok(items))) => {
                    let mut converted: Vec<SearchResultItem> = items
                        .into_iter()
                        .map(|it| convert_remote_item(&peer_name, &remote_alias, it))
                        .collect();
                    retain_by_filter_path(&mut converted, request.filter_path.as_deref());
                    all_lists.push(converted);
                }
                Ok((peer_name, remote_alias, Outcome::Unreachable(reason))) => {
                    warnings.push(format!(
                        "remote project '{}/{}' unreachable: {}",
                        peer_name, remote_alias, reason
                    ));
                }
                Err(joinerr) => {
                    warnings.push(format!("federation task failed: {joinerr}"));
                }
            }
        }

        // 4) RRF-interleave the disjoint ranked lists and render.
        let merged = merge_ranked_lists(all_lists, DEFAULT_RRF_K, limit);
        Ok(self.build_federated_response(merged, warnings))
    }

    /// Query a single mounted remote project (`project=<peer>/<alias>`).
    ///
    /// A 1-to-1 passthrough: the query is forwarded to `peer` scoped to its own
    /// `remote_alias` project, and the peer's results are re-namespaced so
    /// `chunk_ref`s route back through `federated_get_chunk`. There is no local
    /// list to merge, but results still pass through `merge_ranked_lists` (a
    /// single list), so item `score`s are the RRF rank score, keeping rendering
    /// identical to the group path. An unreachable peer yields a warning with
    /// zero results rather than a hard error.
    async fn federated_project_search(
        &self,
        request: &SearchRequest,
        peer_name: String,
        peer: crate::db_discovery::repos::RemotePeer,
        remote_alias: String,
    ) -> Result<CallToolResult, McpError> {
        use crate::federation::{FederationClient, Outcome};
        use crate::rerank::DEFAULT_RRF_K;

        let mode = request.mode.as_deref().unwrap_or("semantic").to_lowercase();
        let limit = request.limit.unwrap_or(10);

        // `filter_path` is applied CLIENT-SIDE (see retain_by_filter_path) on the
        // namespaced result paths, NOT forwarded to the peer — a server-side
        // match against the peer's own store paths returns 0 for any value. When
        // a filter is set we over-fetch from the peer so enough survives the
        // post-filter to still fill `limit`.
        let has_filter = is_meaningful_filter(request.filter_path.as_deref());
        let peer_limit = if has_filter {
            Some(request.limit.map(|l| (l * 10).max(50)).unwrap_or(50))
        } else {
            request.limit
        };

        // Same shape as the group fan-out body; the federation client forces
        // `project=<remote_alias>` and strips `group`. `filter_path` is
        // intentionally omitted — applied client-side below.
        let body = Self::build_remote_search_body(request, &mode, peer_limit);

        let client = match FederationClient::new() {
            Ok(c) => c,
            Err(e) => {
                return Ok(self.build_federated_response(
                    vec![],
                    vec![format!("federation disabled (http client error): {e}")],
                ));
            }
        };

        // Record real activity on this peer so the embedded TUI pokes an
        // immediate `/status` refresh (scale-to-zero friendly). See
        // `federated_search` for the same note.
        if let Some(ref serve_state) = self.serve_state {
            serve_state.record_remote_peer_activity(&peer_name);
        }

        let outcome = client.search_project(&peer, body, &remote_alias).await;
        let (mut items, warnings) = match outcome {
            Outcome::Ok(items) => (
                items
                    .into_iter()
                    .map(|it| convert_remote_item(&peer_name, &remote_alias, it))
                    .collect::<Vec<_>>(),
                Vec::new(),
            ),
            Outcome::Unreachable(reason) => (
                Vec::new(),
                vec![format!(
                    "remote project '{}/{}' unreachable: {}",
                    peer_name, remote_alias, reason
                )],
            ),
        };

        // Client-side path scoping on the namespaced result paths.
        retain_by_filter_path(&mut items, request.filter_path.as_deref());

        // Single ranked list — RRF here is order-preserving and just caps to
        // `limit`, keeping rendering identical to the group path.
        let merged = merge_ranked_lists(vec![items], DEFAULT_RRF_K, limit);
        Ok(self.build_federated_response(merged, warnings))
    }

    /// Fetch a chunk from a remote peer by its namespaced `chunk_ref`.
    async fn federated_get_chunk(
        &self,
        chunk_ref: &str,
        context_lines: Option<usize>,
    ) -> Result<CallToolResult, McpError> {
        use crate::federation::{FederationClient, Outcome};

        let (peer_name, remote_alias, chunk_id) = match parse_federated_chunk_ref(chunk_ref) {
            Some(parts) => parts,
            None => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Invalid chunk_ref '{}': expected '<peer>/<alias>:<chunk_id>'.",
                    chunk_ref
                ))]));
            }
        };
        let cfg = self.federation_config();
        let peer = match cfg.remotes.get(peer_name) {
            Some(p) => p.clone(),
            None => {
                let known: Vec<String> = cfg.remotes.keys().cloned().collect();
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Unknown remote peer '{}' in chunk_ref '{}'. Known remotes: {}",
                    peer_name,
                    chunk_ref,
                    known.join(", ")
                ))]));
            }
        };
        let client = match FederationClient::new() {
            Ok(c) => c,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "federation disabled (http client error): {e}"
                ))]));
            }
        };
        // Record real activity on this peer so the embedded TUI pokes an
        // immediate `/status` refresh (scale-to-zero friendly). See
        // `federated_search` for the same note.
        if let Some(ref serve_state) = self.serve_state {
            serve_state.record_remote_peer_activity(peer_name);
        }
        match client
            .get_chunk(&peer, remote_alias, chunk_id, context_lines)
            .await
        {
            Outcome::Ok(value) => Ok(CallToolResult::success(vec![Content::text(
                value.to_string(),
            )])),
            Outcome::Unreachable(reason) => {
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Could not fetch chunk from remote peer '{}': {}",
                    peer_name, reason
                ))]))
            }
        }
    }

    /// Render the merged federated results as a `SemanticSearchResponse` JSON.
    ///
    /// `low_confidence` is only flagged when the merged set is empty: RRF-fused
    /// scores (`1/(k+rank+1)`, max ≈ 0.048 for k=20) are NOT comparable to the
    /// single-source embedding/BM25 thresholds, so applying any score cutoff
    /// here would be meaningless. An empty result, however, is a genuine signal
    /// to the agent that federation yielded nothing and it should try a broader
    /// query or a different scope.
    fn build_federated_response(
        &self,
        items: Vec<SearchResultItem>,
        warnings: Vec<String>,
    ) -> CallToolResult {
        let response = SemanticSearchResponse {
            low_confidence: if items.is_empty() { Some(true) } else { None },
            results: items,
            suggested_tool: None,
            warnings: if warnings.is_empty() {
                None
            } else {
                Some(warnings)
            },
        };
        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        CallToolResult::success(vec![Content::text(json)])
    }
}

// === Server Handler Implementation ===

/// Check if a chunk is a definition of the given symbol.
///
/// Best-effort heuristic for v1: a chunk is considered a definition if:
/// 1. Its kind is a definition kind (Function, Struct, Class, etc.)
/// 2. Its signature starts with a common definition pattern containing the symbol name
///
/// Limitation: this uses simple substring matching on the signature field.
/// False positives/negatives are possible for symbols that appear in signatures
/// of chunks that are not their definitions.
fn is_definition_chunk(kind: &str, signature: &Option<String>, symbol: &str) -> bool {
    // Only check definition kinds
    if !DEFINITION_KINDS.contains(&kind) {
        return false;
    }

    let sig = match signature {
        Some(s) if !s.is_empty() => s,
        _ => return false,
    };

    // Common definition prefixes across languages.
    // Keep this allocation-free in hot paths by using &str prefixes and boundary checks.
    const PREFIXES: &[&str] = &[
        "fn ",
        "def ",
        "class ",
        "struct ",
        "enum ",
        "trait ",
        "type ",
        "interface ",
        "impl ",
        "pub fn ",
        "pub async fn ",
        "pub struct ",
        "pub enum ",
        "pub trait ",
        "pub type ",
        "async fn ",
        "const ",
        "static ",
    ];

    let prefix_match = PREFIXES.iter().any(|prefix| {
        if !sig.starts_with(prefix) {
            return false;
        }

        let rest = &sig[prefix.len()..];
        if !rest.starts_with(symbol) {
            return false;
        }

        let next = rest[symbol.len()..].chars().next();
        matches!(next, None | Some('(' | '<' | ':' | ' ' | '\t'))
    });

    if prefix_match {
        return true;
    }

    // Fallback for languages with verbose signatures (C#, Java):
    // signatures include access modifiers and return types before the symbol name,
    // e.g. "public async Task<string> UploadFileAsync(...)" or "protected override void Update(...)".
    // Search for the symbol as a whole word anywhere in the signature.
    contains_symbol_as_word(sig, symbol)
}

/// Check whether `symbol` appears as a whole word in `sig`.
/// A word boundary requires the character before to be a space/tab (or start-of-string)
/// and the character after to be `(`, `<`, `:`, space, tab, or end-of-string.
/// This is intentionally conservative to avoid matching parameter type names.
fn contains_symbol_as_word(sig: &str, symbol: &str) -> bool {
    let sig_bytes = sig.as_bytes();
    let sym_len = symbol.len();
    let mut start = 0usize;
    while start + sym_len <= sig.len() {
        if let Some(rel) = sig[start..].find(symbol) {
            let abs = start + rel;
            let before_ok = abs == 0
                || matches!(
                    sig_bytes.get(abs - 1),
                    Some(&b' ') | Some(&b'\t') | Some(&b'\n')
                );
            let after_char = sig[abs + sym_len..].chars().next();
            let after_ok = matches!(after_char, None | Some('(' | '<' | ':' | ' ' | '\t'));
            if before_ok && after_ok {
                return true;
            }
            start = abs + 1;
        } else {
            break;
        }
    }
    false
}

// ════════════════════════════════════════════════════════════════
// REST API handlers (federation-friendly HTTP+JSON mirror of MCP tools).
//
// Expose the same logic over plain HTTP so a remote codesearch serve can be
// queried for federation WITHOUT an MCP session. Each handler constructs a
// throwaway `CodesearchService` bound to the live `ServeState`, invokes the
// existing `#[tool]` method, and returns the tool's JSON payload unwrapped
// from `CallToolResult`. Protected by serve's `require_auth_for_network`
// layer (same as /status, /mcp) — no separate auth code needed.
// ════════════════════════════════════════════════════════════════
use axum::extract::{Path as AxumPath, Query as AxumQuery, State as AxumState};
use axum::http::StatusCode;
use axum::response::Json as AxumJson;

type RestResponse = AxumJson<serde_json::Value>;
type RestError = (StatusCode, AxumJson<serde_json::Value>);

/// Unwrap a `CallToolResult` into the JSON a federation client wants.
///
/// `CallToolResult` carries its payload as `Content::text(json_string)`. The
/// normal case for search/find/explore/get_chunk is a single text item whose
/// value parses as JSON, so we parse it back and return the structured value
/// (clients get clean objects instead of a JSON-in-string). When the tool set
/// `is_error` (e.g. a `scope_required` error) we still parse the body but mark
/// it with `"_mcp_is_error": true` so a federation caller can distinguish tool
/// errors from HTTP errors. Non-JSON payloads fall back to `{content, is_error}`.
pub(crate) fn call_tool_result_to_json(result: CallToolResult) -> serde_json::Value {
    let is_error = result.is_error.unwrap_or(false);
    let val = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::json!({}));
    let text = val
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(mut parsed) => {
            if is_error {
                if let Some(obj) = parsed.as_object_mut() {
                    obj.insert("_mcp_is_error".into(), serde_json::json!(true));
                }
            }
            parsed
        }
        Err(_) => serde_json::json!({ "content": text, "is_error": is_error }),
    }
}

/// Build a per-request `CodesearchService` bound to the live serve state.
fn make_service(
    state: &std::sync::Arc<crate::serve::ServeState>,
) -> Result<CodesearchService, RestError> {
    CodesearchService::new_for_serve(state.clone()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            AxumJson(serde_json::json!({"error": format!("failed to build service: {e}")})),
        )
    })
}

/// Map an MCP-layer error to an HTTP 500. `McpError` (= `rmcp::ErrorData`)
/// derives `Serialize`, so round-trip it through a JSON value for the body.
fn mcp_err_to_http(e: McpError) -> RestError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        AxumJson(serde_json::json!({
            "error": serde_json::to_value(&e).unwrap_or(serde_json::Value::Null)
        })),
    )
}

pub(crate) async fn rest_search_handler(
    AxumState(state): AxumState<std::sync::Arc<crate::serve::ServeState>>,
    AxumJson(req): AxumJson<SearchRequest>,
) -> Result<RestResponse, RestError> {
    let service = make_service(&state)?;
    let result = service
        .search(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(AxumJson(call_tool_result_to_json(result)))
}

pub(crate) async fn rest_find_handler(
    AxumState(state): AxumState<std::sync::Arc<crate::serve::ServeState>>,
    AxumJson(req): AxumJson<FindRequest>,
) -> Result<RestResponse, RestError> {
    let service = make_service(&state)?;
    let result = service
        .find(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(AxumJson(call_tool_result_to_json(result)))
}

pub(crate) async fn rest_explore_handler(
    AxumState(state): AxumState<std::sync::Arc<crate::serve::ServeState>>,
    AxumJson(req): AxumJson<ExploreRequest>,
) -> Result<RestResponse, RestError> {
    let service = make_service(&state)?;
    let result = service
        .explore(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(AxumJson(call_tool_result_to_json(result)))
}

pub(crate) async fn rest_get_chunk_handler(
    AxumState(state): AxumState<std::sync::Arc<crate::serve::ServeState>>,
    AxumPath(chunk_id): AxumPath<u32>,
    AxumQuery(params): AxumQuery<std::collections::HashMap<String, String>>,
) -> Result<RestResponse, RestError> {
    let req = GetChunkRequest {
        chunk_id,
        chunk_ref: params.get("chunk_ref").cloned(),
        context_lines: params.get("context_lines").and_then(|s| s.parse().ok()),
        project: params.get("project").cloned(),
        group: params.get("group").cloned(),
    };
    let service = make_service(&state)?;
    let result = service
        .get_chunk(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(AxumJson(call_tool_result_to_json(result)))
}

impl CodesearchService {
    /// Single composition point for the tool router: this file's own routes
    /// plus every per-module router merged in. `#[tool_handler]` and both
    /// ctors wire through here, so extracting a `#[tool]` method into its own
    /// module only adds one `ToolRouter::merge` line — and the registration
    /// test in `tests.rs` proves nothing was silently dropped (`#[tool]` fns
    /// in an impl block the router macro does not scan are NOT registered).
    fn merged_tool_router() -> ToolRouter<CodesearchService> {
        let mut router = Self::tool_router();
        // The macro generates `find_impact_router` as an associated fn of
        // CodesearchService (inside the find_impact.rs impl block), so the
        // per-module routers all resolve through the type, not the module.
        router.merge(Self::find_impact_router());
        router.merge(Self::search_router());
        router.merge(Self::find_router());
        router.merge(Self::explore_router());
        router.merge(Self::status_router());
        router.merge(Self::get_chunk_router());
        router
    }
}

#[tool_handler(router = Self::merged_tool_router())]
impl ServerHandler for CodesearchService {
    fn get_info(&self) -> ServerInfo {
        let db_exists = self.db_path.exists();
        let mode = if self.serve_state.is_some() {
            "serve hub (direct)"
        } else {
            "self-contained (stdio)"
        };

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("codesearch", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                INSTRUCTIONS_TEMPLATE
                    .replace("{mode}", mode)
                    .replace("{project}", &self.project_path.display().to_string())
                    .replace("{db}", &self.db_path.display().to_string())
                    .replace("{exists}", if db_exists { "ready" } else { "not found" })
                    .replace("{model}", self.model_type.short_name())
                    .replace("{dims}", &self.dimensions.to_string()),
            )
    }
}
