use super::helpers::prefix_path_with_alias;
use crate::embed::ModelType;
use crate::index::SharedStores;
use crate::vectordb::VectorStore;
use rmcp::model::{CallToolResult, Content};
use rmcp::ErrorData as McpError;
use std::path::Path;
use std::sync::Arc;

/// Read model short-name and dimensions from a database's `metadata.json`.
/// Returns `(model_name, dimensions)`, defaulting to `("unknown", DEFAULT_EMBEDDING_DIMENSIONS)`.
pub(crate) fn read_model_metadata(db_path: &Path) -> (String, usize) {
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
pub(crate) fn read_metadata_stats(db_path: &Path) -> (usize, usize) {
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
pub(crate) fn live_chunk_count(db_path: &Path) -> Option<(usize, usize)> {
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
pub(crate) const LOW_CONFIDENCE_THRESHOLD: f32 = 0.02;

/// Chunk kinds that represent symbol definitions (not usages/comments/etc.)
pub(crate) const DEFINITION_KINDS: &[&str] = &[
    "Function",
    "Class",
    "Method",
    "Struct",
    "Trait",
    "Enum",
    "TypeAlias",
    "Interface",
];

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
pub(crate) struct MultiReadOutcome<R> {
    /// Merged, deduplicated, score-sorted results from the stores that worked.
    pub(crate) results: Vec<R>,
    /// `(alias, full error chain)` for every store that failed. Empty on a
    /// clean run.
    pub(crate) failures: Vec<(String, String)>,
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
pub(crate) fn index_status_summary(
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
pub(crate) fn repo_stats_from_result(
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
pub(crate) fn record_stats_or_warn(
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
pub(crate) fn note_store_failure(
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
pub(crate) fn store_warning(alias: &str, what: &str, err: &str) -> String {
    format!("repo '{alias}' {what} failed: {err}")
}

/// Append a warning unless it is already present, logging it once.
pub(crate) fn push_store_warning(warnings: &mut Vec<String>, msg: &str) {
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
pub(crate) fn respond_with_items<T: serde::Serialize>(
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
pub(crate) fn respond_with_items_noted<T: serde::Serialize>(
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
pub(crate) fn respond_with_object<T: serde::Serialize>(
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
pub(crate) fn ambiguous_chunk_payload(
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
pub(crate) fn retry_hint(
    suggested: Option<String>,
    warnings: &Option<Vec<String>>,
) -> Option<String> {
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
pub(crate) fn qualify_empty_result(message: String, warnings: &[String]) -> String {
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
    pub(crate) fn warnings(&self, what: &str) -> Vec<String> {
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
    pub(crate) fn into_results(self, warnings: &mut Vec<String>, what: &str) -> Vec<R> {
        for (alias, err) in &self.failures {
            push_store_warning(warnings, &store_warning(alias, what, err));
        }
        self.results
    }
}

pub(crate) struct MultiStoreContext {
    /// Single-store override (set when exactly 1 repo resolved, or None).
    /// Pass to `with_*_store_read_for()` methods.
    pub(crate) stores: Option<Arc<SharedStores>>,
    /// Multi-store vec for fan-out (set when 2+ repos resolved, or None).
    /// Use `if let Some(ref sv) = ctx.stores_vec { ... }` for the multi-store path.
    pub(crate) stores_vec: Option<Vec<Arc<SharedStores>>>,
    /// Alias for each store in `stores_vec` (parallel with stores_vec).
    /// Used for path prefixing and per-alias dedup.
    pub(crate) store_aliases: Option<Vec<String>>,
    /// Alias for single-project routing (set when project= is given).
    pub(crate) project_alias: Option<String>,
    /// Normalized project root for each alias (alias → root path).
    /// Used by `prefix_path` to strip absolute paths and add alias prefix.
    pub(crate) alias_roots: std::collections::HashMap<String, String>,
    /// True when `stores_vec` has 2+ entries (group fan-out).
    pub(crate) is_multi: bool,
    /// True when no serve-state stores resolved and local DB should be checked.
    pub(crate) needs_local_db: bool,
}

impl MultiStoreContext {
    /// Aliases parallel to `stores_vec`, or an empty slice when absent.
    ///
    /// Every fan-out that reports a per-store failure needs this, and hand-rolling
    /// `let empty = Vec::new(); ...unwrap_or(&empty)` at each site produced four
    /// copies of the same two lines — and one handler where the binding was out
    /// of scope, which is how a silent store read survived a round of review.
    pub(crate) fn aliases(&self) -> &[String] {
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
    pub(crate) fn prefix_result_path(&self, path: &str) -> String {
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
