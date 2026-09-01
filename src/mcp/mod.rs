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
use crate::symbols::{SymbolIndexerRegistry, SymbolReference};
use crate::vectordb::VectorStore;
use anyhow::{Context, Result};
use regex::Regex;
use rmcp::{
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// Re-export types
pub use types::*;

mod federation_helpers;
mod instructions;
mod proxy;
mod runtime;

// Re-export the extracted modules' items so every existing path keeps working:
// `super::X` from sibling test files and `crate::mcp::X` from serve/cli.
pub(crate) use federation_helpers::*;
pub(crate) use instructions::*;
pub use runtime::*;

/// Resolve the `find_impact` wall-clock budget: env var →
/// `DEFAULT_FIND_IMPACT_BUDGET_SECS`. `0` disables the budget (unbounded
/// lookup, the pre-budget behaviour). Mirrors
/// `resolve_proxy_idle_disconnect_secs`.
fn resolve_find_impact_budget_secs() -> u64 {
    std::env::var(crate::constants::FIND_IMPACT_BUDGET_SECS_ENV)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(crate::constants::DEFAULT_FIND_IMPACT_BUDGET_SECS)
}

/// Outcome of a budget-bounded `find_impact` lookup.
pub(crate) enum ImpactLookupOutcome {
    /// The lookup finished within the budget (or the budget is disabled).
    /// `Err` preserves the store/helper failure for the caller to report.
    Done(Result<Vec<SymbolReference>, anyhow::Error>),
    /// The budget overran; the lookup keeps running in the background.
    Busy {
        /// What is still running (goes into the busy envelope verbatim).
        state: String,
        /// Wall-clock time actually waited before giving up.
        waited_ms: u64,
    },
}

/// Race a `find_impact` lookup against its wall-clock budget.
///
/// `lookup` is the already-offloaded lookup future (the handler runs the
/// blocking SCIP call on `spawn_blocking`); it is NOT cancelled on overrun —
/// dropping the future abandons it while the detached blocking task keeps
/// running, so its reference-cache writes still land in LMDB and the retry
/// hinted by the busy answer is served warm. `budget_secs == 0` disables the
/// race entirely. Kept generic over the future so tests can plant a sleeping
/// handler instead of a real SCIP helper.
pub(crate) async fn find_impact_with_budget<F>(
    budget_secs: u64,
    state: String,
    lookup: F,
) -> ImpactLookupOutcome
where
    F: std::future::Future<Output = Result<Vec<SymbolReference>, anyhow::Error>>,
{
    if budget_secs == 0 {
        return ImpactLookupOutcome::Done(lookup.await);
    }
    let started = std::time::Instant::now();
    match tokio::time::timeout(std::time::Duration::from_secs(budget_secs), lookup).await {
        Ok(result) => ImpactLookupOutcome::Done(result),
        Err(_elapsed) => ImpactLookupOutcome::Busy {
            state,
            waited_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        },
    }
}

/// Tests for the `find_impact` wall-clock budget: sleeping-handler race
/// tests (generic lookup future) plus `#[serial]` env-resolution tests.
#[cfg(test)]
#[path = "find_impact_tests.rs"]
mod find_impact_tests;

/// Background-continuation registry for budget-overrun `find_impact`
/// lookups: in-flight tracking keyed by (project, symbol) so a retry
/// observes progress or the warm result instead of restarting cold.
mod find_impact_tracker;

/// Read model short-name and dimensions from a database's `metadata.json`.
/// Returns `(model_name, dimensions)`, defaulting to `("unknown", DEFAULT_EMBEDDING_DIMENSIONS)`.
fn read_model_metadata(db_path: &Path) -> (String, usize) {
    let metadata_path = db_path.join("metadata.json");
    if let Ok(content) = std::fs::read_to_string(&metadata_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            let model_name = json
                .get("model_short_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let dims = json.get("dimensions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            // If metadata has explicit dimensions, use those; otherwise infer from model name.
            let dims = if dims > 0 {
                dims
            } else {
                ModelType::parse(&model_name)
                    .map(|m| m.dimensions())
                    .unwrap_or(crate::constants::DEFAULT_EMBEDDING_DIMENSIONS)
            };
            return (model_name, dims);
        }
    }
    (
        "unknown".to_string(),
        crate::constants::DEFAULT_EMBEDDING_DIMENSIONS,
    )
}

/// Read chunk/file counts from metadata.json (written after each indexing operation).
/// Returns `(total_chunks, total_files)` defaulting to `(0, 0)`.
///
/// When metadata.json reports `total_chunks == 0` but the LMDB database exists,
/// falls back to opening the store read-only and counting live chunks.
/// This catches the case where a metadata writer clobbered the stats fields
/// (see `merge_metadata_atomic` for the definitive fix). The fallback is lazy —
/// only triggered when metadata reports zero — so it does not unnecessarily
/// open databases for repos that already have correct metadata.
fn read_metadata_stats(db_path: &Path) -> (usize, usize) {
    let metadata_path = db_path.join("metadata.json");
    if let Ok(content) = std::fs::read_to_string(&metadata_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            let total_chunks = json
                .get("total_chunks")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let total_files = json
                .get("total_files")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            if total_chunks > 0 {
                return (total_chunks, total_files);
            }

            // Metadata says 0 chunks — try live LMDB count as fallback.
            // This is safe in serve context: `read_metadata_stats` is only called
            // for repos NOT yet opened in SharedStores (opened repos use vs.stats()
            // directly), so no double-open risk.
            if let Some((live_chunks, live_files)) = live_chunk_count(db_path) {
                tracing::info!(
                    "metadata.json reports 0 chunks for {}, but LMDB has {} chunks / {} files — using live count",
                    db_path.display(), live_chunks, live_files
                );
                return (live_chunks, live_files);
            }

            return (total_chunks, total_files);
        }
    }
    (0, 0)
}

/// Open the LMDB read-only and count chunks/files.
/// Returns `None` if the database cannot be opened (missing, corrupt, or
/// already locked by another handle).
///
/// # Safety (LMDB double-open)
///
/// This function is only called when `get_opened_stores(alias)` returned `None`,
/// meaning no `SharedStores` handle exists for this repo. There is a theoretical
/// race window between that check and this `open_readonly` call where another task
/// could open the repo via `get_or_open_stores`. In practice this is safe because:
/// 1. The tokio runtime uses a single thread for non-spawned futures.
/// 2. Even if the race occurs, `open_readonly` returns `Err` (TrackedEnv blocks it),
///    and we return `None` — no crash, no corruption.
fn live_chunk_count(db_path: &Path) -> Option<(usize, usize)> {
    let (model_name, dims) = read_model_metadata(db_path);
    if model_name == "unknown" {
        return None;
    }
    match VectorStore::open_readonly(db_path, dims) {
        Ok(store) => match store.stats() {
            Ok(stats) if stats.total_chunks > 0 => Some((stats.total_chunks, stats.total_files)),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(
                    "live_chunk_count: stats() failed for {}: {}",
                    db_path.display(),
                    e
                );
                None
            }
        },
        Err(e) => {
            tracing::debug!(
                "live_chunk_count: open_readonly failed for {}: {}",
                db_path.display(),
                e
            );
            None
        }
    }
}

/// RRF score threshold below which results are considered low-confidence.
/// When the top result's RRF score falls below this, the response includes
/// a `low_confidence` flag and a `suggested_tool` hint.
const LOW_CONFIDENCE_THRESHOLD: f32 = 0.02;

/// Chunk kinds that represent symbol definitions (not usages/comments/etc.)
const DEFINITION_KINDS: &[&str] = &[
    "Function",
    "Class",
    "Method",
    "Struct",
    "Trait",
    "Enum",
    "TypeAlias",
    "Interface",
];

/// Collapse exact-duplicate references from the SCIP find-refs output.
///
/// The helper can emit multiple occurrences of the same symbol at the same
/// file:line (declaration plus multiple roles on one line), which reaches the
/// agent as visible noise (observed live: a definition listed 5×). Two
/// references that are identical in ALL of (file, line range, kind) carry no
/// information the agent could act on separately — `SymbolReference` has no
/// column, so two genuinely distinct same-line calls are indistinguishable
/// from duplicates and collapse too, which is the right call for a caller
/// that wants "where is this used", not occurrence counts. Order is stable.
fn dedupe_references(
    refs: Vec<crate::symbols::SymbolReference>,
) -> Vec<crate::symbols::SymbolReference> {
    let mut seen: std::collections::HashSet<(String, u32, u32, String)> =
        std::collections::HashSet::with_capacity(refs.len());
    let mut out = Vec::with_capacity(refs.len());
    for r in refs {
        let key = (
            r.file.to_string_lossy().into_owned(),
            r.start_line,
            r.end_line,
            r.kind.clone(),
        );
        if seen.insert(key) {
            out.push(r);
        }
    }
    out
}

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

// === Multi-store fan-out traits ===

/// Trait for types that have a chunk ID (used for deduplication in group fan-out).
trait HasChunkId {
    fn chunk_id(&self) -> u32;
}

/// Trait for types that have a relevance score (used for sorting in group fan-out).
trait HasScore {
    fn score(&self) -> f32;
}

impl HasChunkId for crate::vectordb::SearchResult {
    fn chunk_id(&self) -> u32 {
        self.id
    }
}

impl HasScore for crate::vectordb::SearchResult {
    fn score(&self) -> f32 {
        self.score
    }
}

impl HasChunkId for crate::fts::FtsResult {
    fn chunk_id(&self) -> u32 {
        self.chunk_id
    }
}

impl HasScore for crate::fts::FtsResult {
    fn score(&self) -> f32 {
        self.score
    }
}

// === Simple Glob Matcher ===
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

fn normalize_tool_path(path: &str, project_root: &Path) -> String {
    let p = Path::new(path);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        project_root.join(p)
    };
    crate::cache::normalize_path_str(resolved.to_string_lossy().as_ref())
}

/// Strip a project-alias prefix from a tool path.
///
/// In serve mode, tools like explore receive `target = "ALIAS/src/foo.rs"` with
/// `project = "ALIAS"`.  The alias prefix must be stripped before calling
/// `chunks_for_file`, which expects a path relative to the project root.
fn strip_alias_prefix(path: &str, alias: Option<&String>) -> String {
    if let Some(a) = alias {
        let prefix = format!("{}/", a);
        match path.strip_prefix(&prefix) {
            Some(rest) => rest.to_string(),
            None => path.to_string(),
        }
    } else {
        path.to_string()
    }
}

/// Prefix a result path with its repo alias for group queries, normalizing
/// Windows backslashes to forward slashes in the process. When `alias` is
/// None or empty, the path is still normalized (useful for stdio mode).
pub(crate) fn prefix_path_with_alias(
    path: &str,
    alias: Option<&str>,
    project_root: &str,
) -> String {
    let normalized = crate::cache::normalize_path_str(path);
    let normalized_root = crate::cache::normalize_path_str(project_root)
        .trim_end_matches('/')
        .to_string();
    match normalized.strip_prefix(&normalized_root) {
        Some(rest) => {
            let relative = rest.trim_start_matches('/');
            match alias {
                Some(a) if !a.is_empty() => format!("{}/{}", a, relative),
                _ => relative.to_string(),
            }
        }
        None => normalized,
    }
}

/// Prefix a result path with the matching repo alias from a set of aliases and their roots.
/// Used by handlers that have alias/root info but not a full `MultiStoreContext`.
fn prefix_path_multi(
    path: &str,
    aliases: &[String],
    alias_roots: &std::collections::HashMap<String, String>,
) -> String {
    let normalized = crate::cache::normalize_path_str(path);
    for alias in aliases {
        if let Some(root) = alias_roots.get(alias) {
            if normalized.starts_with(root.as_str()) {
                return prefix_path_with_alias(path, Some(alias), root);
            }
        }
    }
    normalized
}

/// Pick the project root to relativise a result path against for a `filter_path`
/// prefix match, so `filter_path` is interpreted **relative to the repo root**
/// in every routing mode:
/// - serve single-project routing → the routed alias's root (`alias_roots[alias]`);
/// - serve multi/group → the longest alias root the (absolute) path lives under;
/// - stdio single-repo (no alias roots) → the service's own `project_path`
///   (`fallback_root`).
///
/// Before this, the filter always used the service's `project_path`, which for a
/// serve-routed project is NOT the routed repo's root — so the absolute stored
/// path never relativised and every hit was dropped. The federated paths solve
/// the same class of bug client-side (see `retain_by_filter_path`); this covers
/// the local (non-federated) serve/multi case.
fn pick_filter_root(
    path: &str,
    project_alias: Option<&str>,
    alias_roots: &std::collections::HashMap<String, String>,
    fallback_root: &str,
) -> String {
    if let Some(alias) = project_alias {
        if let Some(root) = alias_roots.get(alias) {
            return root.clone();
        }
    }
    if !alias_roots.is_empty() {
        let normalized = crate::cache::normalize_path_str(path);
        if let Some(root) = alias_roots
            .values()
            .filter(|r| normalized.starts_with(r.as_str()))
            .max_by_key(|r| r.len())
        {
            return root.clone();
        }
    }
    fallback_root.to_string()
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

// === Multi-Store Routing Context ===

/// Pre-computed routing context for a tool handler.
///
/// Created by `CodesearchService::resolve_routing()`, this struct encapsulates
/// all the decisions a handler needs: which store to use, whether to fan out,
/// and whether to call `ensure_database_exists()`.
/// Outcome of a fan-out read across several repos.
///
/// Exists so an empty `results` is never ambiguous. A group query that hits a
/// broken store used to come back as a successful search with zero hits, which
/// is the most misleading signal this system can emit — it reads as "the corpus
/// does not contain that", and it is what sent an earlier round of this
/// investigation chasing an indexing problem that did not exist.
#[must_use]
struct MultiReadOutcome<R> {
    /// Merged, deduplicated, score-sorted results from the stores that worked.
    results: Vec<R>,
    /// `(alias, full error chain)` for every store that failed. Empty on a
    /// clean run.
    failures: Vec<(String, String)>,
}

/// Decide the `status`/`status_message` pair for a multi-store
/// `status(kind="index"|"projects")` response.
///
/// Pulled out of the handler so the four-way call — every store down, still
/// building, ready but one or more stores failed to report their stats, or
/// fully ready — is testable without opening a single store. `failed_count`
/// is checked before declaring "building" or "ready" precisely so a store
/// that came back `Err` cannot render identically to one that returned
/// healthy zero-valued stats. The all-failed case is checked first: a
/// correlated failure (e.g. every store hits the same read-only-snapshot or
/// disk-full condition at once) also has `total_chunks == 0`, and without
/// this ordering it fell through to "building" — byte-identical to a group
/// that simply has not been indexed yet, which is the exact indistinguishable
/// case this fix exists to close. See AGENTS.md's fan-out warnings-channel
/// rule.
fn index_status_summary(
    total_repos: usize,
    failed_count: usize,
    total_chunks: usize,
) -> (String, String) {
    if total_repos > 0 && failed_count >= total_repos {
        (
            "error".to_string(),
            format!(
                "All {total_repos} repo(s) failed to report status — every store errored, see `warnings`."
            ),
        )
    } else if total_chunks == 0 {
        (
            "building".to_string(),
            format!(
                "Index is being built across {total_repos} repo(s). Searches may fail until indexing completes."
            ),
        )
    } else if failed_count > 0 {
        (
            "ready".to_string(),
            format!(
                "Index is ready for searching across {} of {total_repos} repo(s) — {failed_count} store(s) failed to report status, see `warnings`.",
                total_repos.saturating_sub(failed_count),
            ),
        )
    } else {
        (
            "ready".to_string(),
            format!("Index is ready for searching across {total_repos} repo(s)."),
        )
    }
}

/// Turn a store's `stats()` result into the `(total_chunks, total_files,
/// error)` triple `list_projects` reports per repo.
///
/// Pulled out of the handler, mirroring `index_status_summary` just above, so
/// the fix's actual claim — a `stats()` failure surfaces as `error: Some(..)`
/// with zero-valued counts, instead of silently rendering as a healthy-looking
/// empty repo — is unit-testable without opening a real `VectorStore` or
/// `ServeState`. The two calls to `serve_state.repo_lock_status()` in
/// `list_projects` don't vary by outcome, so they stay in the handler; this
/// covers only the part that does.
fn repo_stats_from_result(
    stats: anyhow::Result<crate::vectordb::StoreStats>,
) -> (usize, usize, Option<String>) {
    match stats {
        Ok(s) => (s.total_chunks, s.total_files, None),
        Err(ref e) => (0, 0, Some(format!("stats unavailable: {e:#}"))),
    }
}

/// `repo_stats_from_result` plus recording the failure as a caller-facing
/// warning, in one call.
///
/// `list_projects` used to inline `repo_stats_from_result` and then decide
/// separately whether to push a warning — two steps a future edit could
/// silently pull apart (drop the second one, keep the first) without
/// affecting `total_chunks`/`total_files` at all, so nothing would look
/// wrong at the call site. Folding both into one call means a regression
/// that drops the warning has to delete this call entirely, which also
/// deletes the counts — no longer a silent edit. This is also the seam a
/// test can drive without opening a real `VectorStore`/`ServeState`: it
/// exercises the exact composition `list_projects` calls, not a
/// re-implementation of it.
fn record_stats_or_warn(
    stats: anyhow::Result<crate::vectordb::StoreStats>,
    alias: &str,
    warnings: &mut Vec<String>,
) -> (usize, usize, Option<String>) {
    let (total_chunks, total_files, error) = repo_stats_from_result(stats);
    if let Some(ref msg) = error {
        push_store_warning(warnings, &store_warning(alias, "stats", msg));
    }
    (total_chunks, total_files, error)
}

/// Record a per-store failure as a caller-facing warning, once per store.
///
/// Resolution loops run per hit, so a single broken store would otherwise emit
/// one identical warning per result; the caller wants to know *that* the repo
/// is down, not how many times it noticed.
fn note_store_failure(
    warnings: &mut Vec<String>,
    aliases: &[String],
    idx: usize,
    what: &str,
    err: &anyhow::Error,
) {
    let alias = aliases.get(idx).map(|s| s.as_str()).unwrap_or("unknown");
    push_store_warning(warnings, &store_warning(alias, what, &format!("{err:#}")));
}

/// The one place a per-store warning line is formatted. Two copies used to
/// exist and could drift; a caller matching on this text would then silently
/// stop matching half of them.
fn store_warning(alias: &str, what: &str, err: &str) -> String {
    format!("repo '{alias}' {what} failed: {err}")
}

/// Append a warning unless it is already present, logging it once.
fn push_store_warning(warnings: &mut Vec<String>, msg: &str) {
    if !warnings.iter().any(|w| w == msg) {
        tracing::error!("MCP: {}", msg);
        warnings.push(msg.to_string());
    }
}

/// The single exit for a handler that returns a list of items plus a warnings
/// channel.
///
/// Five handlers previously read their channel ONLY on the empty path, so a
/// partially-failed group returned a plausible-looking short list with no
/// signal at all - the same false negative as an empty result, just harder to
/// notice. Routing every exit through here means the channel is carried
/// whether the list is empty or not, and there is no per-handler discipline
/// left to forget.
///
/// A healthy call is byte-identical to the previous behaviour (a bare JSON
/// array), so this is backward compatible.
fn respond_with_items<T: serde::Serialize>(
    items: &[T],
    warnings: &[String],
    empty_message: impl FnOnce() -> String,
) -> Result<CallToolResult, McpError> {
    respond_with_items_noted(items, warnings, None, empty_message)
}

/// `respond_with_items` with an optional agent-facing `note` key — the shared
/// exit for item-list handlers whose result carries an advisory the caller
/// should act on (e.g. `find(kind="usages")` pointing lexical hits at
/// `find_impact`). Same discipline as `respond_with_items`: one exit, the
/// warnings channel terminates on every path.
///
/// Shape:
/// - empty items → text via `qualify_empty_result`; when a note is present it
///   is appended to the empty message (an empty lexical result is exactly
///   where the SCIP upgrade path matters most)
/// - note + warnings → `{results, note, warnings}`
/// - note only → `{results, note}`
/// - warnings only → `{results, warnings}` (identical to `respond_with_items`)
/// - healthy, no note → bare JSON array, byte-identical to the legacy shape
fn respond_with_items_noted<T: serde::Serialize>(
    items: &[T],
    warnings: &[String],
    note: Option<&str>,
    empty_message: impl FnOnce() -> String,
) -> Result<CallToolResult, McpError> {
    if items.is_empty() {
        let mut message = empty_message();
        if let Some(note) = note {
            message.push(' ');
            message.push_str(note);
        }
        return Ok(CallToolResult::success(vec![Content::text(
            qualify_empty_result(message, warnings),
        )]));
    }
    if note.is_none() && warnings.is_empty() {
        let json = serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string());
        return Ok(CallToolResult::success(vec![Content::text(json)]));
    }
    let mut payload = serde_json::Map::new();
    payload.insert("results".to_string(), serde_json::json!(items));
    if let Some(note) = note {
        payload.insert("note".to_string(), serde_json::json!(note));
    }
    if !warnings.is_empty() {
        payload.insert("warnings".to_string(), serde_json::json!(warnings));
    }
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::Value::Object(payload).to_string(),
    )]))
}

/// The object-shaped sibling of `respond_with_items`: one exit for handlers that
/// return a single struct rather than a list.
///
/// A `warnings` *field* on the response struct was the obvious fix and is the
/// weaker one — the handler is still free to populate it with `None`, and a test
/// that builds the struct itself cannot see that happen. Review round 8 proved
/// it: the round-7 defect was reintroduced at the `get_chunk` success path and
/// all 630 tests still passed.
///
/// **This is an improvement, not a guarantee.** Round 9 measured the difference:
/// passing `&[]` here is exactly as writable as `warnings: None` was, the suite
/// still cannot see it, and no lint fires (the channel stays "used" by the
/// ambiguous path). What it actually buys is narrower and real — no optional
/// field whose absence is invisible, no future construction site that can zero
/// it, and an audit that collapses from "check every response struct" to "check
/// the call sites of two functions", which is grep-answerable. The channel can
/// no longer be *forgotten*, only actively discarded.
///
/// Healthy path serializes the struct directly, so its key order and bytes are
/// unchanged. `serde_json::Map` is a `BTreeMap` here (no `preserve_order`
/// feature), so round-tripping through `to_value` would silently re-sort the
/// keys — which is only acceptable on the warning path, where the shape is new
/// anyway.
fn respond_with_object<T: serde::Serialize>(
    value: &T,
    warnings: &[String],
) -> Result<CallToolResult, McpError> {
    if !warnings.is_empty() {
        if let Ok(mut v) = serde_json::to_value(value) {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("warnings".to_string(), serde_json::json!(warnings));
                return Ok(CallToolResult::success(vec![Content::text(v.to_string())]));
            }
        }
    }
    let json = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Build the `ambiguous_chunk_id` payload for `get_chunk`.
///
/// `candidate_projects` reads as the complete set of repos holding this
/// chunk_id, so a store that failed to answer must be declared: the repo the
/// caller actually wants may be the one missing from the list. Extracted so the
/// "is this list complete?" decision is testable without standing up stores.
///
/// `warnings` is *inserted* rather than emitted as `null`, so the healthy-path
/// shape is byte-identical to before — matching `skip_serializing_if` on every
/// other warnings-carrying response.
fn ambiguous_chunk_payload(
    chunk_id: u32,
    candidate_projects: &[&str],
    warnings: &[String],
) -> serde_json::Value {
    let mut message =
        format!("chunk_id {chunk_id} exists in multiple repositories. Specify which one.");
    if !warnings.is_empty() {
        message.push_str(" The candidate list is incomplete — see `warnings`.");
    }
    let mut payload = serde_json::json!({
        "error_code": "ambiguous_chunk_id",
        "message": message,
        "candidate_projects": candidate_projects,
        "hint_for_agent": "The chunk_id collision is a known limitation of multi-repo mode. Re-run get_chunk with one of the candidate_projects, or use search to identify the correct repository first."
    });
    if !warnings.is_empty() {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("warnings".to_string(), serde_json::json!(warnings));
        }
    }
    payload
}

/// Decide whether to keep the "try another tool" hint.
///
/// A weak or empty result caused by a store that is DOWN is not a reason to
/// retry with a different tool — that just sends the agent back at the same
/// broken store. Extracted from `build_semantic_response` so the decision is
/// testable without standing up a service.
fn retry_hint(suggested: Option<String>, warnings: &Option<Vec<String>>) -> Option<String> {
    // `is_some()` alone is wrong: an empty `Some(vec![])` means nothing failed,
    // and suppressing a legitimate hint on it would be a silent regression the
    // moment a caller constructs the warnings vec eagerly.
    if warnings.as_ref().is_some_and(|w| !w.is_empty()) {
        return None;
    }
    suggested
}

/// Qualify a "nothing found" message when a store in scope actually failed.
///
/// This is the defect that keeps coming back in a new handler: "No definition
/// found — the symbol may not be indexed" is a *diagnosis*, and it is flatly
/// wrong when the store never answered. An agent acts on it by giving up or by
/// re-indexing something that was never broken.
fn qualify_empty_result(message: String, warnings: &[String]) -> String {
    if warnings.is_empty() {
        return message;
    }
    format!(
        "{message}\n\nWARNING: this result is not trustworthy — {count} store(s) in \
         scope failed, so \"not found\" may mean \"not searched\":\n{detail}",
        count = warnings.len(),
        detail = warnings.join("\n")
    )
}

// Hand-written rather than derived: `derive(Default)` would demand `R: Default`,
// which the result types do not implement and do not need to.
impl<R> Default for MultiReadOutcome<R> {
    fn default() -> Self {
        Self {
            results: Vec::new(),
            failures: Vec::new(),
        }
    }
}

impl<R> MultiReadOutcome<R> {
    /// Render failures as caller-facing warning lines.
    fn warnings(&self, what: &str) -> Vec<String> {
        self.failures
            .iter()
            .map(|(alias, err)| store_warning(alias, what, err))
            .collect()
    }

    /// Take the results, routing any failures into `warnings` on the way out.
    ///
    /// Deliberately the only ergonomic way to get at `results`: reaching for
    /// the field directly and dropping `failures` is `unwrap_or_default()`
    /// under a new name, and that is the bug this whole type exists to stop.
    fn into_results(self, warnings: &mut Vec<String>, what: &str) -> Vec<R> {
        for (alias, err) in &self.failures {
            push_store_warning(warnings, &store_warning(alias, what, err));
        }
        self.results
    }
}

struct MultiStoreContext {
    /// Single-store override (set when exactly 1 repo resolved, or None).
    /// Pass to `with_*_store_read_for()` methods.
    stores: Option<Arc<SharedStores>>,
    /// Multi-store vec for fan-out (set when 2+ repos resolved, or None).
    /// Use `if let Some(ref sv) = ctx.stores_vec { ... }` for the multi-store path.
    stores_vec: Option<Vec<Arc<SharedStores>>>,
    /// Alias for each store in `stores_vec` (parallel with stores_vec).
    /// Used for path prefixing and per-alias dedup.
    store_aliases: Option<Vec<String>>,
    /// Alias for single-project routing (set when project= is given).
    project_alias: Option<String>,
    /// Normalized project root for each alias (alias → root path).
    /// Used by `prefix_path` to strip absolute paths and add alias prefix.
    alias_roots: std::collections::HashMap<String, String>,
    /// True when `stores_vec` has 2+ entries (group fan-out).
    is_multi: bool,
    /// True when no serve-state stores resolved and local DB should be checked.
    needs_local_db: bool,
}

impl MultiStoreContext {
    /// Aliases parallel to `stores_vec`, or an empty slice when absent.
    ///
    /// Every fan-out that reports a per-store failure needs this, and hand-rolling
    /// `let empty = Vec::new(); ...unwrap_or(&empty)` at each site produced four
    /// copies of the same two lines — and one handler where the binding was out
    /// of scope, which is how a silent store read survived a round of review.
    fn aliases(&self) -> &[String] {
        self.store_aliases.as_deref().unwrap_or(&[])
    }

    /// Prefix a result path with its owning alias for multi-repo identification.
    ///
    /// Three dispatch modes:
    /// - Single-project (`project_alias = Some(...)`): prefix with that alias.
    /// - Group (`store_aliases = Some([...])`): detect alias by prefix-matching
    ///   the path against known project roots in `alias_roots`.
    /// - Stdio / no alias info: normalize only, no prefix.
    ///
    /// Emits a `tracing::debug!` event when an expected alias cannot be resolved.
    /// That usually indicates a config mismatch or a path from an unregistered source —
    /// the path is still normalized and returned, but diagnosis is easier with the log.
    fn prefix_result_path(&self, path: &str) -> String {
        if let Some(ref alias) = self.project_alias {
            if let Some(root) = self.alias_roots.get(alias) {
                return prefix_path_with_alias(path, Some(alias), root);
            }
            tracing::debug!(
                target: "codesearch::mcp::path_prefix",
                alias = %alias,
                path = %path,
                "project_alias has no entry in alias_roots"
            );
        }
        if let Some(ref aliases) = self.store_aliases {
            let normalized = crate::cache::normalize_path_str(path);
            for alias in aliases {
                if let Some(root) = self.alias_roots.get(alias) {
                    if normalized.starts_with(root.as_str()) {
                        return prefix_path_with_alias(path, Some(alias), root);
                    }
                }
            }
            tracing::debug!(
                target: "codesearch::mcp::path_prefix",
                aliases = ?aliases,
                path = %path,
                "no alias root matched path in group mode"
            );
        }
        crate::cache::normalize_path_str(path)
    }
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

    // ─────────────────────────────────────────────────────────────────
    // Consolidated tools (the primary 5-tool surface)
    // ─────────────────────────────────────────────────────────────────

    /// Unified search tool — dispatches to semantic or literal search based on `mode`.
    #[tool(
        description = "Unified code search. Set `mode` to choose the backend:\n\n- `semantic` (default): vector embeddings + BM25 FTS + exact-identifier boosting, fused with RRF. Best for conceptual queries, identifier lookups, and mixed natural-language + symbol queries.\n- `literal`: pure FTS, no embeddings. Fast and works without an embedding model. Sub-mode selection:\n  * Queries with operators, brackets, or punctuation (`foo = null`, `Vec<T>`, `return x;`, `a::b`) -> set `regex=true` and write the query as a regex. BM25 tokenizes on punctuation otherwise, producing noisy results.\n  * Multi-word exact phrases -> set `phrase=true`.\n  * Plain identifier lookups (`CodesearchService`) -> leave both false.\n\nFor semantic mode, optionally set `semantic_mode`: \"auto\" (default) | \"semantic\" | \"lexical\" | \"hybrid\".\nReturns metadata only by default (`compact=true`). Use `get_chunk` to read full code. Prefer `search(mode=\"literal\", regex=true)` over external grep/ripgrep for code patterns.\n\nIMPORTANT (multi-repo): always specify either `project` (single repo) or `group` (cross-repo). Omitting both in multi-repo mode returns a `scope_required` error with the list of available projects and groups. If the user has not indicated which repository to search, ask them to choose."
    )]
    async fn search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(
            "📥 search(query={:?}, mode={:?}, project={:?}, group={:?})",
            request.query,
            request.mode,
            request.project,
            request.group,
        );

        // Federation: when the query targets a group that resolves to one or more
        // remote peers, merge local + remote results (RRF-interleave) instead of
        // searching local repos only. Only `group` federates; `project` stays
        // local because project aliases are instance-local.
        if let Some(group) = request.group.as_deref() {
            let cfg = self.federation_config();
            if Self::group_has_remotes(&cfg, group) {
                let remote_projects = cfg.group_remote_projects(group);
                return self.federated_search(&request, &cfg, remote_projects).await;
            }
        }

        // Project-level federation (mounted remote project): a `project` of the
        // form "<peer>/<alias>" transparently routes to that single peer's own
        // `<alias>` project — a 1-to-1 passthrough, as if the index were local.
        // Local repos ALWAYS win a name clash: only route remotely when the name
        // does not resolve to a local project.
        if let Some(proj) = request.project.as_deref() {
            let cfg = self.federation_config();
            if cfg.resolve(proj).is_none() {
                if let Some(crate::db_discovery::repos::Target::RemoteProject {
                    peer_name,
                    peer,
                    remote_alias,
                }) = cfg.resolve_remote_project(proj)
                {
                    return self
                        .federated_project_search(&request, peer_name, peer, remote_alias)
                        .await;
                }
            }
        }

        let mode = request.mode.as_deref().unwrap_or("semantic").to_lowercase();
        match mode.as_str() {
            "semantic" => {
                // Delegate to the existing semantic_search implementation
                let semantic_req = SemanticSearchRequest {
                    query: request.query,
                    limit: request.limit,
                    compact: request.compact,
                    filter_path: request.filter_path,
                    mode: request.semantic_mode,
                    project: request.project,
                    group: request.group,
                };
                self.semantic_search(Parameters(semantic_req)).await
            }
            "literal" => {
                // Delegate to the existing literal_search implementation
                let literal_req = LiteralSearchRequest {
                    query: request.query,
                    regex: request.regex,
                    phrase: request.phrase,
                    limit: request.limit,
                    file_glob: request.file_glob,
                    language: request.language,
                    format: request.format,
                    project: request.project,
                    group: request.group,
                };
                self.literal_search(Parameters(literal_req)).await
            }
            _ => Ok(CallToolResult::success(vec![Content::text(format!(
                "Unknown search mode '{}'. Use `semantic` or `literal`.",
                mode
            ))])),
        }
    }

    /// Unified symbol navigation — dispatches based on `kind`.
    #[tool(
        description = "Unified symbol navigation. Set `kind` to choose the action:\n\n- `definition` (default): locate where a symbol is defined (function, class, struct, etc.)\n- `usages`: find call-sites of a symbol via LEXICAL TEXT matching — hits may be docs/comments rather than code references; ranking puts source files first and a `note` field flags the precise upgrade path when one exists. On C#/TypeScript projects ALWAYS prefer `find_impact` for usages — it returns exact SCIP references, while this kind is only a text fallback\n- `imports`: list all imports/dependencies declared in a file (set `symbol` to the file path)\n- `dependents`: find all files that import or depend on a module, file, or symbol\n\nFor `imports`, set `symbol` to a file path. For other kinds, `symbol` is the symbol name.\n\nIMPORTANT (multi-repo): always specify either `project` (single repo) or `group` (cross-repo). Omitting both in multi-repo mode returns a `scope_required` error with the list of available projects and groups. If the user has not indicated which repository to search, ask them to choose."
    )]
    async fn find(
        &self,
        Parameters(request): Parameters<FindRequest>,
    ) -> Result<CallToolResult, McpError> {
        let kind = request
            .kind
            .as_deref()
            .unwrap_or("definition")
            .to_lowercase();
        tracing::info!(
            "📥 find(symbol={:?}, kind={}, project={:?}, group={:?})",
            request.symbol,
            kind,
            request.project,
            request.group,
        );
        match kind.as_str() {
            "definition" => {
                let def_req = FindDefinitionRequest {
                    symbol: request.symbol,
                    kind: request.definition_kind,
                    limit: request.limit,
                    project: request.project,
                    group: request.group,
                };
                self.find_definition(Parameters(def_req)).await
            }
            "usages" => {
                let usages_req = FindUsagesRequest {
                    symbol: request.symbol,
                    limit: request.limit,
                    project: request.project,
                    group: request.group,
                };
                self.find_usages(Parameters(usages_req)).await
            }
            "imports" => {
                let imports_req = FindImportsRequest {
                    path: request.symbol,
                    project: request.project,
                    group: request.group,
                };
                self.find_imports(Parameters(imports_req)).await
            }
            "dependents" => {
                let dep_req = FindDependentsRequest {
                    symbol_or_path: request.symbol,
                    limit: request.limit,
                    project: request.project,
                    group: request.group,
                };
                self.find_dependents(Parameters(dep_req)).await
            }
            _ => Ok(CallToolResult::success(vec![Content::text(format!(
                "Unknown find kind '{}'. Use `definition`, `usages`, `imports`, or `dependents`.",
                kind
            ))])),
        }
    }

    /// Unified exploration tool — dispatches based on `kind`.
    #[tool(
        description = "Unified code exploration. Set `kind` to choose the action:\n\n- `outline` (default): list all indexed top-level symbols in a file — kind, signature, and line range. Set `target` to a file path.\n- `similar`: find chunks semantically similar to a given chunk by its ID. Set `target` to the chunk_id (as string).\n\nIMPORTANT (multi-repo): always specify either `project` (single repo) or `group` (cross-repo). Omitting both in multi-repo mode returns a `scope_required` error with the list of available projects and groups. If the user has not indicated which repository to search, ask them to choose."
    )]
    async fn explore(
        &self,
        Parameters(request): Parameters<ExploreRequest>,
    ) -> Result<CallToolResult, McpError> {
        let kind = request.kind.as_deref().unwrap_or("outline").to_lowercase();
        tracing::info!(
            "📥 explore(target={:?}, kind={}, project={:?})",
            request.target,
            kind,
            request.project,
        );
        match kind.as_str() {
            "outline" => {
                let outline_req = FileOutlineRequest {
                    path: request.target,
                    project: request.project,
                    group: request.group,
                };
                self.file_outline(Parameters(outline_req)).await
            }
            "similar" => {
                let chunk_id = match request.target.parse::<u32>() {
                    Ok(id) => id,
                    Err(_) => {
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "For similar mode, `target` must be a numeric chunk_id, got: '{}'",
                            request.target
                        ))]));
                    }
                };
                let similar_req = SimilarChunksRequest {
                    chunk_id,
                    limit: request.limit,
                    project: request.project,
                    group: request.group,
                };
                self.similar_chunks(Parameters(similar_req)).await
            }
            _ => Ok(CallToolResult::success(vec![Content::text(format!(
                "Unknown explore kind '{}'. Use `outline` or `similar`.",
                kind
            ))])),
        }
    }

    /// Unified status tool — dispatches based on `kind`.
    #[tool(
        description = "Unified status/info tool. Set `kind` to choose the action:\n\n- `index` (default): get the status of the local search index (model info, chunk count, readiness)\n- `projects`: list all registered projects/repositories, groups, and their index status"
    )]
    async fn status(
        &self,
        Parameters(request): Parameters<StatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        let kind = request.kind.as_deref().unwrap_or("index").to_lowercase();
        tracing::info!("📥 status(kind={})", kind);
        match kind.as_str() {
            "index" => self.index_status_impl(request.project, request.group).await,
            "projects" => self.list_projects().await,
            _ => Ok(CallToolResult::success(vec![Content::text(format!(
                "Unknown status kind '{}'. Use `index` or `projects`.",
                kind
            ))])),
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Internal implementations (called by consolidated tools above)
    // ─────────────────────────────────────────────────────────────────

    /// Internal: semantic/hybrid search implementation used by `search(mode="semantic")`.
    async fn semantic_search(
        &self,
        Parameters(request): Parameters<SemanticSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing (multi-store for group fan-out)
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "search")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        let limit = request.limit.unwrap_or(10);
        let compact = request.compact.unwrap_or(true);
        let mode = request.mode.as_deref().unwrap_or("auto");
        let identifiers = detect_identifiers(&request.query);
        let has_identifiers = !identifiers.is_empty();

        tracing::debug!(
            "MCP semantic_search: query='{}', limit={}, compact={}, mode='{}', multi={}",
            request.query,
            limit,
            compact,
            mode,
            ctx.is_multi
        );

        // Ensure database exists (skip if serve-mode with routed stores)
        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // === Multi-store group fan-out ===
        if ctx.is_multi {
            return self
                .semantic_search_multi(
                    &request,
                    &identifiers,
                    limit,
                    compact,
                    ctx.stores_vec.unwrap(),
                    ctx.store_aliases.as_ref().unwrap(),
                    &ctx.alias_roots,
                )
                .await;
        }

        // === Mode: "lexical" — FTS only, no embedding ===
        if mode == "lexical" {
            tracing::debug!("MCP: mode=lexical — skipping embedding service");
            return self
                .semantic_search_lexical(
                    &request,
                    &identifiers,
                    limit,
                    compact,
                    ctx.stores,
                    ctx.project_alias.as_deref(),
                    &ctx.alias_roots,
                )
                .await;
        }

        // === Modes: "semantic", "hybrid", "auto" — require embedding ===
        let query_embedding = {
            let mut service_guard = match self.get_embedding_service() {
                Ok(g) => g,
                Err(e) => {
                    tracing::error!("MCP: Failed to get embedding service: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error initializing embedding service: {e:#}"
                    ))]));
                }
            };

            let service = service_guard.as_mut().unwrap();
            tracing::debug!("MCP: Embedding query...");
            match service.embed_query(&request.query) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("MCP: Failed to embed query: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error embedding query: {e:#}"
                    ))]));
                }
            }
        };

        // Failures on this single-store path. The group fan-out has carried a
        // warnings channel since the read-only incident; without the same thing
        // here, `project=<alias>` — the form an agent uses most — still reports
        // a broken store as an ordinary empty result.
        let mut single_warnings: Vec<String> = Vec::new();

        // Search vector store
        let vector_results = match self
            .with_vector_store_read_for(
                |store| {
                    store
                        .search(&query_embedding, limit * 5)
                        .context("Error searching vector store")
                },
                ctx.stores.clone(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("MCP: Search failed: {:?}", e);
                // Only "semantic" has no second backend to fall back on. In
                // hybrid/auto the FTS half can still answer, so hard-failing
                // here would throw away good results — the same mistake this
                // branch already fixed once in the group fan-out.
                //
                // `{:#}` renders the whole anyhow chain. With plain `{}` the
                // caller only ever saw the outermost `.context(...)` wrapper
                // ("Error reading from project-routed vector store"), which
                // hides the actual fault and makes remote diagnosis guesswork.
                if mode == "semantic" {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching vector store: {:#}",
                        e
                    ))]));
                }
                single_warnings.push(format!("vector search failed: {e:#}"));
                Vec::new()
            }
        };

        tracing::debug!("MCP: Found {} vector results", vector_results.len());

        // === Mode: "semantic" — vector only, skip FTS fusion ===
        if mode == "semantic" {
            tracing::debug!("MCP: mode=semantic — using vector results only");
            let fused = vector_only(&vector_results);

            let chunk_to_result: std::collections::HashMap<u32, &crate::vectordb::SearchResult> =
                vector_results.iter().map(|r| (r.id, r)).collect();

            let mut results: Vec<crate::vectordb::SearchResult> = Vec::new();
            for f in fused.into_iter().take(limit) {
                if let Some(result) = chunk_to_result.get(&f.chunk_id) {
                    let mut r = (*result).clone();
                    r.score = f.rrf_score;
                    results.push(r);
                }
            }
            return self.build_semantic_response(
                results,
                &request,
                compact,
                has_identifiers,
                ctx.project_alias.as_deref(),
                &ctx.alias_roots,
                &single_warnings,
            );
        }

        // === Modes: "hybrid" | "auto" — full hybrid search ===
        let structural_intent = detect_structural_intent(&request.query);
        let (vector_k, fts_k) = adapt_rrf_k(&request.query);

        tracing::debug!(
            "MCP: Query analysis - identifiers: {:?}, structural_intent: {:?}, rrf_k: ({}, {})",
            identifiers,
            structural_intent,
            vector_k,
            fts_k
        );

        // Perform FTS search and fusion
        let mut results = match self
            .with_fts_store_read_for(
                |fts_store| {
                    let fts_results = fts_store
                        .search(&request.query, limit * 5, structural_intent)
                        .context("Error searching FTS store")?;

                    let fused = if identifiers.is_empty() {
                        rrf_fusion(&vector_results, &fts_results, vector_k as f32)
                    } else {
                        let mut all_exact: Vec<crate::fts::FtsResult> = Vec::new();
                        for ident in &identifiers {
                            if let Ok(exact) =
                                fts_store.search_exact(ident, limit * 3, structural_intent)
                            {
                                for r in exact {
                                    if !all_exact.iter().any(|e| e.chunk_id == r.chunk_id) {
                                        all_exact.push(r);
                                    }
                                }
                            }
                        }

                        tracing::debug!(
                            "MCP: FTS found {} results, exact found {} results",
                            fts_results.len(),
                            all_exact.len()
                        );

                        rrf_fusion_with_exact(
                            &vector_results,
                            &fts_results,
                            &all_exact,
                            vector_k as f32,
                            fts_k as f32,
                            EXACT_MATCH_RRF_K,
                        )
                    };

                    Ok(fused)
                },
                ctx.stores.clone(),
            )
            .await
        {
            Ok(fused) => {
                // Map FusedResult back to SearchResult
                let chunk_to_result: std::collections::HashMap<
                    u32,
                    &crate::vectordb::SearchResult,
                > = vector_results.iter().map(|r| (r.id, r)).collect();

                let mut mapped: Vec<crate::vectordb::SearchResult> = Vec::new();
                for f in fused.into_iter().take(limit) {
                    if let Some(result) = chunk_to_result.get(&f.chunk_id) {
                        let mut r = (*result).clone();
                        r.score = f.rrf_score;
                        mapped.push(r);
                    }
                }
                mapped
            }
            Err(e) => {
                tracing::warn!("MCP: FTS store unavailable, using vector-only: {:?}", e);
                // Degrading to vector-only is correct, but it must be VISIBLE:
                // a caller that gets half a hybrid search with no signal cannot
                // tell it from a complete one.
                single_warnings.push(format!("lexical (FTS) search failed: {e:#}"));
                vector_results.into_iter().take(limit).collect()
            }
        };

        // Apply language boost
        if let Some((_, _, Some(primary_lang))) = crate::search::read_metadata(&self.db_path) {
            for result in &mut results {
                let file_lang = format!(
                    "{:?}",
                    Language::from_path(std::path::Path::new(&result.path))
                );
                if file_lang.to_lowercase() == primary_lang.to_lowercase() {
                    result.score *= 1.2;
                }
            }
            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }

        // Apply kind boost
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut results, target_kind);
        }

        // Auto-fallback: if hybrid search returned very few results for a code-like query,
        // run literal FTS and merge missing chunks.
        if results.len() < 3 && has_identifiers {
            tracing::debug!(
                "Auto-fallback: semantic returned {} results, trying literal",
                results.len()
            );

            let literal_results = self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search(&request.query, limit, None),
                    ctx.stores.clone(),
                )
                .await
                .unwrap_or_default();

            let mut existing_ids: std::collections::HashSet<u32> =
                results.iter().map(|r| r.id).collect();

            for fts in literal_results {
                if results.len() >= limit {
                    break;
                }
                if existing_ids.contains(&fts.chunk_id) {
                    continue;
                }

                let maybe_resolved = match self
                    .with_vector_store_read_for(
                        |store| {
                            // `Ok(None)` means "this store does not hold that
                            // chunk" — a normal miss to skip. `Err` means the
                            // store is broken and must propagate: flattening
                            // the two silently dropped every remaining literal
                            // hit whenever the vector store was down, turning a
                            // dead store into an ordinary-looking short result.
                            let chunk = match store.get_chunk(fts.chunk_id)? {
                                Some(c) => c,
                                None => return Ok(None),
                            };
                            Ok(Some(crate::vectordb::SearchResult {
                                id: fts.chunk_id,
                                content: chunk.content,
                                path: chunk.path,
                                start_line: chunk.start_line,
                                end_line: chunk.end_line,
                                kind: chunk.kind,
                                signature: chunk.signature,
                                docstring: chunk.docstring,
                                context: chunk.context,
                                hash: chunk.hash,
                                distance: 0.0,
                                score: fts.score,
                                context_prev: chunk.context_prev,
                                context_next: chunk.context_next,
                            }))
                        },
                        ctx.stores.clone(),
                    )
                    .await
                {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        // The old `.ok()` here folded a dead store into "no
                        // more literal hits" with zero signal to the caller —
                        // the exact false negative `single_warnings` exists
                        // for. Note it and stop: every further lookup against
                        // this store would fail the same way.
                        single_warnings.push(format!("literal-hit chunk lookup failed: {e:#}"));
                        break;
                    }
                };

                if let Some(resolved) = maybe_resolved {
                    existing_ids.insert(resolved.id);
                    results.push(resolved);
                }
            }
        }

        tracing::debug!("MCP: Final {} results after hybrid search", results.len());
        self.build_semantic_response(
            results,
            &request,
            compact,
            has_identifiers,
            ctx.project_alias.as_deref(),
            &ctx.alias_roots,
            &single_warnings,
        )
    }

    // === Helper methods (not exposed as tools) ===

    /// Multi-store semantic search: fan out across all stores, merge raw vector/FTS
    /// results, then apply RRF fusion.
    #[allow(clippy::too_many_arguments)]
    async fn semantic_search_multi(
        &self,
        request: &SemanticSearchRequest,
        identifiers: &[String],
        limit: usize,
        compact: bool,
        stores: Vec<Arc<SharedStores>>,
        aliases: &[String],
        alias_roots: &std::collections::HashMap<String, String>,
    ) -> Result<CallToolResult, McpError> {
        let mode = request.mode.as_deref().unwrap_or("auto");
        let structural_intent = detect_structural_intent(&request.query);

        // === Lexical mode: FTS only across all stores ===
        if mode == "lexical" {
            // Lexical has no second backend, so a failed store here is invisible
            // unless it is reported: the query simply looks like it found nothing.
            let mut lexical_warnings: Vec<String> = Vec::new();

            let outcome = self
                .with_fts_store_read_multi(
                    |fts_store| fts_store.search(&request.query, limit * 5, structural_intent),
                    stores.clone(),
                    aliases,
                )
                .await
                .unwrap_or_default();
            if !outcome.failures.is_empty() {
                tracing::error!(
                    "MCP: lexical fan-out degraded — {} of {} repo(s) failed: {:?}",
                    outcome.failures.len(),
                    stores.len(),
                    outcome.failures
                );
                lexical_warnings.extend(outcome.warnings("literal search"));
            }
            let fts_results = outcome.results;

            // Also do exact search if identifiers detected
            let mut all_fts = fts_results;
            for ident in identifiers {
                let exact_outcome = self
                    .with_fts_store_read_multi(
                        |fts_store| fts_store.search_exact(ident, limit * 3, structural_intent),
                        stores.clone(),
                        aliases,
                    )
                    .await
                    .unwrap_or_default();
                lexical_warnings.extend(exact_outcome.warnings("exact-identifier search"));
                merge_exact_into_fts(&mut all_fts, exact_outcome.results);
            }

            all_fts.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let results = self
                .resolve_fts_to_search_results_multi(
                    &all_fts,
                    limit,
                    &stores,
                    aliases,
                    &mut lexical_warnings,
                )
                .await;

            if let Some(target_kind) = structural_intent {
                // We need mutable results but we have them as vectordb::SearchResult
                let mut mutable_results = results;
                boost_kind(&mut mutable_results, target_kind);
                return self.build_semantic_response(
                    mutable_results,
                    request,
                    compact,
                    !identifiers.is_empty(),
                    None,
                    alias_roots,
                    &lexical_warnings,
                );
            }

            return self.build_semantic_response(
                results,
                request,
                compact,
                !identifiers.is_empty(),
                None,
                alias_roots,
                &lexical_warnings,
            );
        }

        // === Modes requiring embedding: "semantic", "hybrid", "auto" ===
        let query_embedding = {
            let mut service_guard = match self.get_embedding_service() {
                Ok(g) => g,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error initializing embedding service: {e:#}"
                    ))]));
                }
            };
            let service = service_guard.as_mut().unwrap();
            match service.embed_query(&request.query) {
                Ok(e) => e,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error embedding query: {e:#}"
                    ))]));
                }
            }
        };

        // Search vector stores across all repos
        let outcome = self
            .with_vector_store_read_multi(
                |store| {
                    store
                        .search(&query_embedding, limit * 5)
                        .context("Error searching vector store")
                },
                stores.clone(),
                aliases,
            )
            .await;

        // Warnings raised by the fan-out, carried into the response so the
        // calling agent can tell "not in the corpus" from "that repo is down".
        let mut search_warnings: Vec<String> = Vec::new();

        let vector_results =
            match outcome {
                Ok(o) => {
                    if !o.failures.is_empty() {
                        tracing::error!(
                            "MCP: vector fan-out degraded — {} of {} repo(s) failed: {:?}",
                            o.failures.len(),
                            stores.len(),
                            o.failures
                        );
                        // Only "semantic" has no second backend to fall back on. In
                        // hybrid/auto/lexical the FTS half can still answer, so
                        // hard-failing here would throw away good results — the same
                        // reason one broken repo does not abort the whole fan-out.
                        if mode == "semantic" && o.results.is_empty() {
                            let detail = o
                                .failures
                                .iter()
                                .map(|(alias, err)| format!("  - {alias}: {err}"))
                                .collect::<Vec<_>>()
                                .join("\n");
                            return Ok(CallToolResult::success(vec![Content::text(format!(
                                "Error searching vector store: {} of {} repo(s) in scope failed \
                             and none returned results:\n{}",
                                o.failures.len(),
                                stores.len(),
                                detail
                            ))]));
                        }
                        search_warnings.extend(o.failures.iter().map(|(alias, err)| {
                            format!("repo '{alias}' vector search failed: {err}")
                        }));
                    }
                    o.results
                }
                Err(e) => {
                    tracing::error!("MCP: vector fan-out failed: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching vector store: {e:#}"
                    ))]));
                }
            };

        // === Mode: "semantic" — vector only ===
        if mode == "semantic" {
            let fused = vector_only(&vector_results);
            let chunk_to_result: std::collections::HashMap<u32, &crate::vectordb::SearchResult> =
                vector_results.iter().map(|r| (r.id, r)).collect();

            let mut results: Vec<crate::vectordb::SearchResult> = Vec::new();
            for f in fused.into_iter().take(limit) {
                if let Some(result) = chunk_to_result.get(&f.chunk_id) {
                    let mut r = (*result).clone();
                    r.score = f.rrf_score;
                    results.push(r);
                }
            }
            return self.build_semantic_response(
                results,
                request,
                compact,
                !identifiers.is_empty(),
                None,
                alias_roots,
                &search_warnings,
            );
        }

        // === Modes: "hybrid" | "auto" — full hybrid search ===
        let (vector_k, fts_k) = adapt_rrf_k(&request.query);

        // FTS search across all stores. Its failures matter as much as the
        // vector half's: during the cloud read-only incident literal search
        // also returned 0 results for every affected vendor, and looked clean.
        let fts_outcome = self
            .with_fts_store_read_multi(
                |fts_store| fts_store.search(&request.query, limit * 5, structural_intent),
                stores.clone(),
                aliases,
            )
            .await
            .unwrap_or_default();
        if !fts_outcome.failures.is_empty() {
            tracing::error!(
                "MCP: FTS fan-out degraded — {} of {} repo(s) failed: {:?}",
                fts_outcome.failures.len(),
                stores.len(),
                fts_outcome.failures
            );
            search_warnings.extend(fts_outcome.warnings("literal search"));
        }
        let fts_results = fts_outcome.results;

        // Exact identifier search across all stores
        let all_exact = if !identifiers.is_empty() {
            let mut exact_results: Vec<crate::fts::FtsResult> = Vec::new();
            for ident in identifiers {
                let exact_outcome = self
                    .with_fts_store_read_multi(
                        |fts_store| fts_store.search_exact(ident, limit * 3, structural_intent),
                        stores.clone(),
                        aliases,
                    )
                    .await
                    .unwrap_or_default();
                search_warnings.extend(exact_outcome.warnings("exact-identifier search"));
                for r in exact_outcome.results {
                    if !exact_results.iter().any(|e| e.chunk_id == r.chunk_id) {
                        exact_results.push(r);
                    }
                }
            }
            exact_results
        } else {
            Vec::new()
        };

        // RRF fusion
        let fused = if identifiers.is_empty() {
            rrf_fusion(&vector_results, &fts_results, vector_k as f32)
        } else {
            rrf_fusion_with_exact(
                &vector_results,
                &fts_results,
                &all_exact,
                vector_k as f32,
                fts_k as f32,
                EXACT_MATCH_RRF_K,
            )
        };

        // Map FusedResult back to SearchResult via chunk lookup across all stores
        let chunk_to_result: std::collections::HashMap<u32, &crate::vectordb::SearchResult> =
            vector_results.iter().map(|r| (r.id, r)).collect();

        let mut mapped: Vec<crate::vectordb::SearchResult> = Vec::new();
        for f in fused.into_iter().take(limit) {
            if let Some(result) = chunk_to_result.get(&f.chunk_id) {
                let mut r = (*result).clone();
                r.score = f.rrf_score;
                mapped.push(r);
            } else {
                // Chunk from FTS but not in vector results — resolve from stores
                if let Some(resolved) = self
                    .resolve_chunk_from_stores(
                        f.chunk_id,
                        f.rrf_score,
                        &stores,
                        aliases,
                        &mut search_warnings,
                    )
                    .await
                {
                    mapped.push(resolved);
                }
            }
        }

        // Apply kind boost
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut mapped, target_kind);
        }

        self.build_semantic_response(
            mapped,
            request,
            compact,
            !identifiers.is_empty(),
            None,
            alias_roots,
            &search_warnings,
        )
    }

    /// Resolve a single chunk from multiple stores (used for FTS-only hits in multi-store fusion).
    async fn resolve_chunk_from_stores(
        &self,
        chunk_id: u32,
        score: f32,
        stores: &[Arc<SharedStores>],
        aliases: &[String],
        warnings: &mut Vec<String>,
    ) -> Option<crate::vectordb::SearchResult> {
        for (idx, store_arc) in stores.iter().enumerate() {
            let store = store_arc.vector_store.read().await;
            let looked_up = store.get_chunk(chunk_id);
            if let Err(ref e) = looked_up {
                note_store_failure(warnings, aliases, idx, "chunk lookup", e);
            }
            if let Ok(Some(chunk)) = looked_up {
                return Some(crate::vectordb::SearchResult {
                    id: chunk_id,
                    content: chunk.content,
                    path: chunk.path,
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    kind: chunk.kind,
                    signature: chunk.signature,
                    docstring: chunk.docstring,
                    context: chunk.context,
                    hash: chunk.hash,
                    distance: 0.0,
                    score,
                    context_prev: chunk.context_prev,
                    context_next: chunk.context_next,
                });
            }
        }
        None
    }

    /// Resolve FTS results to SearchResult using multiple stores.
    async fn resolve_fts_to_search_results_multi(
        &self,
        fts_results: &[crate::fts::FtsResult],
        limit: usize,
        stores: &[Arc<SharedStores>],
        aliases: &[String],
        warnings: &mut Vec<String>,
    ) -> Vec<crate::vectordb::SearchResult> {
        let mut results = Vec::new();
        for fts in fts_results.iter().take(limit) {
            for (idx, store_arc) in stores.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                let looked_up = store.get_chunk(fts.chunk_id);
                if let Err(ref e) = looked_up {
                    // `Ok(None)` means "this store does not hold that chunk" and
                    // is normal during fan-out; `Err` means the store is broken.
                    // Collapsing the two is how a dead vector store renders as
                    // an empty literal search — the exact shape of the step-8
                    // incident, which tantivy-side checks cannot detect.
                    note_store_failure(warnings, aliases, idx, "chunk lookup", e);
                }
                if let Ok(Some(chunk)) = looked_up {
                    results.push(crate::vectordb::SearchResult {
                        id: fts.chunk_id,
                        content: chunk.content,
                        path: chunk.path,
                        start_line: chunk.start_line,
                        end_line: chunk.end_line,
                        kind: chunk.kind,
                        signature: chunk.signature,
                        docstring: chunk.docstring,
                        context: chunk.context,
                        hash: chunk.hash,
                        distance: 0.0,
                        score: fts.score,
                        context_prev: chunk.context_prev,
                        context_next: chunk.context_next,
                    });
                    break; // Found in this store, skip remaining stores
                }
            }
        }
        results
    }

    /// Lexical-only search: FTS without embedding service.
    #[allow(clippy::too_many_arguments)]
    async fn semantic_search_lexical(
        &self,
        request: &SemanticSearchRequest,
        identifiers: &[String],
        limit: usize,
        compact: bool,
        stores: Option<Arc<SharedStores>>,
        project_alias: Option<&str>,
        alias_roots: &std::collections::HashMap<String, String>,
    ) -> Result<CallToolResult, McpError> {
        let structural_intent = detect_structural_intent(&request.query);

        // `project=`-scoped queries route here, not through the fan-out
        // (`is_multi` requires >1 store), so this path needs the same failure
        // reporting — it is at least as common as a group query.
        let mut lexical_warnings: Vec<String> = Vec::new();

        let mut fts_results = match self
            .with_fts_store_read_for(
                |fts_store| fts_store.search(&request.query, limit * 5, structural_intent),
                stores.clone(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("literal search failed: {e:#}");
                tracing::error!("MCP: {}", msg);
                lexical_warnings.push(msg);
                Vec::new()
            }
        };

        // Also do exact search if identifiers detected
        for ident in identifiers {
            let exact = match self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search_exact(ident, limit * 3, structural_intent),
                    stores.clone(),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("exact-identifier search for '{ident}' failed: {e:#}");
                    tracing::error!("MCP: {}", msg);
                    lexical_warnings.push(msg);
                    continue;
                }
            };
            merge_exact_into_fts(&mut fts_results, exact);
        }

        fts_results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Resolve FTS results to chunk metadata
        let mut results = self
            .resolve_fts_to_search_results(&fts_results, limit, stores, &mut lexical_warnings)
            .await;

        // Apply kind boost
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut results, target_kind);
        }

        self.build_semantic_response(
            results,
            request,
            compact,
            !identifiers.is_empty(),
            project_alias,
            alias_roots,
            &lexical_warnings,
        )
    }

    /// Build the final SemanticSearchResponse with low-confidence signaling.
    // Eight parameters, one over clippy's threshold. Bundling them into a
    // `ResponseContext` struct is the right end state and is recorded as a
    // follow-up; doing it in an incident fix would touch all seven call sites
    // for no behavioural gain. The alternative — dropping `warnings` — is not
    // acceptable: without it a failed repo is silently reported as "no match".
    #[allow(clippy::too_many_arguments)]
    fn build_semantic_response(
        &self,
        results: Vec<crate::vectordb::SearchResult>,
        request: &SemanticSearchRequest,
        compact: bool,
        has_identifiers: bool,
        project_alias: Option<&str>,
        alias_roots: &std::collections::HashMap<String, String>,
        // Repos that failed during a fan-out. MUST reach the caller: the
        // consumer of this tool is a remote agent that never sees the server
        // log, so a silently omitted repo reads as "no match there" — a false
        // negative. The federated path already does this (`warnings` on the
        // remote-project fan-out); the local path never could.
        warnings: &[String],
    ) -> Result<CallToolResult, McpError> {
        let warnings = if warnings.is_empty() {
            None
        } else {
            Some(warnings.to_vec())
        };
        if results.is_empty() {
            let response = SemanticSearchResponse {
                results: vec![],
                low_confidence: Some(true),
                suggested_tool: retry_hint(Some("literal_search".to_string()), &warnings),
                warnings,
            };
            let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        // Pre-compute normalized project root for stripping absolute paths
        let project_root_normalized = {
            let root = crate::cache::normalize_path_str(self.project_path.to_str().unwrap_or(""));
            root.trim_end_matches('/').to_string()
        };

        let mut items: Vec<SearchResultItem> = results
            .into_iter()
            .filter(|r| {
                if let Some(ref fp) = request.filter_path {
                    let normalized_filter = crate::cache::normalize_filter_path(fp);
                    if normalized_filter.is_empty() {
                        return true;
                    }
                    // Relativise against the ROUTED project's root, not the
                    // service's own project_path — otherwise a serve-routed
                    // absolute path never strips and every hit is dropped.
                    let filter_root = pick_filter_root(
                        &r.path,
                        project_alias,
                        alias_roots,
                        &project_root_normalized,
                    );
                    crate::cache::path_matches_filter(&r.path, &normalized_filter, &filter_root)
                } else {
                    true
                }
            })
            .map(|r| SearchResultItem {
                chunk_id: Some(r.id),
                path: r.path,
                start_line: r.start_line,
                end_line: r.end_line,
                kind: r.kind,
                score: r.score,
                signature: r.signature,
                content: if compact { None } else { Some(r.content) },
                context_prev: if compact { None } else { r.context_prev },
                context_next: if compact { None } else { r.context_next },
                source: None,
                chunk_ref: None,
            })
            .collect();

        // Prefix paths with alias for multi-repo / single-project identification
        for item in &mut items {
            if let Some(alias) = project_alias {
                if let Some(root) = alias_roots.get(alias) {
                    item.path = prefix_path_with_alias(&item.path, Some(alias), root);
                } else {
                    item.path = crate::cache::normalize_path_str(&item.path);
                }
            } else if !alias_roots.is_empty() {
                item.path = prefix_path_multi(&item.path, &[], alias_roots);
            }
        }

        // Check low-confidence: top result's RRF score below threshold
        let top_score = items.first().map(|r| r.score);
        let (low_confidence, suggested_tool) = compute_low_confidence(top_score, has_identifiers);
        let suggested_tool = retry_hint(suggested_tool, &warnings);

        let response = SemanticSearchResponse {
            results: items,
            low_confidence,
            suggested_tool,
            warnings,
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Resolve FTS results to SearchResult by looking up chunk metadata.
    async fn resolve_fts_to_search_results(
        &self,
        fts_results: &[crate::fts::FtsResult],
        limit: usize,
        stores: Option<Arc<SharedStores>>,
        warnings: &mut Vec<String>,
    ) -> Vec<crate::vectordb::SearchResult> {
        let outcome = self
            .with_vector_store_read_for(
                |store| {
                    let mut results = Vec::new();
                    for fts in fts_results.iter().take(limit) {
                        // A failed lookup is not an absent chunk. Propagating the
                        // error keeps a broken vector store from rendering as an
                        // ordinary empty literal search.
                        let chunk = store
                            .get_chunk(fts.chunk_id)
                            .context("Error resolving FTS hit to chunk metadata")?;
                        if let Some(chunk) = chunk {
                            results.push(crate::vectordb::SearchResult {
                                id: fts.chunk_id,
                                content: chunk.content,
                                path: chunk.path,
                                start_line: chunk.start_line,
                                end_line: chunk.end_line,
                                kind: chunk.kind,
                                signature: chunk.signature,
                                docstring: chunk.docstring,
                                context: chunk.context,
                                hash: chunk.hash,
                                distance: 0.0,
                                score: fts.score,
                                context_prev: chunk.context_prev,
                                context_next: chunk.context_next,
                            });
                        }
                    }
                    Ok(results)
                },
                stores,
            )
            .await;
        match outcome {
            Ok(results) => results,
            Err(e) => {
                let msg = format!("literal search could not read the index: {e:#}");
                tracing::error!("MCP: {}", msg);
                if !warnings.contains(&msg) {
                    warnings.push(msg);
                }
                Vec::new()
            }
        }
    }

    // === find_definition internal ===

    /// Internal: find symbol definitions, used by `find(kind="definition")`.
    async fn find_definition(
        &self,
        Parameters(request): Parameters<FindDefinitionRequest>,
    ) -> Result<CallToolResult, McpError> {
        let limit = request.limit.unwrap_or(20);

        tracing::debug!(
            "MCP find_definition: symbol='{}', kind={:?}, limit={}",
            request.symbol,
            request.kind,
            limit
        );

        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "find")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // Stores that failed during this lookup. Without this, "the symbol may
        // not be indexed" below is emitted as a confident diagnosis even when
        // no store ever answered.
        let mut find_warnings: Vec<String> = Vec::new();

        // FTS search — multi-store or single
        let fts_results = if let Some(ref sv) = ctx.stores_vec {
            let sa = ctx.store_aliases.as_ref().unwrap();
            self.with_fts_store_read_multi(
                |fts_store| fts_store.search(&request.symbol, limit * 3, None),
                sv.clone(),
                sa,
            )
            .await
            .unwrap_or_default()
            .into_results(&mut find_warnings, "definition search")
        } else {
            match self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search(&request.symbol, limit * 3, None),
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching: {e:#}"
                    ))]));
                }
            }
        };

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                qualify_empty_result(
                    format!(
                        "No definition found for '{}'. The symbol may not be indexed.",
                        request.symbol
                    ),
                    &find_warnings,
                ),
            )]));
        }

        // Resolve chunk metadata and filter by definition kinds
        let requested_kind = request.kind.clone();
        let mut items: Vec<ReferenceItem> = if let Some(ref sv) = ctx.stores_vec {
            let aliases = ctx.aliases();
            let mut items: Vec<ReferenceItem> = Vec::new();
            'outer: for fts_result in &fts_results {
                for (store_idx, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    let looked_up = store.get_chunk(fts_result.chunk_id);
                    if let Err(ref e) = looked_up {
                        // `Ok(None)` = chunk not in this store (normal during
                        // fan-out); `Err` = broken store. Skipping the `Err`
                        // silently made a dead store look like "symbol not
                        // found" — carry it in the warnings channel instead.
                        note_store_failure(
                            &mut find_warnings,
                            aliases,
                            store_idx,
                            "chunk lookup",
                            e,
                        );
                    }
                    if let Ok(Some(chunk)) = looked_up {
                        // Skip non-definition kinds — try next FTS result, not next store
                        if !DEFINITION_KINDS.contains(&chunk.kind.as_str()) {
                            continue 'outer;
                        }
                        if let Some(ref rk) = requested_kind {
                            if chunk.kind != *rk {
                                continue 'outer;
                            }
                        }
                        items.push(ReferenceItem {
                            chunk_id: fts_result.chunk_id,
                            path: chunk.path,
                            line: chunk.start_line,
                            kind: chunk.kind,
                            signature: chunk.signature,
                            score: fts_result.score,
                        });
                        if items.len() >= limit {
                            break 'outer;
                        }
                        break; // Found in this store — move to next FTS result
                    }
                }
                // If we get here, the chunk was Ok(None) in every store (not
                // held anywhere — skip it) or its lookups failed (noted in
                // find_warnings above).
            }
            items
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        // Resolve chunk metadata first: a store `Err` must
                        // reach the error arm below ("Error opening database")
                        // instead of masquerading as a non-definition or
                        // missing chunk — `Ok(None)` alone is a true miss.
                        let resolved: anyhow::Result<Vec<_>> = fts_results
                            .iter()
                            .map(|fts_result| {
                                let chunk = store.get_chunk(fts_result.chunk_id)?;
                                Ok((chunk, fts_result.chunk_id, fts_result.score))
                            })
                            .collect();
                        let items = resolved?
                            .into_iter()
                            .filter_map(|(looked_up, chunk_id, score)| {
                                let chunk = looked_up?;
                                if !DEFINITION_KINDS.contains(&chunk.kind.as_str()) {
                                    return None;
                                }
                                if let Some(ref requested_kind) = requested_kind {
                                    if chunk.kind != *requested_kind {
                                        return None;
                                    }
                                }
                                Some(ReferenceItem {
                                    chunk_id,
                                    path: chunk.path,
                                    line: chunk.start_line,
                                    kind: chunk.kind,
                                    signature: chunk.signature,
                                    score,
                                })
                            })
                            .take(limit)
                            .collect();
                        Ok(items)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error opening database: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        respond_with_items(&items, &find_warnings, || {
            format!(
                "No definition found for '{}'. Try find_usages() to find references, \
                 or broaden your search.",
                request.symbol
            )
        })
    }

    // === find_usages tool ===

    async fn find_usages(
        &self,
        Parameters(request): Parameters<FindUsagesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.find_usages_impl(
            request.symbol.clone(),
            request.limit.unwrap_or(20),
            request.project,
            request.group,
        )
        .await
    }

    /// Shared implementation for find_usages (used by `find(kind="usages")`).
    async fn find_usages_impl(
        &self,
        symbol: String,
        limit: usize,
        project: Option<String>,
        group: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        tracing::debug!("MCP find_usages: symbol='{}', limit={}", symbol, limit);

        // Resolve project/group routing
        let ctx = match self.resolve_routing(&project, &group, false, "find").await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // See `find_definition`: an empty result and a dead store must not
        // produce the same sentence.
        let mut find_warnings: Vec<String> = Vec::new();

        // FTS search — multi-store or single
        let fts_results = if let Some(ref sv) = ctx.stores_vec {
            let sa = ctx.store_aliases.as_ref().unwrap();
            self.with_fts_store_read_multi(
                |fts_store| fts_store.search(&symbol, limit * 2, None),
                sv.clone(),
                sa,
            )
            .await
            .unwrap_or_default()
            .into_results(&mut find_warnings, "usage search")
        } else {
            match self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search(&symbol, limit * 2, None),
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching: {e:#}"
                    ))]));
                }
            }
        };

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                qualify_empty_result(
                    format!("No usages found for '{symbol}'. The symbol may not be indexed."),
                    &find_warnings,
                ),
            )]));
        }

        // Resolve chunks and exclude definition chunks
        let mut items: Vec<ReferenceItem> = if let Some(ref sv) = ctx.stores_vec {
            let aliases = ctx.aliases();
            let mut items: Vec<ReferenceItem> = Vec::new();
            for fts_result in &fts_results {
                for (store_idx, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    let looked_up = store.get_chunk(fts_result.chunk_id);
                    if let Err(ref e) = looked_up {
                        // Same rule as find_definition: `Err` is a broken
                        // store, not "no usages" — carry it in the channel.
                        note_store_failure(
                            &mut find_warnings,
                            aliases,
                            store_idx,
                            "chunk lookup",
                            e,
                        );
                    }
                    if let Ok(Some(chunk)) = looked_up {
                        if !is_definition_chunk(&chunk.kind, &chunk.signature, &symbol) {
                            items.push(ReferenceItem {
                                chunk_id: fts_result.chunk_id,
                                path: chunk.path,
                                line: chunk.start_line,
                                kind: chunk.kind,
                                signature: chunk.signature,
                                score: fts_result.score,
                            });
                        }
                        break;
                    }
                }
            }
            items
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        // Resolve first: a store `Err` must reach the error
                        // arm below instead of masquerading as a definition
                        // chunk or a miss — `Ok(None)` alone is a true miss.
                        let resolved: anyhow::Result<Vec<_>> = fts_results
                            .iter()
                            .map(|fts_result| {
                                let chunk = store.get_chunk(fts_result.chunk_id)?;
                                Ok((chunk, fts_result.chunk_id, fts_result.score))
                            })
                            .collect();
                        let items = resolved?
                            .into_iter()
                            .filter_map(|(looked_up, chunk_id, score)| {
                                let chunk = looked_up?;
                                if is_definition_chunk(&chunk.kind, &chunk.signature, &symbol) {
                                    return None;
                                }
                                Some(ReferenceItem {
                                    chunk_id,
                                    path: chunk.path,
                                    line: chunk.start_line,
                                    kind: chunk.kind,
                                    signature: chunk.signature,
                                    score,
                                })
                            })
                            .collect();
                        Ok(items)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error opening database: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        // Lexical FTS ranks docs, comments and code by the same text score,
        // so a usages query can bury the real call-sites under markdown. The
        // cut to `limit` MUST happen after the re-order: BM25 systematically
        // scores short markdown blocks above long source files (document
        // length normalisation), so ranking after a `take(limit)` would have
        // nothing left to reorder exactly when it matters most. Stable sort
        // preserves score order within each group; nothing is filtered.
        rank_code_first(&mut items);
        items.truncate(limit);

        // When the hits include SCIP-backed source files and a precise
        // backend is installed, tell the agent the exact upgrade path —
        // otherwise the lexical list silently stands in for real references.
        let note = scip_usages_note(&self.symbol_registry, &items, &symbol);

        respond_with_items_noted(&items, &find_warnings, note.as_deref(), || {
            format!(
                "No usages found for '{symbol}' (only definitions were found). Try \
                 find_definition() to locate the declaration."
            )
        })
    }

    /// Fetch outline items for an already-normalised absolute path.
    ///
    /// Returns `Ok(vec![])` when no chunks match.
    /// In multi-store mode, per-store I/O failures are recorded in `warnings` and
    /// skipped (never `Err`) so one broken repo cannot blank the whole outline.
    /// In single-store mode, I/O failures are returned as `Err`.
    ///
    /// `warnings` is not optional: without it a failed store is indistinguishable
    /// from a file with no indexed chunks, and the caller is told the file is not
    /// indexed — a diagnosis, and a wrong one.
    async fn outline_items_for_normalized(
        &self,
        normalized: &str,
        ctx: &MultiStoreContext,
        warnings: &mut Vec<String>,
    ) -> anyhow::Result<Vec<FileOutlineItem>> {
        if let Some(ref sv) = ctx.stores_vec {
            let aliases = ctx.aliases();
            let mut all_items: Vec<FileOutlineItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for (store_idx, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.chunks_for_file(normalized) {
                    Ok(metas) => {
                        for c in metas {
                            if seen_ids.insert(c.id) {
                                all_items.push(FileOutlineItem {
                                    chunk_id: c.id,
                                    kind: c.kind,
                                    signature: c.signature,
                                    start_line: c.start_line,
                                    end_line: c.end_line,
                                });
                            }
                        }
                    }
                    Err(ref e) => {
                        note_store_failure(warnings, aliases, store_idx, "outline scan", e);
                    }
                }
            }
            all_items.sort_by_key(|i| i.start_line);
            Ok(all_items)
        } else {
            let normalized_owned = normalized.to_string();
            self.with_vector_store_read_for(
                move |store| {
                    let mut out: Vec<FileOutlineItem> = store
                        .chunks_for_file(&normalized_owned)?
                        .into_iter()
                        .map(|c| FileOutlineItem {
                            chunk_id: c.id,
                            kind: c.kind,
                            signature: c.signature,
                            start_line: c.start_line,
                            end_line: c.end_line,
                        })
                        .collect();
                    out.sort_by_key(|i| i.start_line);
                    Ok(out)
                },
                ctx.stores.clone(),
            )
            .await
        }
    }

    async fn file_outline(
        &self,
        Parameters(request): Parameters<FileOutlineRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "explore")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        // Outline operates on a single repo — reject group fan-out
        if ctx.is_multi {
            return Ok(CallToolResult::success(vec![Content::text(
                "Tool 'explore' operates on a single repo. Use 'project' instead of 'group'."
                    .to_string(),
            )]));
        }

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // In serve mode, use the resolved project root from alias_roots;
        // self.project_path is "serve://multi-repo" which doesn't resolve.
        let project_root = if let Some(ref alias) = ctx.project_alias {
            ctx.alias_roots
                .get(alias)
                .map(PathBuf::from)
                .unwrap_or_else(|| self.project_path.clone())
        } else {
            self.project_path.clone()
        };
        // Strip project-alias prefix from target path if present.
        // E.g. "ExampleRepo/src/foo.cs" with project="ExampleRepo" → "src/foo.cs"
        let stripped_path = strip_alias_prefix(&request.path, ctx.project_alias.as_ref());
        let normalized = normalize_tool_path(&stripped_path, &project_root);

        let mut outline_warnings: Vec<String> = Vec::new();
        let mut items = match self
            .outline_items_for_normalized(&normalized, &ctx, &mut outline_warnings)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error reading outline: {e:#}"
                ))]));
            }
        };

        // Two-pass fallback: if alias-stripping changed the path and yielded no results,
        // try the original un-stripped path. Handles the case where the project alias
        // matches a package subdirectory name (e.g. project "my_pkg" with target
        // "my_pkg/config.py" → after strip becomes "config.py" which is wrong;
        // the correct relative path is "my_pkg/config.py").
        if items.is_empty() && stripped_path != request.path {
            let normalized_orig = normalize_tool_path(&request.path, &project_root);
            if normalized_orig != normalized {
                tracing::debug!(
                    "file_outline: primary '{}' empty, trying fallback '{}'",
                    normalized,
                    normalized_orig
                );
                items = match self
                    .outline_items_for_normalized(&normalized_orig, &ctx, &mut outline_warnings)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            "file_outline: fallback '{}' also failed: {:?}",
                            normalized_orig,
                            e
                        );
                        push_store_warning(
                            &mut outline_warnings,
                            &store_warning(
                                ctx.project_alias.as_deref().unwrap_or("unknown"),
                                "outline scan",
                                &format!("{e:#}"),
                            ),
                        );
                        Vec::new()
                    }
                };
            }
        }

        respond_with_items(&items, &outline_warnings, || {
            "No indexed chunks found for path. Verify the file is within the \
             project root and the index is up to date."
                .to_string()
        })
    }

    #[tool(
        description = "Retrieve the full content of a specific chunk by its ID, plus optional surrounding lines for context.\nUse this after search or explore to read the actual code without loading the whole file.\n\nUSE FOR: reading a specific function/class body after finding it via search.\nSet context_lines (default 0, max 20) to include lines before and after the chunk.\n\nIMPORTANT (multi-repo): chunk_ids are local to each repository and are NOT globally unique.\nWhen `project` is omitted in multi-repo mode, the tool scans all repositories for the chunk_id.\nIf found in exactly one repo, it is returned automatically. If found in multiple repos, an `ambiguous_chunk_id` error lists the candidates so you can retry with `project`."
    )]
    async fn get_chunk(
        &self,
        Parameters(request): Parameters<GetChunkRequest>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(
            "📥 get_chunk(chunk_id={}, project={:?})",
            request.chunk_id,
            request.project,
        );

        // Federation: a `chunk_ref` of the form "<peer>/<remote_alias>:<chunk_id>"
        // (returned by a federated search result) fetches the chunk from a remote
        // peer rather than the local index. The alias scopes the fetch to a single
        // remote project so the multi-repo peer can disambiguate the chunk_id.
        if let Some(chunk_ref) = request.chunk_ref.as_deref() {
            return self
                .federated_get_chunk(chunk_ref, request.context_lines)
                .await;
        }

        // In multi-repo serve mode, require explicit project or group scope.
        // Unscoped get_chunk would fan-out over all repos, opening all DBs unnecessarily.
        // Consistent with search/find/explore which also require scope.
        if request.project.is_none() && request.group.is_none() {
            if let Some(ref serve_state) = self.serve_state {
                let config = serve_state.config_snapshot();
                if config.repos.len() > 1 {
                    return Ok(CallToolResult::success(vec![Content::text(
                        self.format_scope_error(),
                    )]));
                }
            }
        }

        // Resolve project/group routing — allow unscoped only for single-repo mode
        let ctx = match self
            .resolve_routing(&request.project, &request.group, true, "get_chunk")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        let mut clamped = false;
        let mut context_lines = request.context_lines.unwrap_or(0);
        if context_lines > 20 {
            context_lines = 20;
            clamped = true;
        }

        // Stores that failed while looking up this chunk. get_chunk previously
        // collapsed every `Err` into "not found", so during the read-only
        // incident it would have reported every chunk in every vendor repo as
        // missing — a confident, wrong answer.
        let mut chunk_warnings: Vec<String> = Vec::new();

        // Look up chunk — multi-store: smart candidate detection for chunk_id collision.
        // chunk_ids are local per database, not globally unique. When no project is specified
        // and multiple stores are active, scan all stores to find which ones have this chunk_id.
        let chunk = if let Some(ref sv) = ctx.stores_vec {
            if sv.len() > 1 && request.project.is_none() {
                // Smart candidate detection: find which stores actually contain this chunk_id
                let mut candidates: Vec<(&Arc<SharedStores>, String)> = Vec::new();
                let aliases = ctx.aliases();
                for (i, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    match store.get_chunk(request.chunk_id) {
                        Ok(Some(_)) => {
                            // A store that HAS the chunk stays a candidate even if
                            // its alias is missing. `resolve_repo_stores_multi`
                            // keeps stores and aliases the same length, so this is
                            // unreachable today — but gating the push on
                            // `aliases.get(i)` meant a future break of that
                            // invariant would degrade to a silent auto-route rather
                            // than a loud one. The placeholder is per-index so two
                            // aliasless candidates stay distinguishable in
                            // `candidate_projects`.
                            let alias = aliases
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| format!("<unnamed store #{i}>"));
                            candidates.push((store_arc, alias));
                        }
                        Ok(None) => continue,
                        Err(ref e) => {
                            note_store_failure(&mut chunk_warnings, aliases, i, "chunk lookup", e);
                            continue;
                        }
                    }
                }
                match candidates.len() {
                    0 => {
                        return Ok(CallToolResult::success(vec![Content::text(
                            qualify_empty_result(
                                format!(
                                    "Chunk {} not found in any repository. Verify the \
                                     chunk_id and index state.",
                                    request.chunk_id
                                ),
                                &chunk_warnings,
                            ),
                        )]));
                    }
                    1 => {
                        // Exactly one store has this chunk_id — auto-route
                        let (store_arc, ref alias) = candidates[0];
                        // Record tool call for the specific repo that served this chunk
                        if let Some(ref serve_state) = self.serve_state {
                            serve_state.record_tool_call(alias, "get_chunk");
                            serve_state.touch_access(alias);
                        }
                        let store = store_arc.vector_store.read().await;
                        match store.get_chunk(request.chunk_id) {
                            Ok(c) => c,
                            Err(ref e) => {
                                push_store_warning(
                                    &mut chunk_warnings,
                                    &store_warning(alias, "chunk lookup", &format!("{e:#}")),
                                );
                                None
                            }
                        }
                    }
                    _ => {
                        // Multiple stores have this chunk_id — ambiguous.
                        //
                        // `candidate_projects` reads as the complete list, so a
                        // store that failed to answer has to be declared: the
                        // right repo may be the one missing from it.
                        let candidate_names: Vec<&str> =
                            candidates.iter().map(|(_, a)| a.as_str()).collect();
                        let payload = ambiguous_chunk_payload(
                            request.chunk_id,
                            &candidate_names,
                            &chunk_warnings,
                        );
                        return Ok(CallToolResult::success(vec![Content::text(
                            payload.to_string(),
                        )]));
                    }
                }
            } else {
                // Single store or project specified — direct lookup
                let aliases = ctx.aliases();
                let mut found = None;
                for (i, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    match store.get_chunk(request.chunk_id) {
                        Ok(Some(c)) => {
                            found = Some(c);
                            break;
                        }
                        Ok(None) => continue,
                        // Do NOT abandon the remaining stores: one broken store
                        // says nothing about the others, and the chunk may well
                        // live in a healthy one.
                        Err(ref e) => {
                            note_store_failure(&mut chunk_warnings, aliases, i, "chunk lookup", e);
                            continue;
                        }
                    }
                }
                found
            }
        } else {
            match self
                .with_vector_store_read_for(
                    |store| store.get_chunk(request.chunk_id),
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    push_store_warning(
                        &mut chunk_warnings,
                        &store_warning(
                            ctx.project_alias.as_deref().unwrap_or("unknown"),
                            "chunk lookup",
                            &format!("{e:#}"),
                        ),
                    );
                    None
                }
            }
        };

        let mut chunk = match chunk {
            Some(c) => c,
            None => {
                return Ok(CallToolResult::success(vec![Content::text(
                    qualify_empty_result(
                        format!(
                            "Chunk {} not found. Verify the chunk_id and index state.",
                            request.chunk_id
                        ),
                        &chunk_warnings,
                    ),
                )]));
            }
        };

        // Prefix path with alias for multi-repo identification
        chunk.path = ctx.prefix_result_path(&chunk.path);

        let mut context_before = None;
        let mut context_after = None;
        let mut note = None;

        if context_lines > 0 {
            // Resolve relative chunk paths against project root (not process CWD).
            let source_path = if Path::new(&chunk.path).is_absolute() {
                PathBuf::from(&chunk.path)
            } else {
                self.project_path.join(&chunk.path)
            };
            match tokio::fs::read_to_string(&source_path).await {
                Ok(src) => {
                    let lines: Vec<&str> = src.lines().collect();
                    if !lines.is_empty() {
                        let before_start = chunk.start_line.saturating_sub(context_lines);
                        let before_end = chunk.start_line.min(lines.len());
                        if before_start < before_end {
                            context_before = Some(lines[before_start..before_end].join("\n"));
                        }

                        let after_start = chunk.end_line.min(lines.len());
                        let after_end = (chunk.end_line + context_lines).min(lines.len());
                        if after_start < after_end {
                            context_after = Some(lines[after_start..after_end].join("\n"));
                        }
                    }
                }
                Err(_) => {
                    note = Some(
                        "source file not readable, returning indexed content only".to_string(),
                    );
                }
            }
        }

        let response = GetChunkResponse {
            chunk_id: request.chunk_id,
            path: chunk.path,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            kind: chunk.kind,
            signature: chunk.signature,
            content: chunk.content,
            context_before,
            context_after,
            context_lines_clamped: if clamped { Some(true) } else { None },
            note,
        };

        // The success path is the one that used to drop this, and it is the
        // dangerous one: a confidently-returned chunk from a group where a store
        // failed to answer looks exactly like a chunk from a healthy group. Same
        // false negative as an empty result, harder to notice.
        respond_with_object(&response, &chunk_warnings)
    }

    /// Symbol impact analysis — returns transitive call-sites of a symbol with file/line precision.
    ///
    /// The recommended tool for "who calls X?" / "what breaks if I rename X?". Uses
    /// language-specific semantic analysis (SCIP) to find all references, enabling agents
    /// to plan refactors with IDE-class accuracy instead of text-matching grep heuristics.
    /// Precision backends ship per language: C# (bundled `scip-csharp` helper,
    /// `-with-csharp` releases) and TypeScript (`scip-typescript`, resolved via `npx`
    /// or `CODESEARCH_SCIP_TYPESCRIPT`). If no backend is installed for the target
    /// language, the response reports it — fall back to `find` with `kind="usages"`
    /// (lexical) only then.
    #[tool(
        description = "Symbol impact analysis — find all references to a symbol with IDE-class precision (SCIP).\n\nThe right tool for \"who calls X?\" / \"what breaks if I rename X?\". Returns transitive call-sites with file/line precision, enabling agents to plan refactors without missing a caller. More accurate than text-based `find kind=\"usages\"` because it understands language semantics.\n\nInput variants:\n- By name: `{ \"symbol_name\": \"FieldDefinition.Validate\", \"project\": \"myrepo\" }`\n- By position: `{ \"file\": \"src/Validation/FieldDefinition.cs\", \"line\": 42, \"project\": \"myrepo\" }`\n\nPrecision backends (SCIP) ship per language; C# (bundled `scip-csharp` helper, `-with-csharp` releases) and TypeScript (via `npx` or `CODESEARCH_SCIP_TYPESCRIPT`) are available today. For Rust/Python/Go/etc., use `find` with `kind=\"usages\"` as a text-based fallback until SCIP backends for those languages ship.\n\nOn a busy answer (`\"busy\": true`): sleep `retry_after_seconds` and retry the SAME call. Busy is progress, not failure — never fall back to text search on busy.\n\nIMPORTANT (multi-repo): always specify `project` (single repo). Omitting `project` in multi-repo mode returns a `scope_required` error."
    )]
    async fn find_impact(
        &self,
        Parameters(request): Parameters<FindImpactRequest>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(
            "📥 find_impact(symbol_name={:?}, file={:?}, line={:?}, language={:?}, project={:?})",
            request.symbol_name,
            request.file,
            request.line,
            request.language,
            request.project,
        );

        // Validate input: must provide either symbol_name or file+line
        let has_name = request
            .symbol_name
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty());
        let has_position = request.file.is_some() && request.line.is_some();
        if !has_name && !has_position {
            return Ok(CallToolResult::success(vec![Content::text(
                "Must provide either `symbol_name` or both `file` and `line` for position-based lookup.".to_string(),
            )]));
        }

        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "find_impact")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        // Determine project root and db_path for the symbol index
        let (project_root, db_path) = if let Some(ref alias) = ctx.project_alias {
            let root = ctx
                .alias_roots
                .get(alias)
                .map(PathBuf::from)
                .unwrap_or_else(|| self.project_path.clone());
            // The symbol index DB lives alongside the vector DB
            let db = root.join(crate::constants::DB_DIR_NAME);
            (root, db)
        } else {
            // Single-repo / stdio mode: use the service's own paths
            (self.project_path.clone(), self.db_path.clone())
        };

        // Use the shared symbol indexer registry
        let registry = &self.symbol_registry;

        // Determine which language to use
        let language = request.language.clone().or_else(|| {
            // Auto-detect from file extension
            request.file.as_ref().and_then(|f| {
                let ext = Path::new(f).extension()?.to_str()?.to_lowercase();
                match ext.as_str() {
                    "cs" => Some(crate::constants::LANG_CSHARP.to_string()),
                    "ts" | "tsx" | "mts" | "cts" => {
                        Some(crate::constants::LANG_TYPESCRIPT.to_string())
                    }
                    _ => None,
                }
            })
        });

        let indexer: &dyn crate::symbols::SymbolIndexer = match language {
            Some(ref lang) => match registry.get(lang) {
                Some(i) => i,
                None => {
                    let available = registry.available_languages();
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "No symbol indexer for language '{}'. Available languages: {:?}",
                        lang, available
                    ))]));
                }
            },
            None => {
                // No language specified and couldn't auto-detect — try all installed
                let installed = registry.installed_languages();
                if installed.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No symbol indexers installed. Install the `scip-csharp` helper for C# support, or `scip-typescript` (via npx) for TypeScript support.".to_string(),
                    )]));
                }
                // Use the first installed language (MVP: C# or TypeScript)
                match registry.get(&installed[0]) {
                    Some(i) => i,
                    None => {
                        unreachable!("installed_languages() returned a language with no indexer")
                    }
                }
            }
        };

        // Check if the helper is available
        if !indexer.is_available() {
            let error = crate::symbols::SymbolIndexError {
                error: format!(
                    "Symbol indexer for '{}' is not available. The helper binary is not installed.",
                    indexer.language()
                ),
                available_languages: registry.available_languages(),
                hint_for_agent: format!(
                    "Install the `-with-csharp` release variant, or set {} to the helper path.",
                    crate::constants::SCIP_CSHARP_HELPER_ENV
                ),
            };
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string(&error).unwrap_or_else(|_| error.error.clone()),
            )]));
        }

        // Perform the lookup under an internal wall-clock budget.
        //
        // `find_references` may invoke `scip-csharp find-refs` on a cache miss
        // (lazy Opt-2 reference resolution). That subprocess can take several minutes
        // on a large solution. The call therefore runs on `spawn_blocking` (it never
        // blocks an async worker thread) and is raced against
        // CODESEARCH_FIND_IMPACT_BUDGET_SECS: on overrun the caller gets a structured
        // busy answer instead of the MCP client winning the timeout race. The
        // blocking task is abandoned, not cancelled — its cache writes still land
        // in LMDB, so the hinted retry is served warm; the lookup tracker
        // (find_impact_tracker) makes that retry observe progress or the warm
        // result explicitly instead of re-running the helper.
        let language_for_lookup = indexer.language().to_string();
        let symbol_name_for_lookup = request.symbol_name.clone();
        let line_for_lookup = request.line;
        let file_for_pos = if !has_name {
            Some(self.normalize_symbol_query_path(
                &project_root,
                Path::new(request.file.as_ref().unwrap()),
            ))
        } else {
            None
        };
        let what = if has_name {
            format!("'{}'", symbol_name_for_lookup.as_deref().unwrap_or("?"))
        } else {
            format!(
                "{}:{}",
                request.file.as_deref().unwrap_or("?"),
                line_for_lookup.unwrap_or(0)
            )
        };
        let busy_state = format!(
            "resolving {} via the {} SCIP helper (cold reference cache)",
            what, language_for_lookup
        );
        let budget_secs = resolve_find_impact_budget_secs();

        // Index fingerprint: the repository HEAD at response time. The
        // non-fatal git read is offloaded like every blocking call; a
        // failed read simply omits the field. Drift against
        // `index_head_sha` is surfaced, never auto-reindexed (deliberate:
        // reindexing a large solution on every branch switch would thrash).
        let head_root = project_root.clone();
        let current_head_sha =
            tokio::task::spawn_blocking(move || crate::symbols::current_git_head(&head_root))
                .await
                .unwrap_or(None);

        // Shared result construction: the warm-retry path (below) must be
        // byte-identical to a budget-fast completion, so both build the
        // response through this one closure.
        let build_impact =
            |references: Vec<crate::symbols::SymbolReference>| crate::symbols::FindImpactResult {
                symbol: request.symbol_name.clone().unwrap_or_else(|| {
                    format!(
                        "{}:{}",
                        request.file.as_deref().unwrap_or("?"),
                        request.line.unwrap_or(0)
                    )
                }),
                references: dedupe_references(references),
                index_age_seconds: indexer.index_age(&db_path),
                language: indexer.language().to_string(),
                scope: ctx
                    .project_alias
                    .map(|a| format!("project:{}", a))
                    .unwrap_or_else(|| "local".to_string()),
                index_head_sha: indexer.index_head_sha(&db_path),
                current_head_sha: current_head_sha.clone(),
            };

        // Background continuation: consult the tracker before starting a
        // (potentially cold, minutes-long) lookup. A retry of an overran
        // lookup observes progress or the warm result instead of racing a
        // second identical helper subprocess against the cold cache.
        let tracker_key: find_impact_tracker::LookupKey = (db_path.clone(), what.clone());
        match find_impact_tracker::IMPACT_LOOKUP_TRACKER.check(&tracker_key) {
            Some(find_impact_tracker::TrackedStatus::Running { elapsed_ms }) => {
                tracing::info!(
                    "find_impact retry: lookup still running ({}ms elapsed): {}",
                    elapsed_ms,
                    busy_state
                );
                let busy = crate::symbols::SymbolLookupBusy {
                    busy: true,
                    state: busy_state.clone(),
                    waited_ms: elapsed_ms,
                    advice: format!(
                        "still running ({}s elapsed); retry the same call in ~{}s",
                        elapsed_ms / 1000,
                        budget_secs.max(1)
                    ),
                    retry_after_seconds: budget_secs.max(1),
                };
                let json =
                    serde_json::to_string(&busy).unwrap_or_else(|_| "{\"busy\":true}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
            Some(find_impact_tracker::TrackedStatus::Done(Ok(references))) => {
                tracing::info!(
                    "find_impact retry: serving warm result ({} references) from the finished background lookup: {}",
                    references.len(),
                    busy_state
                );
                let impact = build_impact(references);
                let json = serde_json::to_string(&impact).unwrap_or_else(|_| "{}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
            Some(find_impact_tracker::TrackedStatus::Done(Err(chain))) => {
                // Same classification as a fresh failure: the tracked chain
                // is already `{:#}`-rendered, the index age decides the class.
                let failure = crate::symbols::SymbolLookupFailure::classify(
                    chain,
                    indexer.index_age(&db_path),
                );
                let json =
                    serde_json::to_string(&failure).unwrap_or_else(|_| failure.error.clone());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
            None => {}
        }

        let registry_for_lookup = self.symbol_registry.clone();
        let db_path_for_lookup = db_path.clone();
        let file_for_lookup = file_for_pos;
        let lookup_entry = find_impact_tracker::IMPACT_LOOKUP_TRACKER.register(tracker_key.clone());
        let lookup_entry_in_task = lookup_entry;
        let lookup = async move {
            tokio::task::spawn_blocking(move || {
                let indexer = registry_for_lookup
                    .get(&language_for_lookup)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "symbol indexer for '{}' disappeared mid-request",
                            language_for_lookup
                        )
                    })?;
                let result = if has_name {
                    indexer.find_references(
                        &db_path_for_lookup,
                        symbol_name_for_lookup.as_deref().unwrap_or(""),
                    )
                } else {
                    indexer.find_references_by_position(
                        &db_path_for_lookup,
                        &file_for_lookup.unwrap_or_default(),
                        line_for_lookup.unwrap_or(0),
                    )
                };
                // Record INSIDE the blocking task: the handler's awaiting
                // future is dropped at budget overrun, but this detached
                // task survives and the recorded outcome is what the hinted
                // retry observes. (`anyhow::Error` is not `Clone`, so the
                // failure side is recorded as its rendered `{:#}` chain.)
                let recorded = result.as_ref().map_err(|e| format!("{e:#}")).cloned();
                lookup_entry_in_task.finish(recorded);
                result
            })
            .await
            .map_err(|e| anyhow::anyhow!("symbol lookup task failed: {e:#}"))?
        };

        match find_impact_with_budget(budget_secs, busy_state, lookup).await {
            ImpactLookupOutcome::Done(Ok(references)) => {
                // Completed within the budget: nothing is in flight, so a
                // later lookup must consult the real cache, not the tracker.
                find_impact_tracker::IMPACT_LOOKUP_TRACKER.remove(&tracker_key);
                let impact = build_impact(references);
                let json = serde_json::to_string(&impact).unwrap_or_else(|_| "{}".to_string());
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            ImpactLookupOutcome::Done(Err(e)) => {
                find_impact_tracker::IMPACT_LOOKUP_TRACKER.remove(&tracker_key);
                // Typed failure envelope (busy/stale/failed must stay
                // machine-branchable; the index age decides stale vs failed).
                let failure = crate::symbols::SymbolLookupFailure::classify(
                    format!("{e:#}"),
                    indexer.index_age(&db_path),
                );
                let json =
                    serde_json::to_string(&failure).unwrap_or_else(|_| failure.error.clone());
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            ImpactLookupOutcome::Busy { state, waited_ms } => {
                tracing::warn!(
                    "find_impact budget overrun after {}ms (budget {}s): {} — answering busy, lookup continues in background",
                    waited_ms,
                    budget_secs,
                    state
                );
                let busy = crate::symbols::SymbolLookupBusy {
                    busy: true,
                    state,
                    waited_ms,
                    advice: format!(
                        "retry the same call in ~{}s; the lookup keeps running in the background and the retry is served from cache once it completes",
                        budget_secs.max(1)
                    ),
                    retry_after_seconds: budget_secs.max(1),
                };
                let json =
                    serde_json::to_string(&busy).unwrap_or_else(|_| "{\"busy\":true}".to_string());
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
        }
    }

    fn normalize_symbol_query_path(&self, project_root: &Path, file: &Path) -> PathBuf {
        if file.is_absolute() {
            if let Ok(relative) = file.strip_prefix(project_root) {
                return PathBuf::from(relative.to_string_lossy().replace('\\', "/"));
            }
        }

        PathBuf::from(file.to_string_lossy().replace('\\', "/"))
    }

    async fn find_imports(
        &self,
        Parameters(request): Parameters<FindImportsRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "find")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // In serve mode, use the resolved project root from alias_roots
        let project_root = if let Some(ref alias) = ctx.project_alias {
            ctx.alias_roots
                .get(alias)
                .map(PathBuf::from)
                .unwrap_or_else(|| self.project_path.clone())
        } else {
            self.project_path.clone()
        };
        // Strip project-alias prefix from target path if present.
        let stripped_path = strip_alias_prefix(&request.path, ctx.project_alias.as_ref());
        let normalized = normalize_tool_path(&stripped_path, &project_root);

        // Stores that failed during this lookup, so "no imports found" is never
        // reported as fact when a store never answered.
        let mut import_warnings: Vec<String> = Vec::new();

        let mut items = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store group fan-out: collect import items from all stores
            let import_aliases = ctx.aliases();
            let mut all_items: Vec<ImportItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for (store_idx, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.chunks_for_file(&normalized) {
                    Ok(metas) => {
                        for meta in metas {
                            if !is_import_kind(&meta.kind) {
                                continue;
                            }
                            if seen_ids.insert(meta.id) {
                                match store.get_chunk(meta.id) {
                                    Ok(Some(chunk)) => all_items.extend(parse_import_lines(
                                        &chunk.content,
                                        chunk.start_line,
                                    )),
                                    Ok(None) => {}
                                    Err(ref e) => note_store_failure(
                                        &mut import_warnings,
                                        import_aliases,
                                        store_idx,
                                        "chunk lookup",
                                        e,
                                    ),
                                }
                            }
                        }
                    }
                    Err(ref e) => {
                        note_store_failure(
                            &mut import_warnings,
                            import_aliases,
                            store_idx,
                            "imports scan",
                            e,
                        );
                    }
                }
            }
            all_items
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        let mut out = Vec::new();
                        for meta in store.chunks_for_file(&normalized)? {
                            if !is_import_kind(&meta.kind) {
                                continue;
                            }
                            if let Some(chunk) = store.get_chunk(meta.id)? {
                                out.extend(parse_import_lines(&chunk.content, chunk.start_line));
                            }
                        }
                        Ok(out)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error reading imports: {e:#}"
                    ))]));
                }
            }
        };

        if items.is_empty() {
            // Fallback: no import-kind chunks found for this file. Broaden the
            // search to common import keywords and filter to the target path.
            // Limitation: this only finds chunks containing these literal words;
            // language-specific import forms that lack these keywords will be missed.
            let fallback_limit = 40usize;
            let mut all_hits: Vec<(u32, f32)> = Vec::new();
            let mut seen_fts_ids: HashSet<u32> = HashSet::new();

            if let Some(ref sv) = ctx.stores_vec {
                let import_aliases = ctx.aliases();
                // Multi-store FTS fallback
                for keyword in IMPORT_FTS_KEYWORDS {
                    let hits = self
                        .with_fts_store_read_multi(
                            |fts_store| fts_store.search_exact(keyword, fallback_limit, None),
                            sv.clone(),
                            ctx.store_aliases.as_ref().unwrap(),
                        )
                        .await
                        .unwrap_or_default()
                        .into_results(&mut import_warnings, "imports search");
                    for h in hits {
                        if seen_fts_ids.insert(h.chunk_id) {
                            all_hits.push((h.chunk_id, h.score));
                        }
                    }
                }

                // Resolve FTS hits via vector stores
                let mut resolved: Vec<ImportItem> = Vec::new();
                for (chunk_id, _) in &all_hits {
                    for (store_idx, store_arc) in sv.iter().enumerate() {
                        let store = store_arc.vector_store.read().await;
                        match store.get_chunk(*chunk_id) {
                            Ok(Some(chunk)) => {
                                if crate::cache::normalize_path_str(&chunk.path) == normalized {
                                    resolved.extend(parse_import_lines(
                                        &chunk.content,
                                        chunk.start_line,
                                    ));
                                }
                                break;
                            }
                            Ok(None) => continue,
                            Err(ref e) => {
                                note_store_failure(
                                    &mut import_warnings,
                                    import_aliases,
                                    store_idx,
                                    "chunk lookup",
                                    e,
                                );
                                continue;
                            }
                        }
                    }
                }
                items = resolved;
            } else {
                // Single-store FTS fallback
                for keyword in IMPORT_FTS_KEYWORDS {
                    let hits = match self
                        .with_fts_store_read_for(
                            |fts_store| fts_store.search_exact(keyword, fallback_limit, None),
                            ctx.stores.clone(),
                        )
                        .await
                    {
                        Ok(h) => h,
                        Err(e) => {
                            push_store_warning(
                                &mut import_warnings,
                                &store_warning(
                                    ctx.project_alias.as_deref().unwrap_or("unknown"),
                                    "imports search",
                                    &format!("{e:#}"),
                                ),
                            );
                            Vec::new()
                        }
                    };
                    for h in hits {
                        if seen_fts_ids.insert(h.chunk_id) {
                            all_hits.push((h.chunk_id, h.score));
                        }
                    }
                }

                items = self
                    .with_vector_store_read_for(
                        |store| {
                            let mut out = Vec::new();
                            for (chunk_id, _) in &all_hits {
                                if let Some(chunk) = store.get_chunk(*chunk_id)? {
                                    if crate::cache::normalize_path_str(&chunk.path) == normalized {
                                        out.extend(parse_import_lines(
                                            &chunk.content,
                                            chunk.start_line,
                                        ));
                                    }
                                }
                            }
                            Ok(out)
                        },
                        ctx.stores.clone(),
                    )
                    .await
                    .unwrap_or_else(|e| {
                        push_store_warning(
                            &mut import_warnings,
                            &store_warning(
                                ctx.project_alias.as_deref().unwrap_or("unknown"),
                                "chunk lookup",
                                &format!("{e:#}"),
                            ),
                        );
                        Vec::new()
                    });
            }
        }

        items.sort_by_key(|i| i.line);
        respond_with_items(&items, &import_warnings, || {
            "No import chunks found. The index may not include import statements \
             for this language, or the file has no imports."
                .to_string()
        })
    }

    async fn find_dependents(
        &self,
        Parameters(request): Parameters<FindDependentsRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "find")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        let limit = request.limit.unwrap_or(20).min(200);
        let high_limit = (limit * 10).max(200); // generous budget for filtering

        // Stores that failed during this lookup, so "no dependents" is never
        // reported as fact when a store never answered.
        let mut dep_warnings: Vec<String> = Vec::new();

        // Extract a meaningful search term from path-like inputs.
        // Import chunks contain module references like `use crate::constants::X`
        // but the tool receives file paths like `src/constants.rs`.
        // We extract the file stem to match against module names in imports.
        let search_term = if request.symbol_or_path.contains('/')
            || request.symbol_or_path.contains('\\')
            || request.symbol_or_path.contains('.')
        {
            std::path::Path::new(&request.symbol_or_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&request.symbol_or_path)
                .to_string()
        } else {
            request.symbol_or_path.clone()
        };

        let import_kind = Some(crate::chunker::ChunkKind::Imports);

        // Two-phase search strategy:
        // 1. `search_exact` — precise term match on signature+content with
        //    MUST filter for Import kind. Strictly limits results to import chunks.
        // 2. If that yields no import-kind results, fall back to `search`
        //    (QueryParser, broader tokenization) with kind boost for imports.
        //
        // Limitation: the chunker does not emit per-statement AST import chunks;
        // imports are gap-classified as `Imports` kind. Chunks whose kind doesn't
        // match `is_import_kind()` will be missed regardless of search method.
        let fts_results = if let Some(ref sv) = ctx.stores_vec {
            let sa = ctx.store_aliases.as_ref().unwrap();
            // Multi-store FTS search
            let exact_hits = self
                .with_fts_store_read_multi(
                    |fts_store| fts_store.search_exact(&search_term, high_limit, import_kind),
                    sv.clone(),
                    sa,
                )
                .await
                .unwrap_or_default()
                .into_results(&mut dep_warnings, "dependents search");

            if exact_hits.is_empty() {
                self.with_fts_store_read_multi(
                    |fts_store| fts_store.search(&search_term, high_limit, import_kind),
                    sv.clone(),
                    sa,
                )
                .await
                .unwrap_or_default()
                .into_results(&mut dep_warnings, "dependents search")
            } else {
                exact_hits
            }
        } else {
            // Single-store FTS search
            let alias = ctx.project_alias.as_deref().unwrap_or("unknown");
            let mut run = |r: anyhow::Result<Vec<crate::fts::FtsResult>>| match r {
                Ok(hits) => hits,
                Err(e) => {
                    push_store_warning(
                        &mut dep_warnings,
                        &store_warning(alias, "dependents search", &format!("{e:#}")),
                    );
                    Vec::new()
                }
            };
            let exact_hits = run(self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search_exact(&search_term, high_limit, import_kind),
                    ctx.stores.clone(),
                )
                .await);

            if exact_hits.is_empty() {
                run(self
                    .with_fts_store_read_for(
                        |fts_store| fts_store.search(&search_term, high_limit, import_kind),
                        ctx.stores.clone(),
                    )
                    .await)
            } else {
                exact_hits
            }
        };

        let mut items = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: resolve chunks across all stores
            let dep_aliases = ctx.aliases();
            let mut seen_paths = HashSet::new();
            let mut out = Vec::new();
            for f in &fts_results {
                for (store_idx, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    match store.get_chunk(f.chunk_id) {
                        Ok(Some(chunk)) => {
                            if !is_import_kind(&chunk.kind) {
                                break; // try next FTS result
                            }

                            let norm = crate::cache::normalize_path_str(&chunk.path);
                            if !seen_paths.insert(norm) {
                                break;
                            }

                            let term_lower = search_term.to_lowercase();
                            let import_statement =
                                if chunk.content.to_lowercase().contains(&term_lower) {
                                    chunk
                                        .content
                                        .lines()
                                        .find(|l| l.to_lowercase().contains(&term_lower))
                                        .unwrap_or("")
                                        .to_string()
                                } else {
                                    chunk.signature.filter(|s| !s.is_empty()).unwrap_or(
                                        chunk.content.lines().next().unwrap_or("").to_string(),
                                    )
                                };

                            out.push(DependentItem {
                                path: chunk.path,
                                line: chunk.start_line,
                                import_statement,
                            });

                            break; // found in this store, move to next FTS result
                        }
                        Ok(None) => {} // try next store
                        // One broken store says nothing about the others; a
                        // `break` here silently drops a chunk that lives in a
                        // healthy store later in the list.
                        Err(ref e) => {
                            note_store_failure(
                                &mut dep_warnings,
                                dep_aliases,
                                store_idx,
                                "chunk lookup",
                                e,
                            );
                            continue;
                        }
                    }
                }
                if out.len() >= limit {
                    break;
                }
            }
            out
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        let mut seen_paths = HashSet::new();
                        let mut out = Vec::new();
                        let term_lower = search_term.to_lowercase();
                        for f in &fts_results {
                            if let Some(chunk) = store.get_chunk(f.chunk_id)? {
                                if !is_import_kind(&chunk.kind) {
                                    continue;
                                }

                                let norm = crate::cache::normalize_path_str(&chunk.path);
                                if !seen_paths.insert(norm) {
                                    continue;
                                }

                                // Extract the specific import line(s) that mention the
                                // module name, rather than returning the entire chunk content.
                                let import_statement =
                                    if chunk.content.to_lowercase().contains(&term_lower) {
                                        chunk
                                            .content
                                            .lines()
                                            .find(|l| l.to_lowercase().contains(&term_lower))
                                            .unwrap_or("")
                                            .to_string()
                                    } else {
                                        chunk.signature.filter(|s| !s.is_empty()).unwrap_or(
                                            chunk.content.lines().next().unwrap_or("").to_string(),
                                        )
                                    };

                                out.push(DependentItem {
                                    path: chunk.path,
                                    line: chunk.start_line,
                                    import_statement,
                                });

                                if out.len() >= limit {
                                    break;
                                }
                            }
                        }
                        Ok(out)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error resolving dependents: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        items.sort_by(|a, b| a.path.cmp(&b.path));
        respond_with_items(&items, &dep_warnings, || {
            format!("No dependent files found for '{}'.", request.symbol_or_path)
        })
    }

    /// Internal: find similar chunks, used by `explore(kind="similar")`.
    async fn similar_chunks(
        &self,
        Parameters(request): Parameters<SimilarChunksRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "explore")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        let limit = request.limit.unwrap_or(5).min(20);

        // Stores that failed while resolving the source embedding. `if let
        // Ok(Some(..))` used to discard the error, so a dead store produced
        // "embedding not found" — a wrong diagnosis, not a missing chunk.
        let mut similar_warnings: Vec<String> = Vec::new();

        let mut results = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: find the embedding in whichever store has it,
            // then search across all stores for similar chunks.
            let aliases = ctx.aliases();
            let mut embedding: Option<Vec<f32>> = None;
            for (i, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.get_embedding(request.chunk_id) {
                    Ok(Some(emb)) => {
                        embedding = Some(emb);
                        break;
                    }
                    Ok(None) => continue,
                    Err(ref e) => {
                        note_store_failure(
                            &mut similar_warnings,
                            aliases,
                            i,
                            "embedding lookup",
                            e,
                        );
                        continue;
                    }
                }
            }

            let embedding = match embedding {
                Some(e) => e,
                None => {
                    return Ok(CallToolResult::success(vec![Content::text(
                        qualify_empty_result(
                            format!(
                                "Embedding not found for chunk_id {} in any store.",
                                request.chunk_id
                            ),
                            &similar_warnings,
                        ),
                    )]));
                }
            };

            // Search across all stores with the found embedding
            let mut all_results: Vec<SearchResultItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for (store_idx, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.search(&embedding, limit + 1) {
                    Ok(mut neighbors) => {
                        neighbors.retain(|r| r.id != request.chunk_id);
                        for r in neighbors {
                            if seen_ids.insert(r.id) {
                                all_results.push(SearchResultItem {
                                    chunk_id: Some(r.id),
                                    path: r.path,
                                    start_line: r.start_line,
                                    end_line: r.end_line,
                                    kind: r.kind,
                                    score: r.score,
                                    signature: r.signature,
                                    content: None,
                                    context_prev: None,
                                    context_next: None,
                                    source: None,
                                    chunk_ref: None,
                                });
                            }
                        }
                    }
                    Err(ref e) => {
                        // The embedding was found, so the handler returns results
                        // either way; without this, a group query silently omits
                        // every neighbour from the broken repo.
                        note_store_failure(
                            &mut similar_warnings,
                            aliases,
                            store_idx,
                            "similarity search",
                            e,
                        );
                    }
                }
            }

            all_results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            all_results.truncate(limit);
            all_results
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        let embedding =
                            store.get_embedding(request.chunk_id)?.ok_or_else(|| {
                                anyhow::anyhow!(
                                    "embedding not found for chunk_id {}",
                                    request.chunk_id
                                )
                            })?;

                        let mut neighbors = store.search(&embedding, limit + 1)?;
                        neighbors.retain(|r| r.id != request.chunk_id);
                        neighbors.truncate(limit);

                        let items = neighbors
                            .into_iter()
                            .map(|r| SearchResultItem {
                                chunk_id: Some(r.id),
                                path: r.path,
                                start_line: r.start_line,
                                end_line: r.end_line,
                                kind: r.kind,
                                score: r.score,
                                signature: r.signature,
                                content: None,
                                context_prev: None,
                                context_next: None,
                                source: None,
                                chunk_ref: None,
                            })
                            .collect::<Vec<_>>();
                        Ok(items)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error finding similar chunks: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut results {
            item.path = ctx.prefix_result_path(&item.path);
        }

        // Every exit carries the channel: the earlier read sat in an
        // early-return arm, so once an embedding was found, every failure
        // recorded afterwards (the whole neighbour fan-out) was discarded.
        respond_with_items(&results, &similar_warnings, || {
            format!("No similar chunks found for chunk_id {}.", request.chunk_id)
        })
    }

    async fn literal_search(
        &self,
        Parameters(request): Parameters<LiteralSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "search")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        let limit = request.limit.unwrap_or(20);
        let output_format = request.format.as_deref().unwrap_or("json");

        // Repos that failed during this search. Reported to the caller: an
        // agent that never sees the server log cannot otherwise distinguish a
        // broken store from a repo that holds no match.
        let mut literal_warnings: Vec<String> = Vec::new();

        // Auto-regex promotion: detect code patterns that BM25 would destroy
        let user_set_regex = request.regex.unwrap_or(false);
        let user_set_phrase = request.phrase.unwrap_or(false);
        let auto_promoted =
            !user_set_regex && !user_set_phrase && looks_like_code_pattern(&request.query);

        let (effective_query, effective_regex) = if auto_promoted {
            let escaped = regex::escape(&request.query);
            // Relax whitespace to \s+ so "foo = null" → "foo\s+=\s+null"
            // regex::escape does not escape spaces, so replace literal spaces.
            let relaxed = escaped.replace(' ', r"\s+");
            (relaxed, true)
        } else {
            (request.query.clone(), user_set_regex)
        };

        tracing::debug!(
            "MCP literal_search: query='{}', regex={:?}, phrase={:?}, limit={}, file_glob={:?}, language={:?}, format={}, multi={}",
            request.query, request.regex, request.phrase, limit,
            request.file_glob, request.language, output_format, ctx.is_multi
        );

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // Pre-compute normalized project root for stripping absolute paths in glob matching
        let lang_filter = request.language.clone();
        let glob_filter = request.file_glob.clone();
        let regex_enabled = effective_regex;
        let snippet_regex = if regex_enabled {
            Regex::new(&effective_query).ok()
        } else {
            None
        };
        let project_root_normalized = {
            let root = crate::cache::normalize_path_str(self.project_path.to_str().unwrap_or(""));
            root.trim_end_matches('/').to_string()
        };

        // Decide: BM25 path (for anchorable queries) or scan path (for tokenless regex
        // or disjunctive OR patterns like TODO|FIXME|HACK that BM25 treats as AND).
        let tokenless_regex = regex_enabled
            && snippet_regex.is_some()
            && (!regex_has_anchorable_token(&effective_query)
                || regex_has_disjunctive_or(&effective_query));

        let mut items: Vec<LiteralSearchResultItem> = if tokenless_regex {
            // ── Scan path ──────────────────────────────────────────────
            // Tokenless regex (e.g. \bfn\s+\w+) — BM25 cannot produce useful
            // candidates. Scan all chunks sequentially, apply regex post-filter.
            // Score is 0.0 for all results (no BM25 ranking applies).
            tracing::debug!("literal_search: tokenless regex detected, using scan path");
            if let Some(ref sv) = ctx.stores_vec {
                // Multi-store scan
                let mut items: Vec<LiteralSearchResultItem> = Vec::new();
                for store_arc in sv {
                    let store = store_arc.vector_store.read().await;
                    let all_chunks = match store.iter_all_chunks() {
                        Ok(chunks) => chunks,
                        Err(_) => continue,
                    };
                    for (_, chunk) in all_chunks {
                        if let Some(ref lang) = lang_filter {
                            let file_lang = Language::from_path(std::path::Path::new(&chunk.path));
                            if file_lang.name() != lang {
                                continue;
                            }
                        }
                        if let Some(ref glob) = glob_filter {
                            let relative_path = chunk
                                .path
                                .strip_prefix(&project_root_normalized)
                                .unwrap_or(&chunk.path)
                                .trim_start_matches('/');
                            if !simple_glob_match(glob, relative_path) {
                                continue;
                            }
                        }
                        if let Some((match_offset, snippet)) = match_line_for_literal(
                            &chunk.content,
                            &effective_query,
                            snippet_regex.as_ref(),
                        ) {
                            let match_line = chunk.start_line + match_offset;
                            items.push(LiteralSearchResultItem {
                                path: chunk.path,
                                start_line: match_line,
                                end_line: match_line,
                                snippet,
                                score: 0.0, // No BM25 score — scan-path results are unranked
                                kind: if chunk.kind.is_empty() {
                                    None
                                } else {
                                    Some(chunk.kind)
                                },
                                signature: chunk.signature.filter(|s| !s.is_empty()),
                            });
                            if items.len() >= limit {
                                break;
                            }
                        }
                    }
                    if items.len() >= limit {
                        break;
                    }
                }
                items
            } else {
                // Single-store scan
                match self
                    .with_vector_store_read_for(
                        |store| {
                            let all_chunks = store.iter_all_chunks()?;
                            let mut items: Vec<LiteralSearchResultItem> = Vec::new();
                            for (_, chunk) in all_chunks {
                                if let Some(ref lang) = lang_filter {
                                    let file_lang =
                                        Language::from_path(std::path::Path::new(&chunk.path));
                                    if file_lang.name() != lang {
                                        continue;
                                    }
                                }
                                if let Some(ref glob) = glob_filter {
                                    let relative_path = chunk
                                        .path
                                        .strip_prefix(&project_root_normalized)
                                        .unwrap_or(&chunk.path)
                                        .trim_start_matches('/');
                                    if !simple_glob_match(glob, relative_path) {
                                        continue;
                                    }
                                }
                                if let Some((match_offset, snippet)) = match_line_for_literal(
                                    &chunk.content,
                                    &effective_query,
                                    snippet_regex.as_ref(),
                                ) {
                                    let match_line = chunk.start_line + match_offset;
                                    items.push(LiteralSearchResultItem {
                                        path: chunk.path,
                                        start_line: match_line,
                                        end_line: match_line,
                                        snippet,
                                        score: 0.0, // No BM25 score — scan-path results are unranked
                                        kind: if chunk.kind.is_empty() {
                                            None
                                        } else {
                                            Some(chunk.kind)
                                        },
                                        signature: chunk.signature.filter(|s| !s.is_empty()),
                                    });
                                    if items.len() >= limit {
                                        break;
                                    }
                                }
                            }
                            Ok(items)
                        },
                        ctx.stores.clone(),
                    )
                    .await
                {
                    Ok(items) => items,
                    Err(e) => {
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "Error scanning chunks: {e:#}"
                        ))]));
                    }
                }
            }
        } else {
            // ── BM25 path ──────────────────────────────────────────────
            // Note: regex=true uses BM25 for candidates, then post-filters with the
            // actual regex on raw content (Tantivy's RegexQuery only works on individual
            // tokens, not raw text — underscores/punctuation cause empty results).
            //
            // When regex is enabled, strip metacharacters from the BM25 query so
            // Tantivy gets clean tokens (e.g. "class Cache" instead of "class \w+Cache\b").
            let bm25_query = if regex_enabled {
                let cleaned = extract_bm25_query_from_regex(&effective_query);
                if cleaned.is_empty() {
                    effective_query.clone()
                } else {
                    cleaned
                }
            } else {
                effective_query.clone()
            };
            let fts_results = if let Some(ref sv) = ctx.stores_vec {
                let sa = ctx.store_aliases.as_ref().unwrap();
                let outcome = self
                    .with_fts_store_read_multi(
                        |fts_store| {
                            if request.phrase.unwrap_or(false) {
                                fts_store.search_phrase(&bm25_query, limit * 3)
                            } else {
                                fts_store.search(&bm25_query, limit * 3, None)
                            }
                        },
                        sv.clone(),
                        sa,
                    )
                    .await
                    .unwrap_or_default();
                for (alias, err) in &outcome.failures {
                    let msg = format!("repo '{alias}' literal search failed: {err}");
                    tracing::error!("MCP: {}", msg);
                    literal_warnings.push(msg);
                }
                outcome.results
            } else {
                match self
                    .with_fts_store_read_for(
                        |fts_store| {
                            if request.phrase.unwrap_or(false) {
                                fts_store.search_phrase(&bm25_query, limit * 3)
                            } else {
                                fts_store.search(&bm25_query, limit * 3, None)
                            }
                        },
                        ctx.stores.clone(),
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "Error searching: {e:#}"
                        ))]));
                    }
                }
            };

            // Resolve chunk metadata and apply post-filters
            if let Some(ref sv) = ctx.stores_vec {
                // Multi-store: resolve chunks from all stores
                let mut items: Vec<LiteralSearchResultItem> = Vec::new();
                'outer: for fts_result in &fts_results {
                    let sa = ctx.store_aliases.as_ref().unwrap();
                    for (idx, store_arc) in sv.iter().enumerate() {
                        let store = store_arc.vector_store.read().await;
                        let looked_up = store.get_chunk(fts_result.chunk_id);
                        if let Err(ref e) = looked_up {
                            note_store_failure(&mut literal_warnings, sa, idx, "chunk lookup", e);
                        }
                        if let Some(chunk) = looked_up.ok().flatten() {
                            if let Some(ref lang) = lang_filter {
                                let file_lang =
                                    Language::from_path(std::path::Path::new(&chunk.path));
                                if file_lang.name() != lang {
                                    continue;
                                }
                            }
                            if let Some(ref glob) = glob_filter {
                                let relative_path = chunk
                                    .path
                                    .strip_prefix(&project_root_normalized)
                                    .unwrap_or(&chunk.path)
                                    .trim_start_matches('/');
                                if !simple_glob_match(glob, relative_path) {
                                    continue;
                                }
                            }
                            let match_info = match_line_for_literal(
                                &chunk.content,
                                &effective_query,
                                snippet_regex.as_ref(),
                            );
                            if regex_enabled && match_info.is_none() {
                                continue;
                            }
                            let (match_offset, snippet) = match_info.unwrap_or_else(|| {
                                (0, chunk.content.lines().next().unwrap_or("").to_string())
                            });
                            let match_line = chunk.start_line + match_offset;
                            items.push(LiteralSearchResultItem {
                                path: chunk.path,
                                start_line: match_line,
                                end_line: match_line,
                                snippet,
                                score: fts_result.score,
                                kind: if chunk.kind.is_empty() {
                                    None
                                } else {
                                    Some(chunk.kind)
                                },
                                signature: chunk.signature.filter(|s| !s.is_empty()),
                            });
                            if items.len() >= limit {
                                break 'outer;
                            }
                            break; // Found in this store
                        }
                    }
                }
                items
            } else {
                match self
                    .with_vector_store_read_for(
                        |store| {
                            // Resolve chunk metadata first so a store `Err`
                            // propagates to the error arm below ("Error
                            // resolving search results") instead of silently
                            // dropping the hit — `Ok(None)` alone is a true
                            // miss ("chunk not in this store").
                            let resolved: anyhow::Result<Vec<_>> = fts_results
                                .iter()
                                .map(|fts_result| {
                                    let chunk = store.get_chunk(fts_result.chunk_id)?;
                                    Ok((chunk, fts_result.score))
                                })
                                .collect();
                            let items: Vec<LiteralSearchResultItem> = resolved?
                                .into_iter()
                                .filter_map(|(looked_up, score)| {
                                    let chunk = looked_up?;
                                    Some((chunk, score))
                                })
                                .filter(|(chunk, _)| {
                                    if let Some(ref lang) = lang_filter {
                                        let file_lang =
                                            Language::from_path(std::path::Path::new(&chunk.path));
                                        if file_lang.name() != lang {
                                            return false;
                                        }
                                    }
                                    if let Some(ref glob) = glob_filter {
                                        let relative_path = chunk
                                            .path
                                            .strip_prefix(&project_root_normalized)
                                            .unwrap_or(&chunk.path)
                                            .trim_start_matches('/');
                                        if !simple_glob_match(glob, relative_path) {
                                            return false;
                                        }
                                    }
                                    true
                                })
                                .take(limit)
                                .filter_map(|(chunk, score)| {
                                    let match_info = match_line_for_literal(
                                        &chunk.content,
                                        &effective_query,
                                        snippet_regex.as_ref(),
                                    );
                                    if regex_enabled && match_info.is_none() {
                                        return None;
                                    }
                                    let (match_offset, snippet) = match_info.unwrap_or_else(|| {
                                        (0, chunk.content.lines().next().unwrap_or("").to_string())
                                    });
                                    let match_line = chunk.start_line + match_offset;
                                    Some(LiteralSearchResultItem {
                                        path: chunk.path,
                                        start_line: match_line,
                                        end_line: match_line,
                                        snippet,
                                        score,
                                        kind: if chunk.kind.is_empty() {
                                            None
                                        } else {
                                            Some(chunk.kind)
                                        },
                                        signature: chunk.signature.filter(|s| !s.is_empty()),
                                    })
                                })
                                .collect();
                            Ok(items)
                        },
                        ctx.stores.clone(),
                    )
                    .await
                {
                    Ok(items) => items,
                    Err(e) => {
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "Error resolving search results: {e:#}"
                        ))]));
                    }
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        // Compute low-confidence signal
        let top_score = items.first().map(|i| i.score);
        let (low_confidence, suggested_tool) =
            compute_literal_low_confidence(top_score, &request.query);

        // Build note
        let note = if auto_promoted {
            Some(format!(
                "Query auto-promoted to regex mode (original: '{}', effective: '{}'). \
                 The query contained code-like punctuation that BM25 would tokenize incorrectly.",
                request.query, effective_query
            ))
        } else if low_confidence == Some(true) {
            suggested_tool.as_ref().map(|tool| {
                format!(
                    "Top result has weak BM25 score; consider using `{}` for better matches.",
                    tool
                )
            })
        } else {
            None
        };

        let response = LiteralSearchResponse {
            results: items,
            auto_promoted_to_regex: if auto_promoted { Some(true) } else { None },
            note,
            low_confidence,
            suggested_tool: if low_confidence == Some(true) {
                suggested_tool
            } else {
                None
            },
            warnings: if literal_warnings.is_empty() {
                None
            } else {
                Some(literal_warnings)
            },
        };

        // Instrument BM25 score for threshold calibration
        if let Some(top) = response.results.first() {
            tracing::debug!(
                target: "codesearch::literal_confidence",
                query = %request.query,
                top_bm25_score = top.score,
                result_count = response.results.len(),
                "literal_search score sample"
            );
        }

        // Format output
        let output = if output_format == "grep" {
            let mut lines: Vec<String> = Vec::new();
            if response.auto_promoted_to_regex == Some(true) {
                lines.push(
                    "# auto-promoted to regex mode (query contained code-like punctuation)"
                        .to_string(),
                );
            }
            if response.low_confidence == Some(true) {
                if let Some(ref hint) = response.suggested_tool {
                    lines.push(format!("# low confidence — consider: {}", hint));
                }
            }
            for item in &response.results {
                lines.push(format!(
                    "{}:{}:{}",
                    item.path, item.start_line, item.snippet
                ));
            }
            lines.join("\n")
        } else {
            serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string())
        };

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Internal implementation for index_status with optional project/group routing.
    async fn index_status_impl(
        &self,
        project: Option<String>,
        group: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        // When no project/group specified in serve mode, return lightweight aggregated
        // status WITHOUT opening any databases. Only a specific project/group request
        // should trigger DB activation.
        if project.is_none() && group.is_none() {
            if let Some(ref serve_state) = self.serve_state {
                let config = serve_state.config_snapshot();
                let repo_count = config.repos.len();
                // Count the virtual "all" group when repos are registered, so the
                // summary doesn't read "0 group(s)" while `all` is actually available.
                let group_count = config.groups.len() + if config.repos.is_empty() { 0 } else { 1 };
                let statuses = serve_state.repo_statuses_lightweight();
                let open_count = statuses
                    .iter()
                    .filter(|(_, r)| matches!(r.status, crate::serve::RepoStateLabel::Open))
                    .count();
                let warm_count = statuses
                    .iter()
                    .filter(|(_, r)| matches!(r.status, crate::serve::RepoStateLabel::Warm))
                    .count();
                let closed_count = statuses
                    .iter()
                    .filter(|(_, r)| matches!(r.status, crate::serve::RepoStateLabel::Closed))
                    .count();

                let status = if open_count + warm_count > 0 {
                    "ready".to_string()
                } else if repo_count > 0 {
                    "idle".to_string()
                } else {
                    "no_repos".to_string()
                };

                let status_message = format!(
                    "{} repo(s) registered, {} group(s). Open: {}, Warm: {}, Closed: {}.",
                    repo_count, group_count, open_count, warm_count, closed_count
                );

                let response = IndexStatusResponse {
                    indexed: open_count + warm_count > 0,
                    status,
                    status_message,
                    total_chunks: 0, // Not available without opening DBs
                    total_files: 0,
                    model: self.model_type.short_name().to_string(),
                    dimensions: 0,
                    max_chunk_id: 0,
                    db_path: format!("({} repos)", repo_count),
                    project_path: format!("serve mode — {} repo(s)", repo_count),
                    error_message: None,
                    mode: self.mcp_mode(),
                };

                let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
        }

        // Resolve project/group routing — status is scope-free, allow unscoped fan-out
        let ctx = match self.resolve_routing(&project, &group, true, "status").await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            let indexed = self.db_path.exists();

            if !indexed {
                let response = IndexStatusResponse {
                    indexed: false,
                    status: "not_indexed".to_string(),
                    status_message: "No index found. Run 'codesearch index' or start with --create-index=true to automatically create one.".to_string(),
                    total_chunks: 0,
                    total_files: 0,
                    model: "none".to_string(),
                    dimensions: 0,
                    max_chunk_id: 0,
                    db_path: self.db_path.display().to_string(),
                    project_path: self.project_path.display().to_string(),
                    error_message: None,
                    mode: self.mcp_mode(),
                };
                let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
        }

        if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: aggregate stats across all group members
            let mut total_chunks = 0usize;
            let mut total_files = 0usize;
            let mut max_chunk_id = 0u32;
            let mut dimensions = 0usize;
            let mut all_indexed = true;
            let aliases = ctx.aliases();
            let mut stats_warnings: Vec<String> = Vec::new();
            let mut failed_count = 0usize;

            for (i, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.stats() {
                    Ok(stats) => {
                        total_chunks += stats.total_chunks;
                        total_files += stats.total_files;
                        if stats.max_chunk_id > max_chunk_id {
                            max_chunk_id = stats.max_chunk_id;
                        }
                        if stats.dimensions > 0 {
                            dimensions = stats.dimensions;
                        }
                        if !stats.indexed {
                            all_indexed = false;
                        }
                    }
                    // `all_indexed = false` alone renders identically to "still
                    // warming" — the caller has no way to tell "wait" from "this
                    // store is down". This is the tool whose job is reporting index
                    // health, so it must not stay silent on the one signal that
                    // matters here: bind the error, carry it, never `Err(_)`.
                    Err(ref e) => {
                        all_indexed = false;
                        failed_count += 1;
                        note_store_failure(&mut stats_warnings, aliases, i, "stats", e);
                    }
                }
            }

            let (status, status_message) =
                index_status_summary(sv.len(), failed_count, total_chunks);

            let response = IndexStatusResponse {
                indexed: all_indexed,
                status,
                status_message,
                total_chunks,
                total_files,
                model: self.model_type.short_name().to_string(),
                dimensions,
                max_chunk_id,
                db_path: format!("({} repos)", sv.len()),
                project_path: format!("group with {} repo(s)", sv.len()),
                error_message: None,
                mode: self.mcp_mode(),
            };

            return respond_with_object(&response, &stats_warnings);
        }

        // Single-store path
        let stats = match self
            .with_vector_store_read_for(
                |store| store.stats().context("Error getting index stats"),
                ctx.stores.clone(),
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                let response = IndexStatusResponse {
                    indexed: false,
                    status: "error".to_string(),
                    status_message: format!("{}", e),
                    total_chunks: 0,
                    total_files: 0,
                    model: self.model_type.short_name().to_string(),
                    dimensions: 0,
                    max_chunk_id: 0,
                    db_path: self.db_path.display().to_string(),
                    project_path: self.project_path.display().to_string(),
                    error_message: Some(format!("{}", e)),
                    mode: self.mcp_mode(),
                };
                let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
        };

        // Determine status based on database state
        let (status, status_message) = if stats.total_chunks == 0 {
            (
                "building".to_string(),
                "Index is being built in the background. Searches may fail until indexing completes. Please check back in a few minutes.".to_string(),
            )
        } else {
            (
                "ready".to_string(),
                "Index is ready for searching.".to_string(),
            )
        };

        let response = IndexStatusResponse {
            indexed: stats.indexed,
            status,
            status_message,
            total_chunks: stats.total_chunks,
            total_files: stats.total_files,
            model: self.model_type.short_name().to_string(),
            dimensions: stats.dimensions,
            max_chunk_id: stats.max_chunk_id,
            db_path: self.db_path.display().to_string(),
            project_path: self.project_path.display().to_string(),
            error_message: None,
            mode: self.mcp_mode(),
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// List all registered projects and groups. Called by `status(kind="projects")`.
    /// Build the `remote_projects` listing (opt-in mounts) for `list_projects`.
    fn remote_projects_listing(
        config: &crate::db_discovery::repos::ReposConfig,
    ) -> Vec<RemoteProjectInfo> {
        config
            .mounted_remote_projects()
            .into_iter()
            .filter_map(|(name, target)| match target {
                crate::db_discovery::repos::Target::RemoteProject {
                    peer_name,
                    peer,
                    remote_alias,
                } => Some(RemoteProjectInfo {
                    name,
                    peer: peer_name,
                    remote_alias,
                    peer_url: peer.url,
                }),
                _ => None,
            })
            .collect()
    }

    async fn list_projects(&self) -> Result<CallToolResult, McpError> {
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let serve_active = self.serve_state.is_some();
        let serve_url = if serve_active {
            Some(serve_url_from_env())
        } else {
            None
        };

        // When serve is active, use ServeState as source of truth for lock status
        if let Some(ref serve_state) = self.serve_state {
            let config = serve_state.config_snapshot();
            let project_groups = config.project_groups();
            let mut repos_info = Vec::new();
            let mut list_warnings: Vec<String> = Vec::new();

            for (alias, path) in &config.repos {
                let db_path = path.join(crate::constants::DB_DIR_NAME);

                let (total_chunks, total_files, model, lock_status, error) = if db_path.exists() {
                    let (model_name, _dims) = read_model_metadata(&db_path);

                    // For repos already opened in DashMap, use the live SharedStores for stats
                    // WITHOUT opening a new VectorStore connection.
                    // For unopened repos, just report metadata — do NOT open the DB.
                    if let Some(stores) = serve_state.get_opened_stores(alias) {
                        let stats_result = {
                            let vs = stores.vector_store.read().await;
                            vs.stats()
                        };
                        // `0 chunks` alone reads exactly like "not indexed yet" — the
                        // repo may in fact be full and simply failing to answer (the
                        // read-only-incident shape this branch exists for). Attribute
                        // the failure to THIS repo rather than a top-level channel:
                        // list_projects returns one entry per repo, so per-item is the
                        // shape that actually matches the fan-out.
                        // `repo_stats_from_result` carries only the part of this
                        // decision that varies by Ok/Err — see its doc comment.
                        // `record_stats_or_warn` wraps it so this call site cannot
                        // silently drop the warning half without also breaking the
                        // counts it returns — see its own doc comment.
                        let (total_chunks, total_files, error) =
                            record_stats_or_warn(stats_result, alias, &mut list_warnings);
                        (
                            total_chunks,
                            total_files,
                            model_name,
                            serve_state
                                .repo_lock_status(alias)
                                .unwrap_or("unknown")
                                .to_string(),
                            error,
                        )
                    } else {
                        // Repo NOT opened — read persisted stats from metadata.json
                        let (md_chunks, md_files) = read_metadata_stats(&db_path);
                        let lock_status = if crate::index::is_database_locked(&db_path) {
                            "locked-externally".to_string()
                        } else {
                            "available".to_string()
                        };
                        (md_chunks, md_files, model_name, lock_status, None)
                    }
                } else {
                    (0, 0, "not indexed".to_string(), "unknown".to_string(), None)
                };

                repos_info.push(RepoInfo {
                    alias: alias.clone(),
                    project_path: path.display().to_string(),
                    database_path: db_path.display().to_string(),
                    total_chunks,
                    total_files,
                    model,
                    lock_status,
                    groups: project_groups.get(alias).cloned().unwrap_or_default(),
                    error,
                });
            }

            let response = ListProjectsResponse {
                repos: repos_info,
                groups: config.groups_with_virtual_all(),
                remote_projects: Self::remote_projects_listing(&config),
                serve_active,
                serve_url,
                current_directory: current_dir.display().to_string(),
            };

            return respond_with_object(&response, &list_warnings);
        }

        // Stdio mode: fall back to disk-based lock detection
        let config = load_repos_config().unwrap_or_default();
        let project_groups = config.project_groups();
        let mut repos_info = Vec::new();
        for (alias, path) in &config.repos {
            let db_path = path.join(crate::constants::DB_DIR_NAME);

            // Get stats
            let (total_chunks, total_files, model, lock_status) = if db_path.exists() {
                let (model_name, dims) = read_model_metadata(&db_path);

                let lock = if crate::index::is_database_locked(&db_path) {
                    "conflicted"
                } else {
                    "available"
                };

                if let Ok(store) = VectorStore::new(&db_path, dims) {
                    if let Ok(stats) = store.stats() {
                        (
                            stats.total_chunks,
                            stats.total_files,
                            model_name,
                            lock.to_string(),
                        )
                    } else {
                        (0, 0, model_name, lock.to_string())
                    }
                } else {
                    (0, 0, model_name, "readonly".to_string())
                }
            } else {
                (0, 0, "not indexed".to_string(), "unknown".to_string())
            };

            // Stdio mode is single-repo-at-a-time CLI usage, not the live multi-repo
            // federation this fan-out fix targets — a stats() failure here is out of
            // scope for this fix (VectorStore::new/stats failing locally is a different
            // shape than a store going down mid-request in a shared serve process).
            repos_info.push(RepoInfo {
                alias: alias.clone(),
                project_path: path.display().to_string(),
                database_path: db_path.display().to_string(),
                total_chunks,
                total_files,
                model,
                lock_status,
                groups: project_groups.get(alias).cloned().unwrap_or_default(),
                error: None,
            });
        }

        let response = ListProjectsResponse {
            repos: repos_info,
            groups: config.groups_with_virtual_all(),
            remote_projects: Self::remote_projects_listing(&config),
            serve_active,
            serve_url,
            current_directory: current_dir.display().to_string(),
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
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
        Self::tool_router()
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
