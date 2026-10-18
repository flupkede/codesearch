//! C# symbol indexer adapter.
//!
//! Detects the `scip-csharp` helper binary, invokes it as a subprocess
//! to produce a JSON symbol index from a .sln/.csproj, parses the
//! output, and stores references in LMDB.
//!
//! ## Reference model
//!
//! `rebuild()` calls `scip-csharp index`, which emits definitions and — on a
//! full-scope run of a helper that supports it — every solution reference.
//! Those references are written to `scip_ref_cache` in the rebuild txn and
//! the meta `refs_coverage` stamp records how much of the cache that covers
//! (see [`crate::constants::SCIP_REFS_COVERAGE_KEY`]).
//!
//! `find_references_for_key()`:
//! 1. Return definitions from `scip_symbols` (always populated after rebuild).
//! 2. Check `scip_ref_cache` — return if present.
//! 3. Cache miss: invoke `scip-csharp find-refs` for the single requested symbol,
//!    cache the result in `scip_ref_cache`, then return.
//!
//! ## Incremental rebuild (Opt 3 — RebuildScope::Files)
//!
//! When a `.cs` file changes, the 60s debounce fires with `RebuildScope::Files`.
//! Instead of clearing and rebuilding the entire LMDB, the adapter:
//! - Runs `scip-csharp index --filter-project <affected.csproj>` (faster).
//! - Merges the result: updates symbols and positions for affected files only;
//!   symbols from other projects are preserved.
//! - Rebuilds `scip_simple_names` from all current `scip_symbols` entries.
//! - Clears `scip_ref_cache`; references then resolve lazily.
//!
//! ## Phase 3 pre-warm (background ref cache filling)
//!
//! For an index without precomputed references (`refs_coverage` = lazy),
//! Phase 3 runs `scip-csharp batch-find-refs` to resolve references for all
//! symbols in one workspace session, amortizing the workspace open cost.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::lmdb_registry::TrackedEnv;
use anyhow::{bail, Context, Result};
use heed::types::{Bytes, Str};
use heed::Database;
use serde::{Deserialize, Serialize};

use super::scip_parse;
use super::{
    ImpactQuery, KeyMatch, PrewarmSummary, RebuildScope, RebuildSummary, SymbolIndexer,
    SymbolReference,
};

// ── Helper stderr routing ─────────────────────────────────────────

/// True when a scip-csharp stderr line carries warning-severity output:
/// the helper's own `[WARN]` prefix or MSBuildWorkspace's `[Failure]`
/// workspace diagnostics (e.g. project-load failures).
pub(crate) fn is_helper_warning_line(line: &str) -> bool {
    line.contains("[WARN]") || line.contains("[Failure]")
}

/// The version token out of the helper's `--version` line
/// ("scip-csharp 1.5.4" → "1.5.4"); anything unparseable → "unknown", which
/// still logs (as a mismatch against the host version) instead of silently
/// skipping the startup entry.
pub(super) fn parse_helper_version(version_line: &str) -> String {
    version_line
        .split_whitespace()
        .last()
        .filter(|tok| tok.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or("unknown")
        .to_string()
}

/// Cap a line at `max` chars, cutting at the last whitespace inside the
/// budget when there is one: a mid-word cut makes compilation-error
/// specimens (the quoted CS0103 name) unreadable. A single token longer
/// than the budget still takes the hard cap.
fn cap_at_whitespace_boundary(line: &str, max: usize) -> String {
    let cut = line
        .char_indices()
        .nth(max)
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    if cut == line.len() {
        return line.to_string();
    }
    match line[..cut].rfind(char::is_whitespace) {
        Some(pos) => line[..pos].to_string(),
        None => line[..cut].to_string(),
    }
}

/// Normalize one warning-severity helper stderr line into a concise,
/// index-level completeness warning entry. MSBuild project-load failures
/// (the class that silently strips cross-project references from the index)
/// collapse to `<project-file>: <message>`; anything else passes through
/// trimmed. `None` for non-warning lines.
pub(crate) fn normalize_index_warning_line(line: &str) -> Option<String> {
    if !is_helper_warning_line(line) {
        return None;
    }
    // "[WARN] Workspace error: [Failure] Msbuild failed when processing the
    // file '<path>' with message: <msg>" → "<file_name>: <msg>"
    if let Some((_, rest)) = line.split_once("Msbuild failed when processing the file '") {
        if let Some((path, tail)) = rest.split_once('\'') {
            let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
            let msg = tail
                .split_once("with message: ")
                .map(|(_, m)| m)
                .unwrap_or(tail)
                .trim();
            let msg: String = msg.chars().take(160).collect();
            return Some(format!("{file}: {msg}"));
        }
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(cap_at_whitespace_boundary(trimmed, 220))
}

/// Collapse the raw warning lines of one helper run into the capped,
/// deduplicated list persisted as `index_warnings` meta. Order-preserving
/// dedup because MSBuild repeats the same failure per project and per
/// pass; capped because the list rides on every find_impact answer.
pub(crate) fn summarize_index_warnings(lines: &[String], cap: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = lines
        .iter()
        .filter_map(|l| normalize_index_warning_line(l))
        .filter(|w| seen.insert(w.clone()))
        .collect();
    if out.len() > cap {
        let dropped = out.len() - cap;
        out.truncate(cap);
        out.push(format!("… and {dropped} more distinct warning(s)"));
    }
    out
}

/// The `refs_coverage` stamp for one rebuild. A run with index warnings may
/// have resolved too few references, so its zero-reference symbols stay lazy.
pub(crate) fn refs_coverage_for(precomputed: bool, clean: bool) -> &'static str {
    match (precomputed, clean) {
        (false, _) => crate::constants::SCIP_REFS_COVERAGE_LAZY,
        (true, true) => crate::constants::SCIP_REFS_COVERAGE_COMPLETE,
        (true, false) => crate::constants::SCIP_REFS_COVERAGE_PARTIAL,
    }
}

/// Whether the Phase 3 batch prewarm still has work: only a lazy (or
/// unstamped) index lacks precomputed references.
pub(crate) fn ref_prewarm_needed(refs_coverage: Option<&str>) -> bool {
    !matches!(
        refs_coverage,
        Some(crate::constants::SCIP_REFS_COVERAGE_COMPLETE)
            | Some(crate::constants::SCIP_REFS_COVERAGE_PARTIAL)
    )
}

/// The stored form of the summarized warnings: a stand-alone summary entry
/// leads, because the list rides on every find_impact answer AND the TUI
/// info panel and must be understandable alone.
pub(crate) fn index_warnings_stored(entries: &[String]) -> Vec<String> {
    if entries.is_empty() {
        return Vec::new();
    }
    let mut stored = vec![format!(
        "The symbol index was built while the C# workspace reported {} distinct failure(s) — cross-project references may be missing from every answer. Failures `dotnet build` also reports are real and fixable in the repo (often a missing dotnet restore); failures only scip-csharp reports are tool-side — please report them and reindex.",
        entries.len()
    )];
    stored.extend(entries.iter().cloned());
    stored
}

/// Route one scip-csharp stderr line into tracing at the right severity.
/// This is the ONLY sanctioned path for helper stderr: spawn helpers with
/// `Stdio::piped()` and drain through here — never `Stdio::inherit()`,
/// which bypasses tracing entirely and sprays raw MSBuild output over the
/// serve process (file-only logging, TUI on stderr), scrambling it.
pub(crate) fn emit_helper_stderr_line(tag: &str, label: &str, line: &str) {
    if is_helper_warning_line(line) {
        tracing::warn!("[{tag}:{label}] {line}");
    } else {
        tracing::info!("[{tag}:{label}] {line}");
    }
}

/// Drain a helper output pipe to EOF, invoking `emit` once per line.
/// Undecodable bytes are lossy-decoded and the line is still emitted, and
/// a persistent read error ends the drain. A drain must never stop on
/// individual bad lines: a stalled drain lets the pipe fill and blocks the
/// helper mid-workspace-load (`lines().map_while(Result::ok)` had exactly
/// that failure mode; `filter_map(Result::ok)` traded it for a busy spin
/// under `clippy::lines_filter_map_ok` — read_until has neither problem).
pub(crate) fn drain_pipe_to_tracing<R: std::io::Read>(pipe: R, mut emit: impl FnMut(&str)) {
    let mut reader = BufReader::new(pipe);
    loop {
        let mut buf = Vec::new();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break, // EOF — helper died; drain thread exits here
            Ok(_) => {
                while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
                    buf.pop();
                }
                emit(&String::from_utf8_lossy(&buf));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                continue; // transient (EINTR) — retry the read, do NOT end the drain
            }
            Err(_) => break, // dead pipe — nothing more to drain
        }
    }
}

// ── Constants ─────────────────────────────────────────────────────

/// LMDB database name for the SCIP symbol table (definition occurrences).
const SCIP_DB_NAME: &str = crate::constants::SCIP_SYMBOLS_DB_NAME;

/// LMDB database name for the rebuild timestamp.
const SCIP_META_DB_NAME: &str = crate::constants::SCIP_META_DB_NAME;

/// LMDB database name for the position-to-symbols index.
const SCIP_POSITION_DB_NAME: &str = crate::constants::SCIP_POSITION_DB_NAME;

/// LMDB database name for the simple-name-to-symbols index.
const SCIP_SIMPLE_NAMES_DB_NAME: &str = crate::constants::SCIP_SIMPLE_NAMES_DB_NAME;

/// LMDB database name for the on-demand reference cache (populated by find-refs).
const SCIP_REF_CACHE_DB_NAME: &str = crate::constants::SCIP_REF_CACHE_DB_NAME;

/// LMDB database name for per-symbol completeness warnings persisted
/// alongside the reference cache (absence of an entry = complete).
const SCIP_REF_WARNINGS_DB_NAME: &str = crate::constants::SCIP_REF_WARNINGS_DB_NAME;

/// Key in the meta database that stores the last rebuild timestamp (UNIX epoch seconds).
const META_REBUILD_TS: &str = crate::constants::SCIP_REBUILD_TIMESTAMP_KEY;

/// Key in the meta database storing the git HEAD sha the index was built for.
const META_HEAD_SHA: &str = crate::constants::SCIP_HEAD_SHA_KEY;

/// Key in the meta database recording the key-format generation the index was
/// built with (see [`crate::constants::SCIP_KEY_FORMAT`]).
const META_KEY_FORMAT: &str = crate::constants::SCIP_KEY_FORMAT_KEY;

/// Key in the meta database holding the JSON array of index-level
/// completeness warnings from the last rebuild (see
/// [`summarize_index_warnings`]).
const META_INDEX_WARNINGS: &str = crate::constants::SCIP_INDEX_WARNINGS_KEY;
const META_BUILDER_VERSION: &str = crate::constants::SCIP_INDEX_BUILDER_VERSION_KEY;
const META_REFS_COVERAGE: &str = crate::constants::SCIP_REFS_COVERAGE_KEY;

/// How many distinct index-level warnings are persisted (and thus surfaced
/// per find_impact answer) before the list is capped with an overflow note.
const INDEX_WARNINGS_CAP: usize = 10;

/// Key in the meta database storing the count of indexed symbols.
#[allow(dead_code)]
const META_SYMBOL_COUNT: &str = "symbol_count";

/// Key in the meta database storing the absolute repo path (set during rebuild,
/// used by find_refs_for_canonical_key to locate the .sln for lazy ref resolution).
const META_REPO_PATH: &str = "repo_path";

/// Environment variable override for the helper binary path.
const HELPER_ENV_VAR: &str = crate::constants::SCIP_CSHARP_HELPER_ENV;

/// Helper binary name (without extension).
const HELPER_BIN_NAME: &str = crate::constants::SCIP_CSHARP_HELPER_NAME;

/// Debounce period for .cs file changes (seconds).
#[allow(dead_code)]
pub const CSHARP_REBUILD_DEBOUNCE_SECS: u64 = crate::constants::SCIP_CSHARP_DEBOUNCE_MS / 1000;

/// Exit code scip-csharp uses for "a dotnet restore for this solution is
/// still running" (issue #225): it printed one `[RESTORE-PENDING]` line,
/// wrote no output file, and built no index.
pub(crate) const HELPER_RESTORE_PENDING_EXIT: i32 = 75;

/// Base of the exponential backoff between restore-pending retries of the
/// same rebuild scope: retry N waits base × 2^(N-1).
const RESTORE_PENDING_RETRY_BASE_INTERVAL_SECS: u64 = 60;

/// Granularity of the restore wait's shutdown check.
const RESTORE_PENDING_POLL_MS: u64 = 500;

/// Restore-pending helper attempts before a rebuild gives up with LMDB
/// untouched: initial attempt + 2 retries. Backoff 60s + 120s = 3 min of
/// restore patience, far below the MAX_INDEXING_SECS (30 min)
/// claim-takeover in serve so a still-running rebuild is never mistaken
/// for a leaked one and double-driven.
const RESTORE_PENDING_RETRY_MAX_ATTEMPTS: u32 = 3;

/// Retry base seconds: `CODESEARCH_RESTORE_RETRY_INTERVAL_SECS` when set
/// to a positive integer, otherwise the built-in 60. Zero and unparseable
/// fall back to the default (a zero interval would hot-spin the helper);
/// tests use 1 to exercise the retry loop in seconds.
fn restore_pending_retry_interval_secs() -> u64 {
    let raw = std::env::var(crate::constants::SCIP_RESTORE_RETRY_INTERVAL_SECS_ENV).ok();
    raw.and_then(|v| v.parse::<u64>().ok())
        .filter(|&secs| secs > 0)
        .unwrap_or(RESTORE_PENDING_RETRY_BASE_INTERVAL_SECS)
}

/// Backoff before restore-pending retry number `attempts` (1-based, the
/// count of pending exits so far): base × 2^(attempts-1), saturating.
fn restore_pending_retry_delay_secs(attempts: u32) -> u64 {
    restore_pending_retry_interval_secs().saturating_mul(1u64 << (attempts - 1).min(16))
}

/// Sleeps `total` in poll steps; returns `false` as soon as shutdown is
/// requested. A rebuild parked here must not outlive quit: the runtime joins
/// its blocking pool at teardown.
fn sleep_unless_shutdown(total: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + total;
    loop {
        if crate::constants::is_shutdown_requested() {
            return false;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return true;
        }
        thread::sleep(remaining.min(std::time::Duration::from_millis(RESTORE_PENDING_POLL_MS)));
    }
}

/// Typed marker for the helper's RESTORE-PENDING exit: distinct from a
/// generic failure so callers can leave LMDB untouched and retry instead of
/// surfacing an error (issue #225).
#[derive(Debug, thiserror::Error)]
#[error("dotnet restore still running for this solution (restore pending)")]
pub struct RestorePendingError;

/// True when `e` is the typed restore-pending outcome.
pub(super) fn is_restore_pending(e: &anyhow::Error) -> bool {
    e.downcast_ref::<RestorePendingError>().is_some()
}

/// Typed marker for the restore retry cap being exhausted: the previous
/// index stays live and LMDB is untouched, so callers render a warning
/// (stale-but-usable index), never a red error.
#[derive(Debug, thiserror::Error)]
#[error(
    "dotnet restore for this solution did not finish within {attempts} attempts \
     (backoff {backoff}) — rebuild abandoned, the previous index stays live \
     (issue #225)"
)]
pub struct RestoreExhaustedError {
    pub attempts: u32,
    /// Concrete waits between attempts, e.g. "60s + 120s".
    pub backoff: String,
}

/// True when `e` is the typed restore-exhausted outcome.
pub(crate) fn is_restore_exhausted(e: &anyhow::Error) -> bool {
    e.downcast_ref::<RestoreExhaustedError>().is_some()
}

/// Solutions whose rebuild retry loop is currently waiting out a restore.
/// find_impact reads this through
/// [`SymbolIndexer::restore_pending`] to attach one caller-facing warning
/// while answering from the previous index. Lifetime = the retry loop: set
/// on the first pending exit, cleared when the loop ends either way.
static RESTORE_PENDING_SOLUTIONS: std::sync::LazyLock<std::sync::Mutex<HashSet<PathBuf>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

pub(super) fn restore_pending_flag_set(db_path: &Path) {
    match RESTORE_PENDING_SOLUTIONS.lock() {
        Ok(mut set) => {
            set.insert(db_path.to_path_buf());
        }
        Err(poisoned) => {
            // A poisoned lock still holds every entry inserted before the
            // panic — recover rather than drop the honesty signal.
            poisoned.into_inner().insert(db_path.to_path_buf());
        }
    }
}

pub(super) fn restore_pending_flag_clear(db_path: &Path) {
    match RESTORE_PENDING_SOLUTIONS.lock() {
        Ok(mut set) => {
            set.remove(db_path);
        }
        Err(poisoned) => {
            poisoned.into_inner().remove(db_path);
        }
    }
}

/// Caller-facing warning find_impact attaches to every answer from an index
/// whose rebuild is waiting out a restore.
pub(crate) const RESTORE_PENDING_INDEX_WARNING: &str =
    "dotnet restore in progress — index refresh pending; this answer comes from the previous index.";

/// Per-symbol warning persisted when a lazy find-refs is refused mid-restore:
/// rides the existing ref-warnings channel and is replaced by the next
/// successful resolution, so a partial answer can never pass for complete.
const RESTORE_PENDING_REF_WARNING: &str =
    "restore in progress — references could not be resolved; this answer lists definitions only and is INCOMPLETE";

// ── Temp-file RAII guard ──────────────────────────────────────────

/// Deletes `self.0` when dropped, even on early `?` returns.
///
/// Prefer this over manual `remove_file` calls around fallible operations:
/// if an intermediate step fails and the function returns early, the temp file
/// is still cleaned up, preventing accumulation of stale `.json` files in the
/// system temp directory.
struct TempFileGuard(std::path::PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ── Serialized reference type (stored in LMDB via bincode) ────────

/// Schema version byte prepended to all bincode payloads stored in LMDB.
/// Bump whenever `StoredReference` (or any other stored struct) changes shape.
const STORED_REFERENCE_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredReference {
    file: PathBuf,
    start_line: u32,
    end_line: u32,
    kind: String,
}

/// Serialize references with a leading version byte.
fn serialize_refs(refs: &[StoredReference]) -> Result<Vec<u8>> {
    let payload = bincode::serialize(refs).with_context(|| "bincode serialize failed")?;
    let mut buf = Vec::with_capacity(1 + payload.len());
    buf.push(STORED_REFERENCE_SCHEMA_VERSION);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Deserialize references, validating the version byte first.
fn deserialize_refs(bytes: &[u8]) -> Result<Vec<StoredReference>> {
    if bytes.is_empty() {
        anyhow::bail!("Empty stored value");
    }
    let version = bytes[0];
    if version != STORED_REFERENCE_SCHEMA_VERSION {
        anyhow::bail!(
            "Unsupported stored reference schema version {} (expected {}). \
             Run `codesearch reindex --symbols` to rebuild.",
            version,
            STORED_REFERENCE_SCHEMA_VERSION
        );
    }
    bincode::deserialize(&bytes[1..]).with_context(|| "bincode deserialize failed")
}

// ── Key-list serialization (for position + simple-name indexes) ─────

/// Schema version byte for key-list payloads (Vec<String>).
const KEYS_LIST_SCHEMA_VERSION: u8 = 1;

/// Serialize a list of symbol keys with a leading version byte.
fn serialize_keys_v1(keys: &[String]) -> Result<Vec<u8>> {
    let payload = bincode::serialize(keys).with_context(|| "bincode serialize keys failed")?;
    let mut buf = Vec::with_capacity(1 + payload.len());
    buf.push(KEYS_LIST_SCHEMA_VERSION);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Deserialize a list of symbol keys, validating the version byte first.
fn deserialize_keys_v1(bytes: &[u8]) -> Result<Vec<String>> {
    if bytes.is_empty() {
        anyhow::bail!("Empty stored key list");
    }
    let version = bytes[0];
    if version != KEYS_LIST_SCHEMA_VERSION {
        anyhow::bail!(
            "Unsupported key list schema version {} (expected {}). \
             Run `codesearch reindex --symbols` to rebuild.",
            version,
            KEYS_LIST_SCHEMA_VERSION
        );
    }
    bincode::deserialize(&bytes[1..]).with_context(|| "bincode deserialize keys failed")
}

/// Context for a SCIP LMDB write. A bare `MDB_BAD_VALSIZE` names neither the
/// table nor the offending key, which is why the 2026-09 wipe loop ran blind:
/// the key size (LMDB rejects 0 and >511 bytes) is the whole diagnosis.
fn put_ctx(db_name: &str, key: &str, value_len: usize) -> String {
    format!(
        "LMDB put into '{}' failed — key {} byte(s), value {} byte(s), key: {:.160}",
        db_name,
        key.len(),
        value_len,
        key
    )
}

// ── Ref-resolution warnings persistence ───────────────────────────

/// Persist one canonical key's resolution warnings into `scip_ref_warnings`,
/// in the caller's transaction so the cached refs and their honesty land as
/// ONE atomic fact. Empty warnings REMOVE the entry — absence means
/// "complete", so a later clean re-resolution clears a stale warning
/// instead of reporting it forever. (Wire format = the key-list format:
/// version byte + bincode Vec<String>.)
fn store_ref_warnings(
    env: &TrackedEnv,
    wtxn: &mut heed::RwTxn<'_>,
    canonical: &str,
    warnings: &[String],
) -> Result<()> {
    let db: Database<Str, Bytes> = env.create_database(wtxn, Some(SCIP_REF_WARNINGS_DB_NAME))?;
    if warnings.is_empty() {
        db.delete(wtxn, canonical)?;
    } else {
        let bytes = serialize_keys_v1(warnings)
            .with_context(|| format!("Failed to serialize warnings for {canonical}"))?;
        db.put(wtxn, canonical, &bytes)
            .with_context(|| put_ctx(SCIP_REF_WARNINGS_DB_NAME, canonical, bytes.len()))?;
    }
    Ok(())
}

/// Read one canonical key's warnings. Missing database or entry — and an
/// undecodable value — read as empty (complete): a warning that cannot be
/// read must not fail a lookup that has valid references.
fn read_ref_warnings(env: &TrackedEnv, rtxn: &heed::RoTxn<'_>, canonical: &str) -> Vec<String> {
    match env.open_database::<Str, Bytes>(rtxn, Some(SCIP_REF_WARNINGS_DB_NAME)) {
        Ok(Some(db)) => match db.get(rtxn, canonical) {
            Ok(Some(bytes)) => deserialize_keys_v1(bytes).unwrap_or_default(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

// ── Simple-name extraction ─────────────────────────────────────────

/// Extracts the last segment of a canonical SCIP symbol as a simple name.
///
/// Examples:
/// - `"csharp App . FieldDefinition#Validate()."` → `"Validate"`
/// - `"csharp SmallSolution.Library . Calculator#Add(int, int)."` → `"Add"`
/// - `"csharp . . . Namespace.TopLevel"` → `"TopLevel"`
///
/// Parameter lists are stripped BEFORE segmenting: key format 2.0 writes
/// fully-qualified parameter types (`SaveBatchAsync(int, System.Collections
/// .Generic.IReadOnlyList<global::…>)`), and splitting on `.` first would
/// land inside the parameters — the "Activity, int)" fragments that left
/// every parameterized method unresolvable by fuzzy name.
pub(crate) fn extract_simple_name(scip_symbol: &str) -> String {
    // Method/field segment: after the last '#' (types without '#' keep the
    // whole string — their name is the last '.'-segment below; a bare
    // trailing '#' falls back to the pre-# text).
    let after_hash = scip_symbol.rsplit('#').next().unwrap_or(scip_symbol);
    let base = if after_hash.trim().is_empty() {
        scip_symbol
    } else {
        after_hash
    };
    // Strip parameters FIRST, then the generic-arity marker: keys group all
    // overloads and arities under one queryable name.
    let name = base
        .split('(')
        .next()
        .unwrap_or(base)
        .split('`')
        .next()
        .unwrap_or(base);
    let cleaned = name.trim().trim_end_matches('.').trim_end_matches('#');
    cleaned
        .rsplit('.')
        .find(|s| !s.trim().is_empty())
        .unwrap_or(cleaned)
        .trim()
        .to_string()
}

// ── CSharpSymbolIndexer ───────────────────────────────────────────

/// C# adapter: locates the Roslyn helper, invokes it, parses SCIP, stores
/// references in LMDB.
pub struct CSharpSymbolIndexer {
    /// Cached detection result.
    /// `None` = not yet attempted.
    /// `Some(None)` = attempted, helper not found.
    /// `Some(Some(path))` = found at given path.
    helper_path: std::sync::Mutex<Option<Option<PathBuf>>>,
}

impl Default for CSharpSymbolIndexer {
    fn default() -> Self {
        Self::new()
    }
}

/// PIDs of live one-shot scip-csharp helper children (index, find-refs,
/// batch-find-refs). serve shutdown kills whatever is still registered: a
/// rebuild outliving its host holds the helper exe locked for minutes (the
/// deploy blocker) and keeps computing for a dead client.
static LIVE_HELPER_PIDS: std::sync::LazyLock<std::sync::Mutex<HashSet<u32>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

/// RAII registration of a spawned helper child in [`LIVE_HELPER_PIDS`];
/// dropped after the child is waited, so only genuinely in-flight PIDs
/// remain registered.
pub(crate) struct LiveHelperChild(u32);

impl LiveHelperChild {
    pub(crate) fn register(child: &std::process::Child) -> Self {
        let pid = child.id();
        if let Ok(mut set) = LIVE_HELPER_PIDS.lock() {
            set.insert(pid);
        }
        Self(pid)
    }
}

impl Drop for LiveHelperChild {
    fn drop(&mut self) {
        if let Ok(mut set) = LIVE_HELPER_PIDS.lock() {
            set.remove(&self.0);
        }
    }
}

/// Kill every still-registered helper child. Called on serve shutdown —
/// statics are not dropped at process exit, so the RAII guard alone cannot
/// cover the 'q'/Ctrl-C path. A PID whose process already exited fails the
/// kill harmlessly.
pub(crate) fn kill_live_helper_children() {
    let pids: Vec<u32> = match LIVE_HELPER_PIDS.lock() {
        Ok(mut set) => set.drain().collect(),
        Err(_) => return,
    };
    for pid in &pids {
        #[cfg(windows)]
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
        #[cfg(not(windows))]
        let _ = Command::new("kill").arg("-9").arg(pid.to_string()).output();
    }
    if !pids.is_empty() {
        tracing::info!(
            "🛑 Killed {} in-flight scip-csharp helper child(ren) on shutdown",
            pids.len()
        );
    }
}

impl CSharpSymbolIndexer {
    pub fn new() -> Self {
        Self {
            helper_path: std::sync::Mutex::new(None),
        }
    }

    /// Locate the scip-csharp helper binary.
    ///
    /// Search order (env var first so users can override):
    /// 1. `CODESEARCH_SCIP_CSHARP` env var
    /// 2. `<codesearch-exe-dir>/helpers/csharp/scip-csharp[.exe]`
    /// 3. `$PATH` lookup
    ///
    /// Results are cached — both positive (found) and negative (not found).
    pub fn detect_helper(&self) -> Option<PathBuf> {
        {
            let lock = self.helper_path.lock().unwrap();
            if let Some(cached) = lock.as_ref() {
                return cached.clone();
            }
        }

        let resolved = self.resolve_helper_path();
        {
            let mut lock = self.helper_path.lock().unwrap();
            *lock = Some(resolved.clone()); // cache both Some and None
        }
        // After the guard is released: the version probe spawns a subprocess.
        if let Some(ref path) = resolved {
            self.log_helper_version_once(path);
        }
        resolved
    }

    /// Logs the resolved helper's self-reported version exactly once per
    /// process. The mismatch class — a deployed helper older than the host
    /// that spawns it, answering with pre-gate behaviour — is otherwise
    /// invisible: the paths look right and every warning reads as a repo
    /// problem. A mismatch is a WARNING, not a refusal: an old helper still
    /// indexes, just without the current guarantees.
    fn log_helper_version_once(&self, helper: &Path) {
        static LOGGED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if LOGGED.set(()).is_err() {
            return;
        }
        let version = std::process::Command::new(helper)
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| parse_helper_version(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_else(|| "unknown".to_string());
        let host = env!("CARGO_PKG_VERSION");
        if version == host {
            tracing::info!(
                "scip-csharp helper v{} at {} (matches codesearch v{})",
                version,
                helper.display(),
                host
            );
        } else {
            tracing::warn!(
                "scip-csharp helper reports v{} at {} but codesearch is v{} — \
                 redeploy (copy-to-common.ps1) or point {} at the matching helper; \
                 behaviour may predate the current restore gate",
                version,
                helper.display(),
                host,
                HELPER_ENV_VAR
            );
        }
    }

    /// Writes the rebuild bookkeeping shared by every scope (full and
    /// incremental): key-format stamp, index warnings (written or cleared),
    /// reference coverage and the builder-version stamp. Called from
    /// `rebuild`'s meta txn, so meta always describes the newest build.
    /// `pub(crate)` so the producer test drives the exact write the pipeline
    /// runs.
    pub(crate) fn write_rebuild_meta(
        meta_db: &Database<Str, Str>,
        wtxn: &mut heed::RwTxn,
        index_warnings: &[String],
        refs_coverage: &str,
    ) -> Result<()> {
        meta_db.put(wtxn, META_REFS_COVERAGE, refs_coverage)?;
        // Unconditional: has_index refuses an index whose key-format stamp is
        // absent or stale, so a key-format change forces exactly one rebuild.
        meta_db.put(wtxn, META_KEY_FORMAT, crate::constants::SCIP_KEY_FORMAT)?;
        // Index-level completeness warnings from this run — written or
        // cleared on EVERY rebuild (full or incremental) so meta always
        // describes the newest build.
        let stored_warnings = index_warnings_stored(index_warnings);
        if stored_warnings.is_empty() {
            meta_db.delete(wtxn, META_INDEX_WARNINGS)?;
        } else {
            let json = serde_json::to_string(&stored_warnings).unwrap_or_else(|_| "[]".to_string());
            meta_db.put(wtxn, META_INDEX_WARNINGS, json.as_str())?;
        }
        // Stamp the producing build: the rebuild gate treats an index from
        // another (or an unknown) build as stale, so a deployed binary
        // rebuilds the indexes it inherited itself at startup. SCIP-only
        // stamp — the text index compares INDEX_BUILDER_VERSION instead.
        meta_db.put(
            wtxn,
            META_BUILDER_VERSION,
            crate::constants::SCIP_INDEX_BUILDER_VERSION,
        )?;
        Ok(())
    }

    fn resolve_helper_path(&self) -> Option<PathBuf> {
        // 1. Environment variable override
        if let Ok(path) = std::env::var(HELPER_ENV_VAR) {
            let p = PathBuf::from(&path);
            if p.exists() {
                if let Some(validated) = Self::validate_helper_path(&p) {
                    tracing::debug!("scip-csharp helper found via {}={}", HELPER_ENV_VAR, path);
                    return Some(validated);
                }
                tracing::warn!(
                    "{}={} does not point to a valid scip-csharp binary (filename mismatch), falling back",
                    HELPER_ENV_VAR,
                    path
                );
            } else {
                tracing::warn!(
                    "{}={} does not exist, falling back to default search",
                    HELPER_ENV_VAR,
                    path
                );
            }
        }

        // 2. Next to the codesearch binary (trusted — constructed from constant)
        if let Ok(exe) = std::env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                let bin_name = if cfg!(windows) {
                    format!("{}.exe", HELPER_BIN_NAME)
                } else {
                    HELPER_BIN_NAME.to_string()
                };
                let local_path = exe_dir
                    .join(crate::constants::HELPERS_SUBDIR)
                    .join("csharp")
                    .join(&bin_name);
                if local_path.exists() {
                    tracing::debug!("scip-csharp helper found at {}", local_path.display());
                    return Some(local_path);
                }
            }
        }

        // 3. $PATH lookup (use `which` on Unix, `where` on Windows)
        let lookup_cmd = if cfg!(windows) { "where" } else { "which" };
        if let Ok(output) = Command::new(lookup_cmd).arg(HELPER_BIN_NAME).output() {
            if output.status.success() {
                let path_str = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let p = PathBuf::from(&path_str);
                if p.exists() {
                    if let Some(validated) = Self::validate_helper_path(&p) {
                        tracing::debug!("scip-csharp helper found on PATH: {}", path_str);
                        return Some(validated);
                    }
                    tracing::warn!(
                        "PATH-resolved helper at {} has unexpected filename, skipping",
                        path_str
                    );
                }
            }
        }

        None
    }

    /// Validate that the resolved helper path points to a file whose name matches
    /// the expected `scip-csharp` binary (with platform-appropriate extension).
    ///
    /// This prevents command injection where an attacker sets the env var or
    /// manipulates PATH to point to an arbitrary executable.
    fn validate_helper_path(path: &Path) -> Option<PathBuf> {
        if !path.is_file() {
            tracing::warn!("Helper path is not a regular file: {}", path.display());
            return None;
        }

        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        // Build the expected filename for this platform
        let expected = if cfg!(windows) {
            format!("{}.exe", HELPER_BIN_NAME)
        } else {
            HELPER_BIN_NAME.to_string()
        };

        if file_name.eq_ignore_ascii_case(&expected) {
            Some(path.to_path_buf())
        } else {
            tracing::warn!(
                "Helper filename '{}' does not match expected '{}'",
                file_name,
                expected
            );
            None
        }
    }

    /// Find the solution file in a repo directory.
    fn find_solution(repo_path: &Path) -> Option<PathBuf> {
        if let Ok(entries) = std::fs::read_dir(repo_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("sln") {
                    return Some(path);
                }
            }
        }
        None
    }

    /// Find the .csproj containing a given file.
    pub fn find_csproj_for_file(repo_path: &Path, file_path: &Path) -> Option<PathBuf> {
        let mut dir = file_path.parent()?;
        loop {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("csproj")
                        && (file_path.starts_with(dir) || dir.starts_with(repo_path))
                    {
                        return Some(p);
                    }
                }
            }
            if dir == repo_path {
                break;
            }
            dir = match dir.parent() {
                Some(p) => p,
                None => break,
            };
        }
        None
    }

    /// Open the shared SCIP LMDB environment for a given repo database path.
    ///
    /// Delegates to [`crate::symbols::get_shared_scip_env`]: one environment
    /// per `db_path/scip` for the whole process, shared across concurrent
    /// queries, rebuilds and the TypeScript adapter, so overlapping users
    /// serialise on LMDB's writer mutex instead of failing the double-open
    /// guard.
    fn open_scip_env(&self, db_path: &Path) -> Result<Arc<TrackedEnv>> {
        crate::symbols::get_shared_scip_env(db_path)
    }

    // ── Helper invocation ──────────────────────────────────────────

    /// Invoke `scip-csharp index` and stream stderr to tracing.
    ///
    /// Returns the raw warning-severity stderr lines of the run — the input
    /// to [`summarize_index_warnings`]. A helper that survives MSBuild
    /// project-load failures exits 0 with a syntactically valid but
    /// semantically partial index (definitions without cross-project
    /// references); those failures land HERE so the rebuild can persist them
    /// as index-level warnings instead of them living only in the log.
    fn invoke_index_helper(
        &self,
        helper: &Path,
        solution: &Path,
        output_path: &Path,
        project_filter: Option<&Path>,
    ) -> Result<Vec<String>> {
        let solution_short = solution
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| solution.display().to_string());

        let mut cmd = Command::new(helper);
        cmd.arg("index")
            .arg("--solution")
            .arg(solution)
            .arg("--output")
            .arg(output_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(proj) = project_filter {
            cmd.arg("--filter-project").arg(proj);
        }

        tracing::info!("Running scip-csharp index: {:?}", cmd);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to execute scip-csharp at {}", helper.display()))?;
        // Deregistered when the guard drops at the end of this fn (after
        // wait), so shutdown only ever kills genuinely in-flight helpers.
        let _live_helper = LiveHelperChild::register(&child);

        let captured_warnings: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stderr_handle = child.stderr.take().map(|stderr| {
            let label = solution_short.clone();
            let sink = std::sync::Arc::clone(&captured_warnings);
            thread::spawn(move || {
                drain_pipe_to_tracing(stderr, |line| {
                    if !line.is_empty() {
                        if is_helper_warning_line(line) {
                            sink.lock().map(|mut v| v.push(line.to_string())).ok();
                        }
                        emit_helper_stderr_line("scip-csharp", &label, line);
                    }
                });
            })
        });

        let stdout_handle = child.stdout.take().map(|stdout| {
            let label = solution_short.clone();
            thread::spawn(move || {
                drain_pipe_to_tracing(stdout, |line| {
                    if !line.is_empty() {
                        tracing::debug!("[scip-csharp:{}] {}", label, line);
                    }
                });
            })
        });

        let status = child
            .wait()
            .with_context(|| format!("Failed to wait for scip-csharp at {}", helper.display()))?;

        if let Some(h) = stderr_handle {
            let _ = h.join();
        }
        if let Some(h) = stdout_handle {
            let _ = h.join();
        }

        if status.code() == Some(HELPER_RESTORE_PENDING_EXIT) {
            // The helper printed [RESTORE-PENDING] and wrote nothing: map to
            // the typed outcome, never a generic failure (issue #225).
            return Err(anyhow::Error::new(RestorePendingError));
        }

        if !status.success() {
            tracing::warn!(
                "scip-csharp exited with {} for {}",
                super::exit_status_text(&status),
                solution_short
            );
            // Don't bail — partial output is acceptable per AGENTS.md spec
        }

        // A poisoned lock still holds every line pushed before the panic —
        // recover rather than drop honesty signals on the floor.
        let raw = match captured_warnings.lock() {
            Ok(mut v) => std::mem::take(&mut *v),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        Ok(raw)
    }

    /// Invoke `scip-csharp find-refs` for a single symbol and return its
    /// references plus the completeness warnings the helper reported.
    ///
    /// Called on a `scip_ref_cache` miss for a symbol the rebuild did not
    /// precompute. Result is cached in `scip_ref_cache`.
    fn invoke_find_refs_helper(
        &self,
        helper: &Path,
        solution: &Path,
        symbol: &str,
    ) -> Result<(Vec<StoredReference>, Vec<String>)> {
        let start = std::time::Instant::now();

        let temp_dir = std::env::temp_dir().join("codesearch-scip");
        std::fs::create_dir_all(&temp_dir)?;
        // Include PID + nanoseconds to avoid collision when multiple find-refs
        // calls are in flight concurrently for different symbols on the same repo.
        let output_path = temp_dir.join(format!(
            "refs-{}-{:x}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _output_guard = TempFileGuard(output_path.clone());

        let solution_short = solution
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| solution.display().to_string());

        let mut cmd = Command::new(helper);
        cmd.arg("find-refs")
            .arg("--solution")
            .arg(solution)
            .arg("--symbol")
            .arg(symbol)
            .arg("--output")
            .arg(&output_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        tracing::info!("scip-csharp find-refs: resolving '{}'", symbol);

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "Failed to spawn scip-csharp find-refs at {}",
                helper.display()
            )
        })?;
        let _live_helper = LiveHelperChild::register(&child);

        let stderr_handle = child.stderr.take().map(|stderr| {
            let label = solution_short.clone();
            thread::spawn(move || {
                drain_pipe_to_tracing(stderr, |line| {
                    if !line.is_empty() {
                        emit_helper_stderr_line("scip-csharp find-refs", &label, line);
                    }
                });
            })
        });

        let stdout_handle = child.stdout.take().map(|stdout| {
            let label = solution_short.clone();
            thread::spawn(move || {
                drain_pipe_to_tracing(stdout, |line| {
                    if !line.is_empty() {
                        tracing::debug!("[scip-csharp find-refs:{}] {}", label, line);
                    }
                });
            })
        });

        let status = child
            .wait()
            .with_context(|| "Failed to wait for scip-csharp find-refs")?;

        if let Some(h) = stderr_handle {
            let _ = h.join();
        }
        if let Some(h) = stdout_handle {
            let _ = h.join();
        }

        if status.code() == Some(HELPER_RESTORE_PENDING_EXIT) {
            // Typed restore-pending outcome: no output file exists to read
            // and nothing may be cached from this run (issue #225).
            return Err(anyhow::Error::new(RestorePendingError));
        }

        if !status.success() {
            tracing::warn!(
                "scip-csharp find-refs exited with {} for '{}'",
                super::exit_status_text(&status),
                symbol
            );
        }

        let data = std::fs::read(&output_path).with_context(|| {
            format!(
                "Failed to read find-refs output at {}",
                output_path.display()
            )
        })?;

        let result = scip_parse::parse_find_refs_output(&data)?;

        let stored: Vec<StoredReference> = result
            .references
            .into_iter()
            .map(|r| StoredReference {
                file: r.file,
                start_line: r.start_line,
                end_line: r.end_line,
                kind: r.kind,
            })
            .collect();

        tracing::info!(
            "scip-csharp find-refs: {} references for '{}' in {}ms",
            stored.len(),
            symbol,
            start.elapsed().as_millis()
        );

        Ok((stored, result.warnings))
    }

    // ── Internal lookup helpers ────────────────────────────────────

    /// Resolve a (possibly fuzzy) symbol name to canonical SCIP key(s).
    ///
    /// Exact key wins. Otherwise the simple-name index lists candidates and
    /// the fuzzy filter narrows them: zero matches is `NotFound`, exactly
    /// one resolves, and SEVERAL come back as `KeyMatch::Ambiguous` — the
    /// caller must choose. The old behaviour silently picked the shortest
    /// candidate, which hid overloads (`Validate()` vs `Validate(string)`)
    /// from the caller and answered about the wrong symbol.
    fn resolve_name_key(&self, env: &TrackedEnv, symbol: &str) -> Result<KeyMatch> {
        let rtxn = env.read_txn()?;

        let symbols_db: Database<Str, Bytes> = match env.open_database(&rtxn, Some(SCIP_DB_NAME))? {
            Some(db) => db,
            None => return Ok(KeyMatch::NotFound),
        };

        // Exact match first
        if symbols_db.get(&rtxn, symbol)?.is_some() {
            return Ok(KeyMatch::Resolved(symbol.to_string()));
        }

        // Fuzzy via simple-name index
        let simple_names_db: Database<Str, Bytes> =
            match env.open_database(&rtxn, Some(SCIP_SIMPLE_NAMES_DB_NAME))? {
                Some(db) => db,
                None => return Ok(KeyMatch::NotFound),
            };

        let simple = extract_simple_name(symbol);
        let candidates: Vec<String> = match simple_names_db.get(&rtxn, &simple as &str)? {
            Some(b) => deserialize_keys_v1(b)?,
            None => return Ok(KeyMatch::NotFound),
        };

        let mut matches: Vec<String> = candidates
            .into_iter()
            .filter(|k| fuzzy_symbol_match(symbol, k))
            .collect();
        matches.sort();
        matches.dedup();
        Ok(match matches.len() {
            0 => KeyMatch::NotFound,
            1 => KeyMatch::Resolved(matches.pop().expect("len checked")),
            _ => KeyMatch::Ambiguous(matches),
        })
    }

    /// Inner implementation: fetch references for an EXACT (canonical) symbol key.
    ///
    /// - Returns definitions from `scip_symbols` (always present after rebuild).
    /// - Returns cached references from `scip_ref_cache` if present.
    /// - On cache miss: invokes `scip-csharp find-refs`, stores in `scip_ref_cache`.
    ///
    /// Inner implementation: fetch references for an EXACT (canonical) symbol key.
    ///
    /// Uses the shared SCIP env ([`crate::symbols::get_shared_scip_env`]); the
    /// internal write txn that caches lazy results serialises against other
    /// writers on LMDB's single-writer mutex instead of erroring the loser.
    fn find_refs_for_canonical_key(
        &self,
        db_path: &Path,
        canonical: &str,
    ) -> Result<Vec<SymbolReference>> {
        // One env for the whole function; read phase and write phase reuse it.
        let env = self.open_scip_env(db_path)?;

        let mut all_stored: Vec<StoredReference> = Vec::new();
        let cache_hit;
        let has_legacy_refs;

        // ── Read phase ─────────────────────────────────────────────
        {
            let rtxn = env.read_txn()?;

            // 1. Load definitions from scip_symbols
            if let Some(symbols_db) = env.open_database::<Str, Bytes>(&rtxn, Some(SCIP_DB_NAME))? {
                if let Some(bytes) = symbols_db.get(&rtxn, canonical)? {
                    match deserialize_refs(bytes) {
                        Ok(defs) => all_stored.extend(defs),
                        Err(e) => tracing::warn!(
                            "Failed to deserialize definitions for '{}': {}",
                            canonical,
                            e
                        ),
                    }
                }
            }

            // 2. Check reference cache
            cache_hit = if let Some(ref_cache_db) =
                env.open_database::<Str, Bytes>(&rtxn, Some(SCIP_REF_CACHE_DB_NAME))?
            {
                match ref_cache_db.get(&rtxn, canonical)? {
                    Some(cached_bytes) => match deserialize_refs(cached_bytes) {
                        Ok(cached_refs) => {
                            all_stored.extend(cached_refs);
                            true
                        }
                        Err(_) => false,
                    },
                    None => false,
                }
            } else {
                false
            };

            // Backward compat: old full-index LMDB has reference-kind entries in
            // scip_symbols. Treat those as cache hits — no helper call needed.
            has_legacy_refs = all_stored.iter().any(|r| r.kind != "definition");
        } // rtxn dropped here

        if cache_hit || has_legacy_refs {
            // A cached answer replays the warnings persisted WITH it — the
            // honesty is part of the cached fact, so a partial result can
            // never quietly pass for complete on the 2nd+ call.
            let warnings = {
                let rtxn = env.read_txn()?;
                read_ref_warnings(&env, &rtxn, canonical)
            };
            for w in &warnings {
                tracing::warn!("cached refs for '{}' may be incomplete: {}", canonical, w);
            }
            return Ok(all_stored.into_iter().map(stored_to_symbol_ref).collect());
        }

        // ── Cache miss — lazy find-refs invocation ─────────────────
        let helper = match self.detect_helper() {
            Some(h) => h,
            None => {
                tracing::debug!(
                    "scip-csharp helper not available for lazy ref resolution of '{}'",
                    canonical
                );
                return Ok(all_stored.into_iter().map(stored_to_symbol_ref).collect());
            }
        };

        // Resolve repo_path: read from LMDB meta (written during rebuild),
        // fall back to db_path.parent() for backward compat with old indexes.
        let repo_path = {
            let rtxn = env.read_txn()?;
            let meta_db: Database<Str, Str> = env
                .open_database(&rtxn, Some(SCIP_META_DB_NAME))?
                .unwrap_or_else(|| {
                    panic!("scip_meta DB should exist (created during open_scip_env)")
                });
            match meta_db.get(&rtxn, META_REPO_PATH)? {
                Some(path_str) => PathBuf::from(path_str),
                None => {
                    tracing::debug!(
                        "META_REPO_PATH not found in scip_meta, falling back to db_path.parent()"
                    );
                    db_path.parent().unwrap_or(db_path).to_path_buf()
                }
            }
        };
        let solution = match Self::find_solution(&repo_path) {
            Some(s) => s,
            None => {
                tracing::warn!(
                    "No .sln found under {} for lazy ref resolution of '{}'",
                    repo_path.display(),
                    canonical
                );
                return Ok(all_stored.into_iter().map(stored_to_symbol_ref).collect());
            }
        };

        tracing::info!(
            "scip_ref_cache miss for '{}' — invoking scip-csharp find-refs \
             (may take several minutes on large solutions; result cached after first call)",
            canonical
        );

        // Preferred path: the resident workspace pool (todo #115) — the
        // solution's Roslyn workspace stays loaded for MAX_RESIDENT repos,
        // so after the first lookup this answers in seconds instead of
        // spawning a fresh helper per call. Fallback: the one-shot spawn,
        // which keeps working when the pool cannot (spawn failure, heap-cap
        // death, eviction race) — correctness never depends on residency.
        // Both paths carry completeness warnings; a partial answer must be
        // cached AS partial, never as a complete one.
        let (lazy_refs, lazy_warnings): (Vec<StoredReference>, Vec<String>) =
            match crate::symbols::resident::WORKSPACE_POOL.find_refs(&helper, &solution, canonical)
            {
                Ok(resident) => (
                    resident
                        .references
                        .into_iter()
                        .map(|r| StoredReference {
                            file: r.file,
                            start_line: r.start_line,
                            end_line: r.end_line,
                            kind: r.kind,
                        })
                        .collect(),
                    resident.warnings,
                ),
                Err(e) => {
                    tracing::warn!(
                        "resident helper unavailable ({e:#}); falling back to one-shot \
                         find-refs for '{}'",
                        canonical
                    );
                    match self.invoke_find_refs_helper(&helper, &solution, canonical) {
                        Ok(pair) => pair,
                        Err(e) if is_restore_pending(&e) => {
                            // Restore in flight: answer from what LMDB
                            // already has (definitions only), persist the
                            // honesty warning on the per-symbol channel
                            // find_impact already reads, and cache NOTHING —
                            // the next lookup after restore finishes misses
                            // the cache, resolves properly and replaces the
                            // warning (issue #225).
                            {
                                let mut wtxn = env.write_txn()?;
                                store_ref_warnings(
                                    &env,
                                    &mut wtxn,
                                    canonical,
                                    &[RESTORE_PENDING_REF_WARNING.to_string()],
                                )?;
                                wtxn.commit()?;
                            }
                            return Ok(all_stored.into_iter().map(stored_to_symbol_ref).collect());
                        }
                        Err(e) => return Err(e),
                    }
                }
            };

        // A real resolution proves assets are fresh again: lift the
        // index-level pending flag if this lookup had set it.
        restore_pending_flag_clear(db_path);

        // ── Write phase — cache the resolved references ────────────
        // Over the LMDB key limit: answer from the live resolution and skip the
        // cache write, rather than failing a lookup that already succeeded.
        if canonical.len() <= env.max_key_size() {
            let mut wtxn = env.write_txn()?;
            let ref_cache_db: Database<Str, Bytes> =
                env.create_database(&mut wtxn, Some(SCIP_REF_CACHE_DB_NAME))?;
            let cached_bytes = serialize_refs(&lazy_refs)
                .with_context(|| format!("Failed to serialize refs for cache: {}", canonical))?;
            ref_cache_db
                .put(&mut wtxn, canonical, &cached_bytes)
                .with_context(|| put_ctx(SCIP_REF_CACHE_DB_NAME, canonical, cached_bytes.len()))?;
            // Same txn as the refs: cached-partial and its warnings are one
            // atomic fact. Empty warnings remove any stale entry.
            store_ref_warnings(&env, &mut wtxn, canonical, &lazy_warnings)?;
            wtxn.commit()?;
        }

        all_stored.extend(lazy_refs);
        Ok(all_stored.into_iter().map(stored_to_symbol_ref).collect())
    }

    /// The `refs_coverage` stamp of the last rebuild; `None` when absent or
    /// the meta is unreadable.
    pub fn refs_coverage(&self, db_path: &Path) -> Option<String> {
        let env = self.open_scip_env(db_path).ok()?;
        let rtxn = env.read_txn().ok()?;
        let meta_db: Database<Str, Str> = env
            .open_database(&rtxn, Some(SCIP_META_DB_NAME))
            .ok()
            .flatten()?;
        meta_db
            .get(&rtxn, META_REFS_COVERAGE)
            .ok()
            .flatten()
            .map(str::to_string)
    }

    /// Collect all symbol keys that do NOT already have cached references.
    ///
    /// Used by Phase 3 to skip symbols whose refs are already in the cache.
    pub fn collect_uncached_symbol_keys(&self, db_path: &Path) -> Result<Vec<String>> {
        let env = self.open_scip_env(db_path)?;
        let rtxn = env.read_txn()?;

        let symbols_db: Database<Str, Bytes> = env
            .open_database(&rtxn, Some(SCIP_DB_NAME))?
            .ok_or_else(|| anyhow::anyhow!("scip_symbols database not found"))?;

        let ref_cache_db: Option<Database<Str, Bytes>> =
            env.open_database(&rtxn, Some(SCIP_REF_CACHE_DB_NAME))?;

        let mut uncached = Vec::new();
        let iter = symbols_db.iter(&rtxn)?;
        for result in iter {
            let (key, _) = result?;
            let cached = match ref_cache_db {
                Some(db) => db.get(&rtxn, key)?.is_some(),
                None => false,
            };
            if !cached {
                uncached.push(key.to_string());
            }
        }

        Ok(uncached)
    }

    /// Pre-warm the reference cache by batch-resolving all uncached symbols.
    ///
    /// Invokes `scip-csharp batch-find-refs` with the uncached symbol keys.
    /// The helper opens the workspace once, resolves all symbols, and writes
    /// results to a temp JSON file. We then parse and cache each result.
    ///
    /// Returns the number of symbols resolved and cached.
    pub fn prewarm_ref_cache(
        &self,
        repo_path: &Path,
        db_path: &Path,
        max_symbols: usize,
    ) -> Result<PrewarmSummary> {
        let helper = match self.detect_helper() {
            Some(h) => h,
            None => {
                return Ok(PrewarmSummary {
                    total_symbols: 0,
                    resolved: 0,
                    cached: 0,
                    duration_ms: 0,
                });
            }
        };

        let solution = match Self::find_solution(repo_path) {
            Some(s) => s,
            None => {
                tracing::debug!("prewarm: no .sln found under {}", repo_path.display());
                return Ok(PrewarmSummary {
                    total_symbols: 0,
                    resolved: 0,
                    cached: 0,
                    duration_ms: 0,
                });
            }
        };

        let start = std::time::Instant::now();

        // Collect uncached symbols
        let uncached = self.collect_uncached_symbol_keys(db_path)?;
        if uncached.is_empty() {
            tracing::info!("prewarm: all symbols already cached, nothing to do");
            return Ok(PrewarmSummary {
                total_symbols: 0,
                resolved: 0,
                cached: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            });
        }

        let total_available = uncached.len();
        let symbols_to_resolve = if uncached.len() > max_symbols {
            tracing::info!(
                "prewarm: limiting to {} of {} uncached symbols",
                max_symbols,
                total_available
            );
            uncached[..max_symbols].to_vec()
        } else {
            uncached
        };

        tracing::info!(
            "prewarm: resolving {} symbols (of {} total uncached) for {}",
            symbols_to_resolve.len(),
            total_available,
            repo_path.file_name().unwrap_or_default().to_string_lossy()
        );

        // Write symbol keys to temp file for batch-find-refs
        // Use a single nonce for both temp files to avoid a race between two
        // start.elapsed() calls that produce different values.
        let nonce = start.elapsed().as_nanos();
        let temp_dir = std::env::temp_dir().join("codesearch-scip");
        std::fs::create_dir_all(&temp_dir)?;
        let symbols_file = temp_dir.join(format!(
            "symbols-{}-{:x}.txt",
            repo_path.file_name().unwrap_or_default().to_string_lossy(),
            nonce
        ));
        // Filter symbols: SCIP keys must not contain newlines (they're used as the line separator).
        // Defensive — Roslyn-derived keys are always single-line, but guard against edge cases.
        let clean_symbols: Vec<&str> = symbols_to_resolve
            .iter()
            .map(|s| s.as_str())
            .filter(|s| !s.contains('\n'))
            .collect();
        std::fs::write(&symbols_file, clean_symbols.join("\n"))?;
        // Guard ensures cleanup on all exit paths (success, early-? returns, panics).
        let _symbols_guard = TempFileGuard(symbols_file.clone());

        let output_path = temp_dir.join(format!(
            "batch-refs-{}-{:x}.json",
            std::process::id(),
            nonce
        ));
        let _output_guard = TempFileGuard(output_path.clone());

        // Invoke batch-find-refs — symbols_file and output_path are cleaned up by guards
        if let Err(e) =
            self.invoke_batch_find_refs_helper(&helper, &solution, &symbols_file, &output_path)
        {
            if is_restore_pending(&e) {
                // Prewarm is best-effort: skip this round, cache nothing.
                // The next prewarm (or any lazy lookup) retries after the
                // restore finished (issue #225).
                tracing::warn!(
                    "prewarm skipped for '{}': dotnet restore still running (restore pending)",
                    solution.display()
                );
                return Ok(PrewarmSummary {
                    total_symbols: total_available,
                    resolved: 0,
                    cached: 0,
                    duration_ms: start.elapsed().as_millis() as u64,
                });
            }
            return Err(e);
        }

        // Parse and cache results
        let cached = self.parse_and_cache_batch_refs(db_path, &output_path)?;
        // Guards drop here (or on early-? return above) and delete both temp files.

        let duration_ms = start.elapsed().as_millis() as u64;
        tracing::info!(
            "prewarm: resolved {} symbols, cached {} refs in {}ms",
            symbols_to_resolve.len(),
            cached,
            duration_ms
        );

        Ok(PrewarmSummary {
            total_symbols: total_available,
            resolved: symbols_to_resolve.len(),
            cached,
            duration_ms,
        })
    }

    /// Invoke `scip-csharp batch-find-refs` to resolve multiple symbols in one session.
    fn invoke_batch_find_refs_helper(
        &self,
        helper: &Path,
        solution: &Path,
        symbols_file: &Path,
        output_path: &Path,
    ) -> Result<()> {
        let solution_short = solution
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| solution.display().to_string());

        let mut cmd = Command::new(helper);
        cmd.arg("batch-find-refs")
            .arg("--solution")
            .arg(solution)
            .arg("--symbols-file")
            .arg(symbols_file)
            .arg("--output")
            .arg(output_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        tracing::info!(
            "scip-csharp batch-find-refs: resolving symbols from {}",
            symbols_file.display()
        );

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "Failed to spawn scip-csharp batch-find-refs at {}",
                helper.display()
            )
        })?;
        let _live_helper = LiveHelperChild::register(&child);

        let stderr_handle = child.stderr.take().map(|stderr| {
            let label = solution_short.clone();
            thread::spawn(move || {
                drain_pipe_to_tracing(stderr, |line| {
                    if !line.is_empty() {
                        emit_helper_stderr_line("scip-csharp batch-find-refs", &label, line);
                    }
                });
            })
        });

        let stdout_handle = child.stdout.take().map(|stdout| {
            let label = solution_short.clone();
            thread::spawn(move || {
                drain_pipe_to_tracing(stdout, |line| {
                    if !line.is_empty() {
                        tracing::debug!("[scip-csharp batch-find-refs:{}] {}", label, line);
                    }
                });
            })
        });

        let status = child
            .wait()
            .with_context(|| "Failed to wait for scip-csharp batch-find-refs")?;

        if let Some(h) = stderr_handle {
            let _ = h.join();
        }
        if let Some(h) = stdout_handle {
            let _ = h.join();
        }

        if status.code() == Some(HELPER_RESTORE_PENDING_EXIT) {
            // Typed restore-pending outcome: no output file exists to parse
            // and nothing may be cached from this run (issue #225).
            return Err(anyhow::Error::new(RestorePendingError));
        }

        if !status.success() {
            tracing::warn!(
                "scip-csharp batch-find-refs exited with {} for {}",
                super::exit_status_text(&status),
                solution_short
            );
            // Don't bail — partial output is acceptable
        }

        Ok(())
    }

    /// Parse batch-find-refs output and cache results in LMDB.
    ///
    /// Returns the number of symbols that had their refs cached.
    fn parse_and_cache_batch_refs(&self, db_path: &Path, output_path: &Path) -> Result<usize> {
        let content = std::fs::read_to_string(output_path).with_context(|| {
            format!(
                "Failed to read batch-find-refs output: {}",
                output_path.display()
            )
        })?;

        #[derive(serde::Deserialize)]
        struct BatchOutput {
            version: String,
            results: Vec<SymbolResult>,
        }

        #[derive(serde::Deserialize)]
        struct SymbolResult {
            symbol: String,
            references: Vec<RefEntry>,
            /// Absent in helper output from before warnings existed —
            /// default to empty so old binaries keep parsing as "complete".
            #[serde(default)]
            warnings: Vec<String>,
        }

        #[derive(serde::Deserialize)]
        struct RefEntry {
            file: String,
            #[serde(rename = "start_line")]
            start_line: u32,
            #[serde(rename = "end_line")]
            end_line: u32,
            kind: String,
        }

        let batch: BatchOutput = serde_json::from_str(&content)
            .with_context(|| "Failed to parse batch-find-refs output")?;

        // Reject outputs from unknown helper versions to avoid silently misinterpreting
        // a changed schema (same contract enforced on index and find-refs paths).
        if batch.version != scip_parse::SUPPORTED_INDEX_VERSION {
            anyhow::bail!(
                "Unsupported batch-find-refs version: '{}' (expected '{}'). \
                 Update codesearch and scip-csharp together.",
                batch.version,
                scip_parse::SUPPORTED_INDEX_VERSION
            );
        }

        let env = self.open_scip_env(db_path)?;
        let mut wtxn = env.write_txn()?;
        let ref_cache_db: Database<Str, Bytes> =
            env.create_database(&mut wtxn, Some(SCIP_REF_CACHE_DB_NAME))?;

        let mut cached_count = 0usize;
        for result in &batch.results {
            // Filter: only cache "reference" kind (defensive against definitions)
            let refs: Vec<StoredReference> = result
                .references
                .iter()
                .filter(|r| r.kind == "reference")
                .map(|r| StoredReference {
                    file: PathBuf::from(&r.file),
                    start_line: r.start_line,
                    end_line: r.end_line,
                    kind: r.kind.clone(),
                })
                .collect();

            // Over the LMDB key limit: skip rather than fail the batch, same
            // rule as the rebuild's write loop.
            if result.symbol.len() > env.max_key_size() {
                continue;
            }
            // Cache even if empty — symbols with 0 references must be marked as
            // "resolved" so collect_uncached_symbol_keys() won't retry them forever.
            let bytes = serialize_refs(&refs)
                .with_context(|| format!("Failed to serialize batch refs for {}", result.symbol))?;
            ref_cache_db
                .put(&mut wtxn, result.symbol.as_str(), &bytes)
                .with_context(|| put_ctx(SCIP_REF_CACHE_DB_NAME, &result.symbol, bytes.len()))?;
            // Same txn as the refs: cached-partial and its warnings are one
            // atomic fact. Empty warnings remove any stale entry.
            store_ref_warnings(&env, &mut wtxn, &result.symbol, &result.warnings)?;
            cached_count += 1;
        }

        wtxn.commit()?;
        Ok(cached_count)
    }
}

/// Convert a `StoredReference` to the public `SymbolReference` type.
fn stored_to_symbol_ref(r: StoredReference) -> SymbolReference {
    SymbolReference {
        file: r.file,
        start_line: r.start_line,
        end_line: r.end_line,
        kind: r.kind,
    }
}

impl SymbolIndexer for CSharpSymbolIndexer {
    fn language(&self) -> &str {
        "csharp"
    }

    fn rebuild(
        &self,
        repo_path: &Path,
        db_path: &Path,
        scope: RebuildScope,
    ) -> Result<RebuildSummary> {
        let helper = self.detect_helper().ok_or_else(|| {
            anyhow::anyhow!(
                "scip-csharp helper not found. Install the -with-csharp release variant \
                 or set {} to the helper binary path.",
                HELPER_ENV_VAR
            )
        })?;

        let start = std::time::Instant::now();

        // Determine solution/project from scope.
        // `is_incremental` true → RebuildScope::Files → merge, not replace.
        // Also extract `deleted_for_cleanup`: paths absent from the new index that must
        // still be purged from LMDB (files deleted on disk since last rebuild).
        let (solution, project, is_incremental, deleted_for_cleanup) = match scope {
            RebuildScope::Full => {
                let sln = Self::find_solution(repo_path).ok_or_else(|| {
                    anyhow::anyhow!("No .sln file found in {}", repo_path.display())
                })?;
                (sln, None, false, vec![])
            }
            RebuildScope::Project(csproj) => {
                let sln = Self::find_solution(repo_path).unwrap_or_else(|| csproj.clone());
                (sln, Some(csproj), false, vec![])
            }
            RebuildScope::Files { changed, deleted } => {
                if let Some(first_file) = changed.first() {
                    let csproj = Self::find_csproj_for_file(repo_path, first_file)
                        .unwrap_or_else(|| first_file.clone());
                    let sln = Self::find_solution(repo_path).unwrap_or_else(|| csproj.clone());
                    // Normalise deleted paths to forward-slash strings for LMDB key comparison.
                    let deleted_norm: Vec<String> = deleted
                        .iter()
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                        .collect();
                    (sln, Some(csproj), true, deleted_norm) // ← incremental merge
                } else {
                    bail!("RebuildScope::Files has no changed files");
                }
            }
        };

        // Create temp file for SCIP output
        let temp_dir = std::env::temp_dir().join("codesearch-scip");
        std::fs::create_dir_all(&temp_dir)?;
        let output_path = temp_dir.join(format!(
            "index-{}-{:x}.json",
            repo_path.file_name().unwrap_or_default().to_string_lossy(),
            start.elapsed().as_nanos()
        ));
        let _output_guard = TempFileGuard(output_path.clone());

        // Invoke helper with stderr streaming; warning-severity lines become
        // the index-level completeness warnings persisted below.
        //
        // ── Restore gate (issue #225) ─────────────────────────────────
        // A RESTORE-PENDING exit means a dotnet restore for this solution is
        // still running and the helper deliberately produced nothing.
        // Indexing anyway would store a degraded, CS0246-noisy index into
        // LMDB while the previous good index could have stayed live — so
        // retry the SAME rebuild scope with a bounded backoff until the
        // helper stops reporting pending, and on exhaustion give up with
        // LMDB untouched: every LMDB write below sits after this loop.
        let mut attempts: u32 = 0;
        let mut marked_pending = false;
        let raw_index_warnings = loop {
            match self.invoke_index_helper(&helper, &solution, &output_path, project.as_deref()) {
                Ok(raw) => {
                    if marked_pending {
                        restore_pending_flag_clear(db_path);
                    }
                    break raw;
                }
                Err(e) if is_restore_pending(&e) => {
                    attempts += 1;
                    // Idempotent set on EVERY pending exit: a later restore
                    // can start while this loop is still waiting (an earlier
                    // one resolved and cleared the flag via a lazy find-refs)
                    // and the warning must survive that.
                    restore_pending_flag_set(db_path);
                    marked_pending = true;
                    if attempts >= RESTORE_PENDING_RETRY_MAX_ATTEMPTS {
                        restore_pending_flag_clear(db_path);
                        let backoff = (1..RESTORE_PENDING_RETRY_MAX_ATTEMPTS)
                            .map(|k| format!("{}s", restore_pending_retry_delay_secs(k)))
                            .collect::<Vec<_>>()
                            .join(" + ");
                        bail!(RestoreExhaustedError {
                            attempts: RESTORE_PENDING_RETRY_MAX_ATTEMPTS,
                            backoff,
                        });
                    }
                    let delay_secs = restore_pending_retry_delay_secs(attempts);
                    tracing::warn!(
                        "restore pending for '{}' — retrying the same rebuild scope in {}s \
                         (attempt {}/{})",
                        solution.display(),
                        delay_secs,
                        attempts,
                        RESTORE_PENDING_RETRY_MAX_ATTEMPTS
                    );
                    if !sleep_unless_shutdown(std::time::Duration::from_secs(delay_secs)) {
                        restore_pending_flag_clear(db_path);
                        bail!(
                            "rebuild cancelled while waiting for dotnet restore — shutdown \
                             requested, previous index stays live"
                        );
                    }
                }
                Err(e) => {
                    // A non-pending failure ends the retry loop: the flag
                    // describes THIS loop's wait, so it must not outlive it
                    // — otherwise find_impact would warn "restore in
                    // progress" long after the loop gave up for another
                    // reason (helper crash, solution deleted mid-wait).
                    if marked_pending {
                        restore_pending_flag_clear(db_path);
                    }
                    return Err(e);
                }
            }
        };
        let index_warnings = summarize_index_warnings(&raw_index_warnings, INDEX_WARNINGS_CAP);

        // Parse the JSON output
        let index_data = std::fs::read(&output_path)
            .with_context(|| format!("Failed to read symbol index at {}", output_path.display()))?;

        let scip_parse::ParsedIndex {
            definitions: index,
            references,
        } = scip_parse::parse_json_index(&index_data)?;
        // Only a full-scope run resolves references solution-wide; a filtered
        // run's list would cache project-local answers as the whole truth.
        let precomputed_refs = references.filter(|_| project.is_none());
        let refs_coverage = refs_coverage_for(
            precomputed_refs.is_some(),
            index_warnings_stored(&index_warnings).is_empty(),
        );

        // Open LMDB (all named DBs pre-created by open_scip_env)
        let env = self.open_scip_env(db_path)?;
        let mut wtxn = env.write_txn()?;

        let symbols_db: Database<Str, Bytes> =
            env.create_database(&mut wtxn, Some(SCIP_DB_NAME))?;
        let meta_db: Database<Str, Str> =
            env.create_database(&mut wtxn, Some(SCIP_META_DB_NAME))?;
        let positions_db: Database<Str, Bytes> =
            env.create_database(&mut wtxn, Some(SCIP_POSITION_DB_NAME))?;
        let simple_names_db: Database<Str, Bytes> =
            env.create_database(&mut wtxn, Some(SCIP_SIMPLE_NAMES_DB_NAME))?;
        let ref_cache_db: Database<Str, Bytes> =
            env.create_database(&mut wtxn, Some(SCIP_REF_CACHE_DB_NAME))?;
        let ref_warnings_db: Database<Str, Bytes> =
            env.create_database(&mut wtxn, Some(SCIP_REF_WARNINGS_DB_NAME))?;

        // Collect affected files (non-empty only for incremental/Files scope).
        // Declared outside the if/else so the write loop below can also reference it.
        //
        // For incremental rebuilds we also union in `deleted_for_cleanup`: files
        // that were deleted since the last rebuild are not present in the new index
        // output, so they would be silently skipped otherwise, leaving stale
        // `scip_positions`/`scip_symbols` entries pointing at a non-existent file.
        let affected_files: HashSet<String> = if is_incremental {
            let mut files: HashSet<String> = index
                .values()
                .flat_map(|refs| {
                    refs.iter()
                        .filter(|r| r.kind == "definition")
                        .map(|r| r.file.to_string_lossy().replace('\\', "/"))
                })
                .collect();
            // Explicitly include deleted paths so their LMDB entries are cleaned up.
            files.extend(deleted_for_cleanup);
            files
        } else {
            HashSet::new()
        };

        if !is_incremental {
            // Full rebuild: wipe everything and start fresh.
            symbols_db.clear(&mut wtxn)?;
            positions_db.clear(&mut wtxn)?;
            simple_names_db.clear(&mut wtxn)?;
            ref_cache_db.clear(&mut wtxn)?;
        } else {
            // ── Incremental merge (Opt 3) ──────────────────────────
            tracing::debug!(
                "Incremental rebuild: {} affected file(s): {:?}",
                affected_files.len(),
                affected_files
            );

            // A cache HIT short-circuits find_refs entirely (no helper call,
            // no fresh resolution — see find_refs_for_canonical_key), so an
            // entry cached before this change would keep replaying its old
            // answer forever even with the resident workspace now evicted
            // fresh below. Which existing symbols gained/lost a reference
            // FROM inside the affected files is exactly what Roslyn is
            // needed to answer — not knowable from the file list alone — so
            // there is no cheaper correct scope than the whole cache. It is
            // a pure derived artifact (rebuilt lazily, one helper call per
            // symbol on next lookup): safe to drop, unlike scip_symbols.
            ref_cache_db.clear(&mut wtxn)?;

            // Step 1: Collect stale symbol keys from the position index (reverse map
            // file:line → [symbol_keys]). This tells us exactly which scip_symbols
            // entries to inspect for affected-file definitions.
            let mut stale_symbol_keys: HashSet<String> = HashSet::new();
            let mut pos_keys_to_delete: Vec<String> = Vec::new();
            {
                let pos_iter = positions_db.iter(&wtxn)?;
                for result in pos_iter {
                    let (key, val) = result?;
                    let file_part = key.split(':').next().unwrap_or(""); // "<file>:<line>"
                    if affected_files.contains(file_part) {
                        pos_keys_to_delete.push(key.to_string());
                        if let Ok(sym_keys) = deserialize_keys_v1(val) {
                            stale_symbol_keys.extend(sym_keys);
                        }
                    }
                }
            }

            // Step 2: Delete stale position entries for affected files.
            for key in &pos_keys_to_delete {
                positions_db.delete(&mut wtxn, key.as_str())?;
            }

            // Step 3: Clean up scip_symbols for symbols NOT appearing in the new index.
            //
            // For symbols that DO appear in the new index, the write loop below
            // merges old (non-affected) + new definitions — handling partial classes.
            // For symbols that no longer exist (e.g. deleted/renamed):
            //   - Keep entries that still have definitions in non-affected files.
            //   - Delete entries where all definitions were in affected files.
            let mut purge_count = 0usize;
            for key in &stale_symbol_keys {
                if index.contains_key(key.as_str()) {
                    continue; // handled in write loop below
                }
                if let Some(bytes) = symbols_db.get(&wtxn, key.as_str())? {
                    if let Ok(existing) = deserialize_refs(bytes) {
                        let survivors: Vec<StoredReference> = existing
                            .into_iter()
                            .filter(|r| {
                                r.kind == "definition"
                                    && !affected_files
                                        .contains(&r.file.to_string_lossy().replace('\\', "/"))
                            })
                            .collect();
                        if survivors.is_empty() {
                            symbols_db.delete(&mut wtxn, key.as_str())?;
                            purge_count += 1;
                        } else {
                            // Partial class: keep definitions from non-affected files.
                            let b = serialize_refs(&survivors).with_context(|| {
                                format!("Failed to re-serialize survivors for {}", key)
                            })?;
                            symbols_db
                                .put(&mut wtxn, key.as_str(), &b)
                                .with_context(|| put_ctx(SCIP_DB_NAME, key, b.len()))?;
                        }
                    }
                }
            }

            tracing::debug!(
                "Incremental: removed {} position entries, purged {} fully-deleted symbols",
                pos_keys_to_delete.len(),
                purge_count
            );

            // The old selective invalidation here (definition-site + a
            // reference-site scan of cached entries) only ever purged a
            // symbol whose EXISTING cached reference list already pointed
            // at an affected file. It could never catch the opposite case —
            // an unrelated, already-cached symbol gaining a brand-new
            // reference FROM the changed file — because that requires
            // knowing what the changed file's new content refers to, not
            // what the old cache already recorded. `ref_cache_db.clear()`
            // above (unconditional, whole-table) closes that gap and
            // strictly subsumes this scheme, so the selective passes are
            // gone rather than left as dead code beside it.

            // symbols_db and simple_names_db are merged below, not cleared here.
        }
        // Per-symbol warnings describe ref_cache entries, which both branches
        // dropped; a surviving one would replay against a precomputed entry.
        ref_warnings_db.clear(&mut wtxn)?;

        // ── Write symbol entries (definition occurrences) ──────────
        let mut total_defs = 0usize;
        let mut total_symbols = 0usize;

        let max_key = env.max_key_size();
        let mut oversized_keys = 0usize;

        for (symbol_name, references) in index.iter() {
            // One pathological signature must never fail the whole rebuild:
            // LMDB refuses a key above the page-derived limit, and that error
            // used to read as storage-format corruption and wipe the index.
            if symbol_name.len() > max_key {
                oversized_keys += 1;
                if oversized_keys == 1 {
                    tracing::warn!(
                        "⚠️ Skipping symbol(s) whose SCIP key exceeds this LMDB build's {}-byte \
                         key limit — first: {} byte(s), {:.160}",
                        max_key,
                        symbol_name.len(),
                        symbol_name
                    );
                }
                continue;
            }
            let new_stored: Vec<StoredReference> = references
                .iter()
                .map(|r| StoredReference {
                    file: r.file.clone(),
                    start_line: r.start_line,
                    end_line: r.end_line,
                    kind: r.kind.clone(),
                })
                .collect();

            // For incremental merges: preserve definition entries from non-affected
            // files (supports C# partial classes spanning two projects).
            let stored = if is_incremental {
                if let Some(bytes) = symbols_db.get(&wtxn, symbol_name.as_str())? {
                    if let Ok(existing) = deserialize_refs(bytes) {
                        let mut merged: Vec<StoredReference> = existing
                            .into_iter()
                            .filter(|r| {
                                r.kind == "definition"
                                    && !affected_files
                                        .contains(&r.file.to_string_lossy().replace('\\', "/"))
                            })
                            .collect();
                        merged.extend(new_stored);
                        merged
                    } else {
                        new_stored
                    }
                } else {
                    new_stored
                }
            } else {
                new_stored
            };

            let value_bytes = serialize_refs(&stored)
                .with_context(|| format!("Failed to serialize definitions for {}", symbol_name))?;

            symbols_db
                .put(&mut wtxn, symbol_name.as_str(), &value_bytes)
                .with_context(|| put_ctx(SCIP_DB_NAME, symbol_name, value_bytes.len()))?;
            total_defs += stored.len();
            total_symbols += 1;
        }

        let mut cached_refs = 0usize;
        if let Some(refs) = &precomputed_refs {
            let complete = refs_coverage == crate::constants::SCIP_REFS_COVERAGE_COMPLETE;
            for symbol_name in index.keys().filter(|k| k.len() <= max_key) {
                let stored: Vec<StoredReference> = refs
                    .get(symbol_name)
                    .into_iter()
                    .flatten()
                    .filter(|r| r.kind != "definition")
                    .map(|r| StoredReference {
                        file: r.file.clone(),
                        start_line: r.start_line,
                        end_line: r.end_line,
                        kind: r.kind.clone(),
                    })
                    .collect();
                if stored.is_empty() && !complete {
                    continue;
                }
                let bytes = serialize_refs(&stored)
                    .with_context(|| format!("Failed to serialize refs for {}", symbol_name))?;
                ref_cache_db
                    .put(&mut wtxn, symbol_name.as_str(), &bytes)
                    .with_context(|| put_ctx(SCIP_REF_CACHE_DB_NAME, symbol_name, bytes.len()))?;
                cached_refs += stored.len();
            }
        }

        // ── Build position index ───────────────────────────────────
        // scip_positions: "<file>:<line>" → [symbol_keys]
        // Maps each definition occurrence to the symbols defined at that position.
        // For incremental rebuilds, old position entries for affected files were
        // already deleted above; here we only write new ones.
        if oversized_keys > 0 {
            tracing::warn!(
                "⚠️ {} symbol(s) skipped: SCIP key over the {}-byte LMDB limit. \
                 find_impact cannot resolve those; every other symbol is indexed.",
                oversized_keys,
                max_key
            );
        }

        let mut positions: HashMap<String, Vec<String>> = HashMap::new();
        for (symbol_name, references) in index.iter() {
            // Skipped above: a position entry pointing at an unwritten symbol
            // would only produce lookup misses.
            if symbol_name.len() > max_key {
                continue;
            }
            for r in references.iter().filter(|r| r.kind == "definition") {
                let pos_key = format!(
                    "{}:{}",
                    r.file.to_string_lossy().replace('\\', "/"),
                    r.start_line
                );
                positions
                    .entry(pos_key)
                    .or_default()
                    .push(symbol_name.clone());
            }
        }

        for (key, keys) in &positions {
            let bytes = serialize_keys_v1(keys)
                .with_context(|| format!("Failed to serialize position key: {}", key))?;
            positions_db
                .put(&mut wtxn, key.as_str(), &bytes)
                .with_context(|| put_ctx(SCIP_POSITION_DB_NAME, key, bytes.len()))?;
        }

        tracing::debug!(
            "scip-csharp position index: {} new entries",
            positions.len()
        );

        // ── Build simple-name index ────────────────────────────────
        // For incremental rebuilds: rebuild from ALL current scip_symbols entries
        // (existing + newly written) so that simple-name lookups stay consistent.
        // For full rebuilds: the DB was cleared, so we only have new entries.
        simple_names_db.clear(&mut wtxn)?;

        let mut all_simple_names: HashMap<String, Vec<String>> = HashMap::new();

        // Scan all scip_symbols (includes both existing and newly written entries)
        {
            let sym_iter = symbols_db.iter(&wtxn)?;
            for result in sym_iter {
                let (key, _) = result?;
                let simple = extract_simple_name(key);
                if !simple.is_empty() {
                    all_simple_names
                        .entry(simple)
                        .or_default()
                        .push(key.to_string());
                }
            }
        }

        for (key, keys) in &all_simple_names {
            let bytes = serialize_keys_v1(keys)
                .with_context(|| format!("Failed to serialize simple name key: {}", key))?;
            simple_names_db
                .put(&mut wtxn, key.as_str(), &bytes)
                .with_context(|| put_ctx(SCIP_SIMPLE_NAMES_DB_NAME, key, bytes.len()))?;
        }

        tracing::debug!(
            "scip-csharp simple-name index: {} entries",
            all_simple_names.len()
        );

        // Write metadata.
        // For incremental rebuilds `total_symbols` only counts the merged project —
        // use the full simple-name cardinality (= unique symbol count) instead.
        let reported_symbol_count = if is_incremental {
            all_simple_names.values().map(|v| v.len()).sum::<usize>()
        } else {
            total_symbols
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        meta_db.put(&mut wtxn, META_REBUILD_TS, now.to_string().as_str())?;
        meta_db.put(
            &mut wtxn,
            META_SYMBOL_COUNT,
            reported_symbol_count.to_string().as_str(),
        )?;
        meta_db.put(
            &mut wtxn,
            META_REPO_PATH,
            repo_path.to_string_lossy().as_ref(),
        )?;
        // Fingerprint the index: which HEAD it was built for. Skipped when
        // git is unreadable (better absent than a wrong claim); an older
        // value from a previous build may then survive, which stays
        // approximately right for the usual same-branch rebuild.
        if let Some(sha) = super::current_git_head(repo_path) {
            meta_db.put(&mut wtxn, META_HEAD_SHA, sha.as_str())?;
        }
        Self::write_rebuild_meta(&meta_db, &mut wtxn, &index_warnings, refs_coverage)?;

        wtxn.commit()?;

        // A rebuild changes on-disk source; a resident Roslyn workspace for
        // this solution (if any) was loaded before that change and would
        // silently keep answering find_refs from stale content otherwise —
        // this pool has no other tie to repo state. Evict unconditionally
        // (full or incremental): the next find_refs respawns fresh. Both
        // branches above also clear `ref_cache_db`, so a cache hit can only
        // replay an answer this rebuild wrote.
        crate::symbols::resident::WORKSPACE_POOL.evict(&solution);

        let duration_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            "scip-csharp rebuild complete: {} symbols, {} definition entries, {} precomputed \
             references (coverage={}) in {}ms (incremental={})",
            total_symbols,
            total_defs,
            cached_refs,
            refs_coverage,
            duration_ms,
            is_incremental
        );

        Ok(RebuildSummary {
            symbols_indexed: total_symbols,
            references_stored: total_defs + cached_refs,
            duration_ms,
            index_warnings: index_warnings_stored(&index_warnings),
        })
    }

    fn resolve_query(&self, db_path: &Path, query: &ImpactQuery) -> Result<KeyMatch> {
        match query {
            ImpactQuery::ExactKey(key) => {
                // Explicit selection: presence check only, no fuzzy
                // fallback. A key that is not in the index is NotFound —
                // the handler turns that into a loud failure, never a
                // guess at a near-miss symbol.
                let env = self.open_scip_env(db_path)?;
                let rtxn = env.read_txn()?;
                let present = match env.open_database::<Str, Bytes>(&rtxn, Some(SCIP_DB_NAME))? {
                    Some(db) => db.get(&rtxn, key as &str)?.is_some(),
                    None => false,
                };
                Ok(if present {
                    KeyMatch::Resolved(key.clone())
                } else {
                    KeyMatch::NotFound
                })
            }
            ImpactQuery::Name(name) => {
                let env = self.open_scip_env(db_path)?;
                self.resolve_name_key(&env, name)
            }
            ImpactQuery::Position { file, line } => {
                let env = self.open_scip_env(db_path)?;
                let rtxn = env.read_txn()?;

                let positions_db: Database<Str, Bytes> = env
                    .open_database(&rtxn, Some(SCIP_POSITION_DB_NAME))?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Position index not found. Run a rebuild first.")
                    })?;

                // Normalize file path to forward-slash (Windows compat)
                let pos_key = format!("{}:{}", file.to_string_lossy().replace('\\', "/"), line);

                let mut candidates: Vec<String> = match positions_db.get(&rtxn, &pos_key as &str)? {
                    Some(b) => deserialize_keys_v1(b)?,
                    None => return Ok(KeyMatch::NotFound),
                };
                candidates.sort();
                candidates.dedup();

                // Several symbols on one line (overloads, partial spans)
                // are ambiguity, not a licence to pick the shortest.
                Ok(match candidates.len() {
                    0 => KeyMatch::NotFound,
                    1 => KeyMatch::Resolved(candidates.pop().expect("len checked")),
                    _ => KeyMatch::Ambiguous(candidates),
                })
            }
        }
    }

    fn find_references_for_key(
        &self,
        db_path: &Path,
        canonical_key: &str,
    ) -> Result<Vec<SymbolReference>> {
        self.find_refs_for_canonical_key(db_path, canonical_key)
    }

    fn lookup_warnings(&self, db_path: &Path, canonical: &str) -> Vec<String> {
        // Plain LMDB read — never a helper invocation, so the find_impact
        // handler can call it after a lookup without risking minutes of work.
        let env = match self.open_scip_env(db_path) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        let rtxn = match env.read_txn() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        read_ref_warnings(&env, &rtxn, canonical)
    }

    fn restore_pending(&self, db_path: &Path) -> bool {
        match RESTORE_PENDING_SOLUTIONS.lock() {
            Ok(set) => set.contains(db_path),
            Err(poisoned) => poisoned.into_inner().contains(db_path),
        }
    }

    fn index_age(&self, db_path: &Path) -> u64 {
        let env = match self.open_scip_env(db_path) {
            Ok(e) => e,
            Err(_) => return u64::MAX,
        };
        let rtxn = match env.read_txn() {
            Ok(t) => t,
            Err(_) => return u64::MAX,
        };

        let meta_db: Database<Str, Str> = match env.open_database(&rtxn, Some(SCIP_META_DB_NAME)) {
            Ok(Some(db)) => db,
            _ => return u64::MAX,
        };

        let ts_str: &str = match meta_db.get(&rtxn, META_REBUILD_TS) {
            Ok(Some(s)) => s,
            _ => return u64::MAX,
        };

        let stored_ts: u64 = match ts_str.parse() {
            Ok(v) => v,
            Err(_) => return u64::MAX,
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        now.saturating_sub(stored_ts)
    }

    fn index_head_sha(&self, db_path: &Path) -> Option<String> {
        let env = self.open_scip_env(db_path).ok()?;
        let rtxn = env.read_txn().ok()?;
        let meta_db: Database<Str, Str> = env
            .open_database(&rtxn, Some(SCIP_META_DB_NAME))
            .ok()
            .flatten()?;
        let sha = meta_db.get(&rtxn, META_HEAD_SHA).ok().flatten()?;
        let sha = sha.trim().to_string();
        (!sha.is_empty()).then_some(sha)
    }

    /// Index-level completeness warnings from the last rebuild — the
    /// workspace/MSBuild failures the helper survived. Non-empty means
    /// reference lists from this index may be incomplete even when a
    /// lookup succeeds (observed live: a worktree whose NuGet restore was
    /// broken indexed every definition but zero cross-project references,
    /// and every find_impact answer passed as clean). Plain LMDB read;
    /// unreadable or absent metadata reads as clean (never blocks an answer).
    fn index_warnings(&self, db_path: &Path) -> Vec<String> {
        let env = match self.open_scip_env(db_path) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        let rtxn = match env.read_txn() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let meta_db: Database<Str, Str> = match env.open_database(&rtxn, Some(SCIP_META_DB_NAME)) {
            Ok(Some(db)) => db,
            _ => return Vec::new(),
        };
        let json = match meta_db.get(&rtxn, META_INDEX_WARNINGS) {
            Ok(Some(s)) => s,
            _ => return Vec::new(),
        };
        serde_json::from_str(json).unwrap_or_default()
    }

    /// Which codesearch build produced this index — `None` when the stamp is
    /// absent (pre-stamp index) or the meta is unreadable. The rebuild gate
    /// compares against [`crate::constants::SCIP_INDEX_BUILDER_VERSION`] and
    /// rebuilds on mismatch: this binary never produced that index.
    fn index_builder_version(&self, db_path: &Path) -> Option<String> {
        let env = match self.open_scip_env(db_path) {
            Ok(e) => e,
            Err(_) => return None,
        };
        let rtxn = match env.read_txn() {
            Ok(t) => t,
            Err(_) => return None,
        };
        let meta_db: Database<Str, Str> = match env.open_database(&rtxn, Some(SCIP_META_DB_NAME)) {
            Ok(Some(db)) => db,
            _ => return None,
        };
        meta_db
            .get(&rtxn, META_BUILDER_VERSION)
            .ok()
            .flatten()
            .map(|s| s.to_string())
    }

    /// Same freshness check the rebuild gate applies (mod.rs: an absent or
    /// foreign builder stamp reads as stale for C#, unlike adapters that do
    /// not stamp at all — hence the override, not the trait default).
    /// A missing `refs_coverage` stamp is stale too: such an index predates
    /// precomputed references, and only a rebuild produces them.
    fn index_matches_builder(&self, db_path: &Path) -> bool {
        self.index_builder_version(db_path).as_deref()
            == Some(crate::constants::SCIP_INDEX_BUILDER_VERSION)
            && matches!(
                self.refs_coverage(db_path).as_deref(),
                Some(
                    crate::constants::SCIP_REFS_COVERAGE_COMPLETE
                        | crate::constants::SCIP_REFS_COVERAGE_PARTIAL
                        | crate::constants::SCIP_REFS_COVERAGE_LAZY
                )
            )
    }

    /// Whether a SCIP index exists AND was built with the current key format.
    /// An index that is fresh by timestamp but stamped with another (or no)
    /// key-format generation would serve old-shaped canonical keys as truth,
    /// so it reports as absent and the caller rebuilds.
    fn has_index(&self, db_path: &Path) -> bool {
        let scip_dir = db_path.join("scip");
        if !scip_dir.exists() {
            return false;
        }
        // Quick check: if index_age is finite, the index exists
        if self.index_age(db_path) == u64::MAX {
            return false;
        }
        // Same env/txn/open pattern as `index_head_sha` above.
        let env = match self.open_scip_env(db_path) {
            Ok(e) => e,
            Err(_) => return false,
        };
        let rtxn = match env.read_txn() {
            Ok(t) => t,
            Err(_) => return false,
        };
        let meta_db: Option<Database<Str, Str>> = env
            .open_database(&rtxn, Some(SCIP_META_DB_NAME))
            .ok()
            .flatten();
        let stored = meta_db
            .and_then(|db| db.get(&rtxn, META_KEY_FORMAT).ok().flatten())
            .map(|s| s.trim().to_string());
        stored.as_deref() == Some(crate::constants::SCIP_KEY_FORMAT)
    }

    fn is_available(&self) -> bool {
        self.detect_helper().is_some()
    }

    /// C# adapter is only applicable when a top-level `.sln` file exists.
    ///
    /// Mirrors `ServeState::has_solution_file()` (the phase-2 gate) and the
    /// Full-scope precondition in `rebuild()`. Without this, callers that
    /// invoke `rebuild()` on non-C# repos (e.g. POST /reindex?symbols=true on
    /// a Rust repo) would surface a misleading "No .sln file found" error
    /// and flip the TUI C# indicator red.
    fn applies_to(&self, repo_path: &Path) -> bool {
        Self::find_solution(repo_path).is_some()
    }

    fn applicability_hint(&self) -> &'static str {
        "the scip-csharp adapter requires a .sln file in the repository root"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Fuzzy matching heuristic for symbol names.
///
/// Handles common patterns:
/// - `FieldDefinition.Validate` → `csharp . . . FieldDefinition#Validate().`
/// - `Validate` → any symbol ending with `#Validate().`
fn fuzzy_symbol_match(query: &str, candidate: &str) -> bool {
    let query_parts: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .collect();

    if query_parts.is_empty() {
        return false;
    }

    // All parts of the query must appear in the candidate
    query_parts.iter().all(|part| candidate.contains(part))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_refs_includes_version_byte() {
        let refs = vec![StoredReference {
            file: PathBuf::from("a.cs"),
            start_line: 1,
            end_line: 1,
            kind: "definition".into(),
        }];
        let bytes = serialize_refs(&refs).unwrap();
        assert_eq!(bytes[0], STORED_REFERENCE_SCHEMA_VERSION);
        // Verify the rest is valid bincode
        let decoded: Vec<StoredReference> = bincode::deserialize(&bytes[1..]).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].kind, "definition");
    }

    #[test]
    fn test_deserialize_refs_rejects_unknown_version() {
        let bytes = vec![99u8, 0, 0, 0];
        let err = deserialize_refs(&bytes).unwrap_err();
        assert!(
            err.to_string().contains("Unsupported"),
            "expected 'Unsupported' in error, got: {}",
            err
        );
    }

    #[test]
    fn test_deserialize_refs_rejects_empty() {
        let err = deserialize_refs(&[]).unwrap_err();
        assert!(
            err.to_string().contains("Empty"),
            "expected 'Empty' in error, got: {}",
            err
        );
    }

    #[test]
    fn test_extract_simple_name() {
        assert_eq!(
            extract_simple_name("csharp App . FieldDefinition#Validate()."),
            "Validate"
        );
        assert_eq!(
            extract_simple_name("csharp Lib . Calculator#Add(int, int)."),
            "Add"
        );
        // Type-level key ends with '#' (no member suffix)
        assert_eq!(extract_simple_name("csharp App . MyService#"), "MyService");
        // Namespace-qualified type (no '#' in SCIP key)
        assert_eq!(
            extract_simple_name("csharp . . Namespace.TopLevel#"),
            "TopLevel"
        );
        // Empty input
        assert_eq!(extract_simple_name(""), "");
    }

    #[test]
    fn test_fuzzy_symbol_match() {
        assert!(fuzzy_symbol_match(
            "FieldDefinition.Validate",
            "csharp App . FieldDefinition#Validate()."
        ));
        assert!(fuzzy_symbol_match(
            "Validate",
            "csharp App . FieldDefinition#Validate()."
        ));
        assert!(!fuzzy_symbol_match(
            "UnrelatedName",
            "csharp App . FieldDefinition#Validate()."
        ));
    }

    /// C# SCIP keys carry fully qualified parameter types and reach 900+ bytes
    /// (observed: 912). LMDB's stock 511-byte limit rejected those with
    /// MDB_BAD_VALSIZE, which failed the whole rebuild and read as storage
    /// corruption. This pins heed's `longer-keys` feature in Cargo.toml: drop
    /// it and the incident returns.
    #[test]
    fn the_lmdb_key_limit_is_large_enough_for_fully_qualified_csharp_symbols() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = crate::symbols::get_shared_scip_env(&dir.path().join("db")).unwrap();
        let max_key = env.max_key_size();
        assert!(
            max_key >= 1024,
            "expected a page-derived key limit (heed feature `longer-keys`), got {max_key} bytes"
        );

        let long_key = "a".repeat(1000);
        let mut wtxn = env.write_txn().unwrap();
        let db: Database<Str, Bytes> = env
            .open_database(&wtxn, Some(SCIP_DB_NAME))
            .unwrap()
            .unwrap();
        db.put(&mut wtxn, long_key.as_str(), b"v".as_slice())
            .expect("a 1000-byte key must be writable");
        wtxn.commit().unwrap();
    }

    // ── has_index key-format gate (B4) ────────────────────────────────

    /// A rebuild-stamped meta entry is what the gate reads; the fixture
    /// hand-populates scip_meta exactly like `rebuild` does (timestamp +
    /// key_format) instead of running a real rebuild (needs the helper).
    #[test]
    fn has_index_refuses_indexes_not_stamped_with_the_current_key_format() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        let indexer = CSharpSymbolIndexer::new();

        let put_meta = |key: &str, value: &str| {
            let env = crate::symbols::get_shared_scip_env(&db).unwrap();
            let mut wtxn = env.write_txn().unwrap();
            let meta: Database<Str, Str> = env
                .open_database(&wtxn, Some(SCIP_META_DB_NAME))
                .unwrap()
                .unwrap();
            meta.put(&mut wtxn, META_REBUILD_TS, "0").unwrap();
            if !key.is_empty() {
                meta.put(&mut wtxn, key, value).unwrap();
            }
            wtxn.commit().unwrap();
        };

        // Pre-B4 shape: fresh timestamp, NO key_format meta → refused.
        put_meta("", "");
        assert!(
            !indexer.has_index(&db),
            "an index without key_format meta must not count as an index"
        );

        // Current key format → accepted.
        put_meta(META_KEY_FORMAT, crate::constants::SCIP_KEY_FORMAT);
        assert!(
            indexer.has_index(&db),
            "an index stamped with the current key format must be accepted"
        );

        // A previous generation's stamp → refused again.
        put_meta(META_KEY_FORMAT, "1");
        assert!(
            !indexer.has_index(&db),
            "an index stamped with an older key format must be rebuilt, not served"
        );
    }

    // ── resolve_query semantics (hand-populated LMDB — no helper) ──

    /// Two `Validate` overloads share a simple name, `Compute` is unique.
    /// Writes scip_symbols / scip_simple_names / scip_positions directly.
    fn populate_ambiguity_fixture(db_path: &Path) -> (String, String, String) {
        let env = crate::symbols::get_shared_scip_env(db_path).expect("shared env");
        let mut wtxn = env.write_txn().expect("wtxn");

        let validate1 = "csharp Ns . V#Validate().".to_string();
        let validate2 = "csharp Ns . V#Validate(System.String).".to_string();
        let compute = "csharp Ns . C#Compute().".to_string();

        let symbols: Database<Str, Bytes> = env
            .open_database(&wtxn, Some(SCIP_DB_NAME))
            .unwrap()
            .unwrap();
        for key in [&validate1, &validate2, &compute] {
            let refs = serialize_refs(&[StoredReference {
                file: PathBuf::from("src/v.cs"),
                start_line: 1,
                end_line: 1,
                kind: "definition".into(),
            }])
            .unwrap();
            symbols.put(&mut wtxn, key.as_str(), &refs).unwrap();
        }

        let names: Database<Str, Bytes> = env
            .open_database(&wtxn, Some(SCIP_SIMPLE_NAMES_DB_NAME))
            .unwrap()
            .unwrap();
        // Stored deliberately out of order: resolution must sort.
        names
            .put(
                &mut wtxn,
                "Validate",
                &serialize_keys_v1(&[validate2.clone(), validate1.clone()]).unwrap(),
            )
            .unwrap();
        names
            .put(
                &mut wtxn,
                "Compute",
                &serialize_keys_v1(std::slice::from_ref(&compute)).unwrap(),
            )
            .unwrap();

        let positions: Database<Str, Bytes> = env
            .open_database(&wtxn, Some(SCIP_POSITION_DB_NAME))
            .unwrap()
            .unwrap();
        positions
            .put(
                &mut wtxn,
                "src/v.cs:10",
                &serialize_keys_v1(&[validate1.clone(), validate2.clone()]).unwrap(),
            )
            .unwrap();
        positions
            .put(
                &mut wtxn,
                "src/v.cs:20",
                &serialize_keys_v1(std::slice::from_ref(&compute)).unwrap(),
            )
            .unwrap();

        wtxn.commit().unwrap();
        (validate1, validate2, compute)
    }

    #[test]
    fn resolve_name_unique_fuzzy_resolves_the_single_candidate() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        let (_v1, _v2, compute) = populate_ambiguity_fixture(&db);
        let indexer = CSharpSymbolIndexer::new();
        assert_eq!(
            indexer
                .resolve_query(&db, &ImpactQuery::Name("Compute".into()))
                .unwrap(),
            KeyMatch::Resolved(compute)
        );
    }

    #[test]
    fn resolve_name_overloads_come_back_ambiguous_and_sorted() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        let (v1, v2, _compute) = populate_ambiguity_fixture(&db);
        let indexer = CSharpSymbolIndexer::new();
        let m = indexer
            .resolve_query(&db, &ImpactQuery::Name("Validate".into()))
            .unwrap();
        // The pre-fix behaviour silently picked the shortest key here and
        // answered about the wrong overload.
        assert_eq!(m, KeyMatch::Ambiguous(vec![v1, v2]));
    }

    #[test]
    fn resolve_exact_key_is_verbatim_and_never_fuzzy() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        let (v1, _v2, _compute) = populate_ambiguity_fixture(&db);
        let indexer = CSharpSymbolIndexer::new();
        assert_eq!(
            indexer
                .resolve_query(&db, &ImpactQuery::ExactKey(v1.clone()))
                .unwrap(),
            KeyMatch::Resolved(v1)
        );
        // A key that is only a fuzzy neighbour of a stored one must miss.
        assert_eq!(
            indexer
                .resolve_query(&db, &ImpactQuery::ExactKey("csharp Ns . V#Validate".into()))
                .unwrap(),
            KeyMatch::NotFound
        );
    }

    #[test]
    fn resolve_position_single_resolves_two_symbols_are_ambiguous() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        let (v1, v2, compute) = populate_ambiguity_fixture(&db);
        let indexer = CSharpSymbolIndexer::new();

        // One symbol on the line → resolves.
        assert_eq!(
            indexer
                .resolve_query(
                    &db,
                    &ImpactQuery::Position {
                        file: PathBuf::from("src/v.cs"),
                        line: 20
                    }
                )
                .unwrap(),
            KeyMatch::Resolved(compute)
        );

        // Two overloads on the same line → ambiguity, never a shortest pick.
        assert_eq!(
            indexer
                .resolve_query(
                    &db,
                    &ImpactQuery::Position {
                        file: PathBuf::from("src/v.cs"),
                        line: 10
                    }
                )
                .unwrap(),
            KeyMatch::Ambiguous(vec![v1, v2])
        );

        // Nothing defined there → NotFound.
        assert_eq!(
            indexer
                .resolve_query(
                    &db,
                    &ImpactQuery::Position {
                        file: PathBuf::from("src/v.cs"),
                        line: 99
                    }
                )
                .unwrap(),
            KeyMatch::NotFound
        );
    }

    #[test]
    fn references_for_key_returns_stored_definitions() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        let (v1, _v2, _compute) = populate_ambiguity_fixture(&db);
        let indexer = CSharpSymbolIndexer::new();
        // Safe without the helper: the fixture stores definitions, and both
        // the no-helper and no-.sln lazy paths short-circuit to definitions.
        let refs = indexer.find_references_for_key(&db, &v1).unwrap();
        assert_eq!(refs.len(), 1, "definitions only, got {refs:?}");
        assert_eq!(refs[0].kind, "definition");
        assert_eq!(refs[0].file, PathBuf::from("src/v.cs"));
    }

    #[test]
    fn lookup_warnings_reads_the_ref_warnings_db_and_absence_is_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        let (v1, _v2, compute) = populate_ambiguity_fixture(&db);

        // Hand-populate warnings for v1 only (same wire format the lazy and
        // batch write paths use: version byte + bincode Vec<String>).
        let env = crate::symbols::get_shared_scip_env(&db).unwrap();
        {
            let mut wtxn = env.write_txn().unwrap();
            store_ref_warnings(
                &env,
                &mut wtxn,
                &v1,
                &[
                    "FindReferencesAsync failed for Validate: InvalidOperationException: boom"
                        .to_string(),
                ],
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        let indexer = CSharpSymbolIndexer::new();
        let warnings = indexer.lookup_warnings(&db, &v1);
        assert_eq!(warnings.len(), 1, "the stored warning must be read back");
        assert!(
            warnings[0].contains("FindReferencesAsync failed"),
            "warning text must round-trip, got: {}",
            warnings[0]
        );

        // A key with no entry reads as empty (complete), never an error.
        assert!(indexer.lookup_warnings(&db, &compute).is_empty());

        // Empty warnings must REMOVE the entry: absence = complete, so a
        // clean re-resolution clears a stale warning instead of reporting
        // it forever (a partial find-refs result that was cached and later
        // re-resolved cleanly must stop claiming partiality).
        {
            let mut wtxn = env.write_txn().unwrap();
            store_ref_warnings(&env, &mut wtxn, &v1, &[]).unwrap();
            wtxn.commit().unwrap();
        }
        assert!(
            indexer.lookup_warnings(&db, &v1).is_empty(),
            "storing empty warnings must clear the persisted entry"
        );
    }

    // ── B3 warnings: producer-side coverage (the sites that WRITE) ──
    //
    // Every test above hand-populates the warnings store (the consumer
    // half). These two drive the real producer sites instead, so deleting
    // the store_ref_warnings call in parse_and_cache_batch_refs or in the
    // lazy write phase fails here.

    /// A real helper executable, built with the test run's own rustc.
    ///
    /// A `.cmd` script cannot stand in: `validate_helper_path` accepts only
    /// the literal filename `scip-csharp(.exe)`, and CreateProcess refuses
    /// batch content under an `.exe` name, so the file must be a real PE.
    /// Behaviour: `serve` exits without the handshake (the pool's wait_ready
    /// hits EOF and errors, routing the caller through the one-shot
    /// fallback); `find-refs` writes a fixed warnings-bearing JSON to its
    /// `--output` path, mirroring a helper that survived a partial failure.
    /// With `CS_FAKE_INDEX` set, `index` writes [`FAKE_INDEX_DOCUMENTS`] in
    /// that flavour, plus `CS_FAKE_EXTRA_KEY` (when set) defined in
    /// `src/Long.cs` and referenced once; with `CS_FAKE_MARKER` set, every
    /// other subcommand except `--version` appends its name to that file first.
    const FAKE_HELPER_SRC: &str = r##"fn main() {
    if std::env::var("CS_FAKE_HELPER_MODE").as_deref() == Ok("exit75") {
        std::process::exit(75);
    }
    if std::env::var("CS_FAKE_HELPER_MODE").as_deref() == Ok("pend-then-fail") {
        // First invocation pends (75), every later one fails generically (2):
        // drives a mid-retry non-pending failure through the real loop.
        let flag = std::env::var("CS_FAKE_PEND_FLAG")
            .unwrap_or_else(|_| "pend-flag".into());
        if std::path::Path::new(&flag).exists() {
            std::process::exit(2);
        }
        std::fs::write(&flag, "x").unwrap();
        std::process::exit(75);
    }
    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).map(String::as_str).unwrap_or_default();
    if sub == "--version" {
        println!("scip-csharp 0.0.0-fake");
        return;
    }
    let out = args
        .iter()
        .position(|a| a == "--output")
        .and_then(|i| args.get(i + 1));
    if sub == "index" {
        let flavour = std::env::var("CS_FAKE_INDEX").expect("index needs CS_FAKE_INDEX");
        let (docs, rows) = match std::env::var("CS_FAKE_EXTRA_KEY") {
            Ok(key) => (
                format!(
                    r#"{}, {{"relative_path": "src/Long.cs", "occurrences": [{{"range": [2, 0, 2, 5], "symbol": "{key}", "symbol_roles": 1, "kind": "definition"}}]}}]"#,
                    DOCS.strip_suffix(']').unwrap()
                ),
                format!(
                    r#"{}, {{"symbol": "{key}", "file": "src/B.cs", "start_line": 4, "end_line": 4, "kind": "reference"}}]"#,
                    REFS.strip_suffix(']').unwrap()
                ),
            ),
            Err(_) => (DOCS.to_string(), REFS.to_string()),
        };
        let (version, refs) = match flavour.as_str() {
            "2.0" => ("2.0", String::new()),
            "2.1-null" => ("2.1", r#", "references": null"#.to_string()),
            _ => ("2.1", format!(r#", "references": {rows}"#)),
        };
        if flavour == "2.1-warn" {
            eprintln!("[WARN] Workspace error: project Broken could not be loaded");
        }
        let json = format!(
            r#"{{"metadata": {{"version": "{version}", "tool_info": "fake"}}, "documents": {docs}, "external_symbols": []{refs}}}"#
        );
        std::fs::write(out.expect("index needs --output"), json).unwrap();
        return;
    }
    if let Ok(marker) = std::env::var("CS_FAKE_MARKER") {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(marker).unwrap();
        writeln!(f, "{sub}").unwrap();
    }
    if sub == "serve" {
        return;
    }
    let out = out.expect("usage: find-refs ... --output <path>");
    let json = "{\"version\": \"2.0\", \"symbol\": \"csharp Ns . V#Validate().\", \"references\": [{\"file\": \"src/generated.cs\", \"start_line\": 11, \"end_line\": 11, \"kind\": \"reference\"}], \"warnings\": [\"FindReferencesAsync failed for Validate: InvalidOperationException: boom\"]}";
    std::fs::write(out, json).unwrap();
}
"##;

    /// `documents` of the fake `index` output: [`FAKE_REFERENCED`] and
    /// [`FAKE_UNREFERENCED`] defined in `src/A.cs`.
    const FAKE_INDEX_DOCUMENTS: &str = r#"[{"relative_path": "src/A.cs", "occurrences": [
        {"range": [1, 0, 1, 5], "symbol": "csharp Ns . A#Run().", "symbol_roles": 1, "kind": "definition"},
        {"range": [5, 0, 5, 5], "symbol": "csharp Ns . A#Idle().", "symbol_roles": 1, "kind": "definition"}]}]"#;

    /// `references` of the fake 2.1 `index` output: two real references to
    /// [`FAKE_REFERENCED`], a definition-kind row and a key with no definition
    /// (both must be dropped).
    const FAKE_INDEX_REFERENCES: &str = r#"[
        {"symbol": "csharp Ns . A#Run().", "file": "src/B.cs", "start_line": 3, "end_line": 3, "kind": "reference"},
        {"symbol": "csharp Ns . A#Run().", "file": "src/C.cs", "start_line": 7, "end_line": 8, "kind": "reference"},
        {"symbol": "csharp Ns . A#Run().", "file": "src/A.cs", "start_line": 1, "end_line": 1, "kind": "definition"},
        {"symbol": "csharp Ns . Gone#Away().", "file": "src/B.cs", "start_line": 9, "end_line": 9, "kind": "reference"}]"#;

    const FAKE_REFERENCED: &str = "csharp Ns . A#Run().";
    const FAKE_UNREFERENCED: &str = "csharp Ns . A#Idle().";

    fn build_fake_csharp_helper(dir: &Path) -> PathBuf {
        let src_path = dir.join("fake_helper.rs");
        let src = format!(
            "{FAKE_HELPER_SRC}\nconst DOCS: &str = r#\"{FAKE_INDEX_DOCUMENTS}\"#;\n\
             const REFS: &str = r#\"{FAKE_INDEX_REFERENCES}\"#;\n"
        );
        std::fs::write(&src_path, src).unwrap();
        let exe_path = dir.join(if cfg!(windows) {
            "scip-csharp.exe"
        } else {
            "scip-csharp"
        });
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let status = std::process::Command::new(rustc)
            .args(["--edition", "2021", "-Cdebuginfo=0"])
            .arg("-o")
            .arg(&exe_path)
            .arg(&src_path)
            .status()
            .expect("spawn rustc to build the fake helper");
        assert!(status.success(), "rustc failed to compile the fake helper");
        exe_path
    }

    #[test]
    fn batch_parse_persists_helper_warnings_alongside_the_cached_refs() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        let key = "csharp Ns . V#Validate().";

        // Batch-find-refs output exactly as the helper writes it: version
        // 1.0, one resolved symbol, a `warnings` array reporting what it
        // survived.
        let output_path = dir.path().join("batch-refs.json");
        let doc = serde_json::json!({
            "version": scip_parse::SUPPORTED_INDEX_VERSION,
            "results": [{
                "symbol": key,
                "references": [
                    {"file": "src/generated.cs", "start_line": 11, "end_line": 11, "kind": "reference"},
                ],
                "warnings": [
                    "could not compile project 'Broken' — its symbols are missing from the map",
                ],
            }],
        });
        std::fs::write(&output_path, doc.to_string()).unwrap();

        let indexer = CSharpSymbolIndexer::new();
        let cached = indexer
            .parse_and_cache_batch_refs(&db, &output_path)
            .unwrap();
        assert_eq!(cached, 1, "the one result must be cached");

        // The refs landed in the cache...
        let refs = indexer.find_references_for_key(&db, key).unwrap();
        assert!(
            refs.iter()
                .any(|r| r.file == Path::new("src/generated.cs") && r.kind == "reference"),
            "the batch refs must be cached, got {refs:?}"
        );
        // ...and the warning the helper reported must be persisted WITH
        // them — the cached-partial fact is one atomic fact.
        let warnings = indexer.lookup_warnings(&db, key);
        assert_eq!(warnings.len(), 1, "the batch warning must be persisted");
        assert!(
            warnings[0].contains("could not compile project 'Broken'"),
            "warning text must round-trip, got: {}",
            warnings[0]
        );

        // A later batch run WITHOUT the warnings field (an old helper) must
        // clear the stale warning: absence = complete, so a clean
        // re-resolution stops claiming partiality.
        let clean_path = dir.path().join("batch-refs-clean.json");
        let clean = serde_json::json!({
            "version": scip_parse::SUPPORTED_INDEX_VERSION,
            "results": [{
                "symbol": key,
                "references": [],
            }],
        });
        std::fs::write(&clean_path, clean.to_string()).unwrap();
        indexer
            .parse_and_cache_batch_refs(&db, &clean_path)
            .unwrap();
        assert!(
            indexer.lookup_warnings(&db, key).is_empty(),
            "a warnings-free batch result must clear the stale warning"
        );
    }

    #[test]
    #[serial_test::serial]
    fn lazy_cache_miss_persists_helper_warnings_alongside_the_cached_refs() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        // The .sln sits at db_path.parent() — the repo_path the lazy path
        // falls back to when scip_meta carries no META_REPO_PATH.
        std::fs::write(dir.path().join("FakeSolution.sln"), "").unwrap();

        // Cache-miss fixture: one DEFINITION-only entry (no cached refs, no
        // legacy reference kinds), so the lazy find-refs path runs.
        let key = "csharp Ns . V#Validate().";
        {
            let env = crate::symbols::get_shared_scip_env(&db).unwrap();
            let mut wtxn = env.write_txn().unwrap();
            let symbols: Database<Str, Bytes> = env
                .open_database(&wtxn, Some(SCIP_DB_NAME))
                .unwrap()
                .unwrap();
            symbols
                .put(
                    &mut wtxn,
                    key,
                    &serialize_refs(&[StoredReference {
                        file: PathBuf::from("src/v.cs"),
                        start_line: 1,
                        end_line: 1,
                        kind: "definition".into(),
                    }])
                    .unwrap(),
                )
                .unwrap();
            wtxn.commit().unwrap();
        }

        let helper_dir = dir.path().join("helper");
        std::fs::create_dir(&helper_dir).unwrap();
        let helper = build_fake_csharp_helper(&helper_dir);
        let _guard =
            crate::testing::EnvRestore::set(&[(HELPER_ENV_VAR, helper.to_string_lossy().as_ref())]);

        let indexer = CSharpSymbolIndexer::new();
        let refs = indexer.find_references_for_key(&db, key).unwrap();

        // The helper's reference made it through the whole pipe: resident
        // handshake fail → one-shot spawn → JSON parse → return.
        assert!(
            refs.iter()
                .any(|r| r.file == Path::new("src/generated.cs") && r.kind == "reference"),
            "the helper's reference must be returned, got {refs:?}"
        );
        // THE PRODUCER ASSERTION: the helper's warning was persisted by the
        // SAME write phase that cached the refs (find_refs_for_canonical_key).
        let warnings = indexer.lookup_warnings(&db, key);
        assert_eq!(
            warnings.len(),
            1,
            "the lazy path must persist the helper's warning together with the cache"
        );
        assert!(
            warnings[0].contains("FindReferencesAsync failed"),
            "warning text must round-trip, got: {}",
            warnings[0]
        );
    }

    // ── Precomputed references: the rebuild writes scip_ref_cache ───

    struct FakeIndexRun {
        indexer: CSharpSymbolIndexer,
        db: PathBuf,
        marker: PathBuf,
        summary: RebuildSummary,
        _guard: crate::testing::EnvRestore,
    }

    /// Rebuild `dir` against the fake helper's `index` output in `flavour`,
    /// after seeding a stale per-symbol warning for [`FAKE_REFERENCED`].
    fn rebuild_with_fake_index(dir: &Path, flavour: &str, scope: RebuildScope) -> FakeIndexRun {
        let db = dir.join("db");
        std::fs::write(dir.join("FakeSolution.sln"), "").unwrap();
        {
            let env = crate::symbols::get_shared_scip_env(&db).unwrap();
            let mut wtxn = env.write_txn().unwrap();
            store_ref_warnings(&env, &mut wtxn, FAKE_REFERENCED, &["stale".into()]).unwrap();
            wtxn.commit().unwrap();
        }
        let helper_dir = dir.join("helper");
        std::fs::create_dir(&helper_dir).unwrap();
        let helper = build_fake_csharp_helper(&helper_dir);
        let marker = dir.join("non-index-calls.txt");
        let guard = crate::testing::EnvRestore::set(&[
            (HELPER_ENV_VAR, helper.to_string_lossy().as_ref()),
            ("CS_FAKE_INDEX", flavour),
            ("CS_FAKE_MARKER", marker.to_string_lossy().as_ref()),
        ]);
        let indexer = CSharpSymbolIndexer::new();
        let summary = indexer.rebuild(dir, &db, scope).unwrap();
        FakeIndexRun {
            indexer,
            db,
            marker,
            summary,
            _guard: guard,
        }
    }

    fn ref_cache_keys(db: &Path) -> Vec<String> {
        table_keys(db, SCIP_REF_CACHE_DB_NAME)
    }

    fn table_keys(db: &Path, table: &str) -> Vec<String> {
        let env = crate::symbols::get_shared_scip_env(db).unwrap();
        let rtxn = env.read_txn().unwrap();
        let entries: Database<Str, Bytes> = env.open_database(&rtxn, Some(table)).unwrap().unwrap();
        let mut keys: Vec<String> = entries
            .iter(&rtxn)
            .unwrap()
            .map(|e| e.unwrap().0.to_string())
            .collect();
        keys.sort();
        keys
    }

    fn located(refs: &[SymbolReference]) -> Vec<(String, u32, u32, String)> {
        let mut out: Vec<_> = refs
            .iter()
            .map(|r| {
                (
                    r.file.to_string_lossy().replace('\\', "/"),
                    r.start_line,
                    r.end_line,
                    r.kind.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn loc(file: &str, start: u32, end: u32, kind: &str) -> (String, u32, u32, String) {
        (file.to_string(), start, end, kind.to_string())
    }

    #[test]
    #[serial_test::serial]
    fn clean_full_rebuild_answers_every_symbol_from_lmdb_without_the_helper() {
        let dir = tempfile::TempDir::new().unwrap();
        let run = rebuild_with_fake_index(dir.path(), "2.1", RebuildScope::Full);

        assert_eq!(
            run.indexer.refs_coverage(&run.db).as_deref(),
            Some(crate::constants::SCIP_REFS_COVERAGE_COMPLETE)
        );
        assert_eq!(
            run.summary.references_stored, 4,
            "2 definitions + 2 references"
        );
        assert_eq!(
            ref_cache_keys(&run.db),
            vec![FAKE_UNREFERENCED.to_string(), FAKE_REFERENCED.to_string()],
            "one entry per definition key; the definition-less key is dropped"
        );

        let referenced = run
            .indexer
            .find_references_for_key(&run.db, FAKE_REFERENCED)
            .unwrap();
        assert_eq!(
            located(&referenced),
            vec![
                loc("src/A.cs", 1, 1, "definition"),
                loc("src/B.cs", 3, 3, "reference"),
                loc("src/C.cs", 7, 8, "reference"),
            ]
        );
        let unreferenced = run
            .indexer
            .find_references_for_key(&run.db, FAKE_UNREFERENCED)
            .unwrap();
        assert_eq!(
            located(&unreferenced),
            vec![loc("src/A.cs", 5, 5, "definition")]
        );

        assert!(
            !run.marker.exists(),
            "no helper call besides index: {:?}",
            std::fs::read_to_string(&run.marker)
        );
        assert!(
            run.indexer
                .lookup_warnings(&run.db, FAKE_REFERENCED)
                .is_empty(),
            "a warning from before the rebuild must not replay against the new cache"
        );
    }

    #[test]
    #[serial_test::serial]
    fn full_rebuild_skips_an_oversized_precomputed_key_and_caches_the_rest() {
        let dir = tempfile::TempDir::new().unwrap();
        let max_key = crate::symbols::get_shared_scip_env(&dir.path().join("db"))
            .unwrap()
            .max_key_size();
        let oversized = format!("csharp Ns . A#Long({}).", "x".repeat(max_key));
        let _extra = crate::testing::EnvRestore::set(&[("CS_FAKE_EXTRA_KEY", &oversized)]);
        let run = rebuild_with_fake_index(dir.path(), "2.1", RebuildScope::Full);

        let emitted = dir.path().join("emitted.json");
        let status = std::process::Command::new(std::env::var(HELPER_ENV_VAR).unwrap())
            .args(["index", "--output"])
            .arg(&emitted)
            .status()
            .unwrap();
        assert!(status.success());
        let input = scip_parse::parse_json_index(&std::fs::read(&emitted).unwrap()).unwrap();
        assert!(input.definitions.contains_key(&oversized));
        assert!(input
            .references
            .is_some_and(|refs| refs.contains_key(&oversized)));

        assert_eq!(
            run.indexer.refs_coverage(&run.db).as_deref(),
            Some(crate::constants::SCIP_REFS_COVERAGE_COMPLETE)
        );
        assert_eq!(run.summary.symbols_indexed, 2);
        assert_eq!(run.summary.references_stored, 4);
        let normal = vec![FAKE_UNREFERENCED.to_string(), FAKE_REFERENCED.to_string()];
        assert_eq!(ref_cache_keys(&run.db), normal);
        assert_eq!(table_keys(&run.db, SCIP_DB_NAME), normal);
        assert_eq!(
            table_keys(&run.db, SCIP_POSITION_DB_NAME),
            vec!["src/A.cs:1".to_string(), "src/A.cs:5".to_string()]
        );

        let referenced = run
            .indexer
            .find_references_for_key(&run.db, FAKE_REFERENCED)
            .unwrap();
        assert_eq!(
            located(&referenced),
            vec![
                loc("src/A.cs", 1, 1, "definition"),
                loc("src/B.cs", 3, 3, "reference"),
                loc("src/C.cs", 7, 8, "reference"),
            ]
        );
        let unreferenced = run
            .indexer
            .find_references_for_key(&run.db, FAKE_UNREFERENCED)
            .unwrap();
        assert_eq!(
            located(&unreferenced),
            vec![loc("src/A.cs", 5, 5, "definition")]
        );
        assert!(
            !run.marker.exists(),
            "no helper call besides index: {:?}",
            std::fs::read_to_string(&run.marker)
        );
    }

    #[test]
    #[serial_test::serial]
    fn degraded_full_rebuild_caches_referenced_keys_and_leaves_the_rest_lazy() {
        let dir = tempfile::TempDir::new().unwrap();
        let run = rebuild_with_fake_index(dir.path(), "2.1-warn", RebuildScope::Full);

        assert_eq!(
            run.indexer.refs_coverage(&run.db).as_deref(),
            Some(crate::constants::SCIP_REFS_COVERAGE_PARTIAL)
        );
        assert!(!run.indexer.index_warnings(&run.db).is_empty());
        assert_eq!(ref_cache_keys(&run.db), vec![FAKE_REFERENCED.to_string()]);

        let referenced = run
            .indexer
            .find_references_for_key(&run.db, FAKE_REFERENCED)
            .unwrap();
        assert_eq!(located(&referenced).len(), 3, "{referenced:?}");
        assert!(
            !run.marker.exists(),
            "a cached key must not reach the helper"
        );

        let unreferenced = run
            .indexer
            .find_references_for_key(&run.db, FAKE_UNREFERENCED)
            .unwrap();
        let calls = std::fs::read_to_string(&run.marker).unwrap_or_default();
        assert!(
            calls.lines().any(|l| l == "find-refs"),
            "an uncached key must resolve lazily, helper calls: {calls:?}"
        );
        assert!(
            unreferenced
                .iter()
                .any(|r| r.file == Path::new("src/generated.cs")),
            "{unreferenced:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn rebuild_without_solution_wide_references_stays_lazy() {
        for (flavour, scope) in [
            ("2.0", RebuildScope::Full),
            ("2.1-null", RebuildScope::Full),
            (
                "2.1",
                RebuildScope::Files {
                    changed: vec![PathBuf::from("src/A.cs")],
                    deleted: vec![],
                },
            ),
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            let run = rebuild_with_fake_index(dir.path(), flavour, scope);
            assert_eq!(
                run.indexer.refs_coverage(&run.db).as_deref(),
                Some(crate::constants::SCIP_REFS_COVERAGE_LAZY),
                "flavour {flavour}"
            );
            assert!(ref_cache_keys(&run.db).is_empty(), "flavour {flavour}");
            assert_eq!(run.summary.references_stored, 2, "flavour {flavour}");
        }
    }

    // ── Restore gate (issue #225): producer-side coverage ───────────

    //
    // The exit-75 mapping, the retry exhaustion and the lazy find-refs
    // pending branch, driven through the real fake-helper subprocess so
    // deleting any of them fails here.

    fn fake_helper_always_pending(dir: &Path) -> (PathBuf, crate::testing::EnvRestore) {
        let helper_dir = dir.join("helper");
        std::fs::create_dir(&helper_dir).unwrap();
        let helper = build_fake_csharp_helper(&helper_dir);
        let guard = crate::testing::EnvRestore::set(&[
            (HELPER_ENV_VAR, helper.to_string_lossy().as_ref()),
            ("CS_FAKE_HELPER_MODE", "exit75"),
        ]);
        (helper, guard)
    }

    #[test]
    #[serial_test::serial]
    fn exit_75_from_every_helper_entry_maps_to_the_typed_pending_outcome() {
        let dir = tempfile::TempDir::new().unwrap();
        let (helper, _guard) = fake_helper_always_pending(dir.path());
        let indexer = CSharpSymbolIndexer::new();
        let solution = dir.path().join("FakeSolution.sln");
        let output = dir.path().join("out.json");

        let err = indexer
            .invoke_index_helper(&helper, &solution, &output, None)
            .unwrap_err();
        assert!(
            is_restore_pending(&err),
            "index exit 75 must map to the typed outcome, got: {err:#}"
        );

        let err = indexer
            .invoke_find_refs_helper(&helper, &solution, "csharp Ns . V#V().")
            .unwrap_err();
        assert!(
            is_restore_pending(&err),
            "find-refs exit 75 must map to the typed outcome, got: {err:#}"
        );

        let err = indexer
            .invoke_batch_find_refs_helper(
                &helper,
                &solution,
                &dir.path().join("symbols.txt"),
                &output,
            )
            .unwrap_err();
        assert!(
            is_restore_pending(&err),
            "batch exit 75 must map to the typed outcome, got: {err:#}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn rebuild_bails_after_the_retry_cap_with_lmdb_untouched() {
        use super::SymbolIndexer as _;

        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        std::fs::write(dir.path().join("FakeSolution.sln"), "").unwrap();

        let helper_dir = dir.path().join("helper");
        std::fs::create_dir(&helper_dir).unwrap();
        let helper = build_fake_csharp_helper(&helper_dir);
        let _guard = crate::testing::EnvRestore::set(&[
            (HELPER_ENV_VAR, helper.to_string_lossy().as_ref()),
            ("CS_FAKE_HELPER_MODE", "exit75"),
            // 1s base: 0 is filtered (fail-safe against a prod hot-spin),
            // so the backoff runs 1s + 2s — the cap exhausts in ~3s, not 3 min.
            (crate::constants::SCIP_RESTORE_RETRY_INTERVAL_SECS_ENV, "1"),
        ]);

        let indexer = CSharpSymbolIndexer::new();
        let err = indexer
            .rebuild(dir.path(), &db, RebuildScope::Full)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!(
                "did not finish within {RESTORE_PENDING_RETRY_MAX_ATTEMPTS} attempts"
            )),
            "cap exhaustion must name attempts and the abandonment, got: {msg}"
        );
        assert!(
            msg.contains("backoff 1s + 2s"),
            "the message must carry the concrete backoff waits, got: {msg}"
        );
        assert!(
            err.downcast_ref::<RestoreExhaustedError>().is_some(),
            "cap exhaustion must be the typed outcome, got: {err:#}"
        );

        // LMDB untouched: no rebuild meta written, and the pending flag that
        // rode the retry loop is cleared on the way out.
        assert_eq!(
            indexer.index_age(&db),
            u64::MAX,
            "an abandoned rebuild must not write rebuild meta"
        );
        assert!(
            !indexer.restore_pending(&db),
            "the pending flag must be cleared when the retry loop gives up"
        );
    }

    #[test]
    #[serial_test::serial]
    fn restore_pending_backoff_doubles_from_the_configured_base() {
        let _guard = crate::testing::EnvRestore::set(&[(
            crate::constants::SCIP_RESTORE_RETRY_INTERVAL_SECS_ENV,
            "1",
        )]);
        assert_eq!(restore_pending_retry_delay_secs(1), 1);
        assert_eq!(restore_pending_retry_delay_secs(2), 2);
        // Saturating shift cap: an absurd attempt count can never overflow.
        assert_eq!(restore_pending_retry_delay_secs(1_000), 1 << 16);
    }

    #[test]
    #[serial_test::serial]
    fn rebuild_waiting_on_restore_returns_promptly_once_shutdown_is_requested() {
        use super::SymbolIndexer as _;

        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        std::fs::write(dir.path().join("FakeSolution.sln"), "").unwrap();
        let helper_dir = dir.path().join("helper");
        std::fs::create_dir(&helper_dir).unwrap();
        let helper = build_fake_csharp_helper(&helper_dir);
        let _guard = crate::testing::EnvRestore::set(&[
            (HELPER_ENV_VAR, helper.to_string_lossy().as_ref()),
            ("CS_FAKE_HELPER_MODE", "exit75"),
        ]);

        let (tx, rx) = std::sync::mpsc::channel();
        let repo = dir.path().to_path_buf();
        let db_thread = db.clone();
        std::thread::spawn(move || {
            let res = CSharpSymbolIndexer::new().rebuild(&repo, &db_thread, RebuildScope::Full);
            let _ = tx.send(res.map(|_| ()).map_err(|e| format!("{e:#}")));
        });

        // Arm the promptness clock only once the rebuild is actually parked
        // in the restore wait: under full-suite load the helper spawn + LMDB
        // open can outlast a fixed settle sleep, failing the test before the
        // property under test ever engaged. The pending flag rides the retry
        // loop, so it flips true exactly when the wait starts.
        let indexer = CSharpSymbolIndexer::new();
        let park_started = std::time::Instant::now();
        loop {
            assert!(
                park_started.elapsed() < std::time::Duration::from_secs(30),
                "rebuild never reached the restore wait"
            );
            if indexer.restore_pending(&db) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        crate::constants::SHUTDOWN_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
        let outcome = rx.recv_timeout(std::time::Duration::from_secs(10));
        crate::constants::SHUTDOWN_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);

        match outcome {
            Ok(Err(msg)) => assert!(msg.contains("shutdown"), "unexpected error: {msg}"),
            Ok(Ok(())) => panic!("a rebuild waiting on restore must not succeed"),
            Err(_) => {
                panic!("rebuild still parked in the restore wait 10s after shutdown was requested")
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn runtime_teardown_finishes_a_restore_parked_blocking_task_within_grace() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.spawn_blocking(|| {
            while super::sleep_unless_shutdown(std::time::Duration::from_secs(60)) {}
        });
        std::thread::sleep(std::time::Duration::from_millis(300));

        crate::constants::SHUTDOWN_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
        let started = std::time::Instant::now();
        runtime.shutdown_timeout(std::time::Duration::from_secs(
            crate::constants::RUNTIME_SHUTDOWN_GRACE_SECS,
        ));
        let elapsed = started.elapsed();
        crate::constants::SHUTDOWN_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);

        assert!(
            elapsed < std::time::Duration::from_secs(crate::constants::RUNTIME_SHUTDOWN_GRACE_SECS),
            "teardown waited out the full grace ({elapsed:?}): the restore wait did not observe shutdown"
        );
    }

    #[test]
    #[serial_test::serial]
    fn lazy_find_refs_pending_persists_warning_and_caches_nothing() {
        use super::SymbolIndexer as _;

        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        std::fs::write(dir.path().join("FakeSolution.sln"), "").unwrap();
        let key = "csharp Ns . V#Validate().";
        {
            let env = crate::symbols::get_shared_scip_env(&db).unwrap();
            let mut wtxn = env.write_txn().unwrap();
            let symbols: Database<Str, Bytes> = env
                .open_database(&wtxn, Some(SCIP_DB_NAME))
                .unwrap()
                .unwrap();
            symbols
                .put(
                    &mut wtxn,
                    key,
                    &serialize_refs(&[StoredReference {
                        file: PathBuf::from("src/v.cs"),
                        start_line: 1,
                        end_line: 1,
                        kind: "definition".into(),
                    }])
                    .unwrap(),
                )
                .unwrap();
            wtxn.commit().unwrap();
        }

        let (_helper, _guard) = fake_helper_always_pending(dir.path());
        let indexer = CSharpSymbolIndexer::new();
        let refs = indexer.find_references_for_key(&db, key).unwrap();

        // Definitions survive (the previous index stays live)...
        assert!(
            refs.iter()
                .all(|r| r.kind == "definition" && r.file == Path::new("src/v.cs")),
            "only the stored definition may answer, got {refs:?}"
        );
        // ...the honesty warning is persisted on the per-symbol channel...
        let warnings = indexer.lookup_warnings(&db, key);
        assert_eq!(
            warnings.len(),
            1,
            "the pending warning must be persisted, got {warnings:?}"
        );
        assert!(warnings[0].contains("restore in progress"));
        // ...and NOTHING is cached: the next lookup after restore finishes
        // must miss the cache and resolve properly.
        let env = crate::symbols::get_shared_scip_env(&db).unwrap();
        let rtxn = env.read_txn().unwrap();
        let cache: Database<Str, Bytes> = env
            .open_database(&rtxn, Some(SCIP_REF_CACHE_DB_NAME))
            .unwrap()
            .unwrap();
        assert!(
            cache.get(&rtxn, key).unwrap().is_none(),
            "a pending find-refs must not write the ref cache"
        );
    }

    #[test]
    #[serial_test::serial]
    fn rebuild_non_pending_failure_mid_retry_clears_the_pending_flag() {
        use super::SymbolIndexer as _;

        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("db");
        std::fs::write(dir.path().join("FakeSolution.sln"), "").unwrap();
        let flag_path = dir.path().join("pend-flag");

        let helper_dir = dir.path().join("helper");
        std::fs::create_dir(&helper_dir).unwrap();
        let helper = build_fake_csharp_helper(&helper_dir);
        let _guard = crate::testing::EnvRestore::set(&[
            (HELPER_ENV_VAR, helper.to_string_lossy().as_ref()),
            ("CS_FAKE_HELPER_MODE", "pend-then-fail"),
            ("CS_FAKE_PEND_FLAG", flag_path.to_string_lossy().as_ref()),
            (crate::constants::SCIP_RESTORE_RETRY_INTERVAL_SECS_ENV, "1"),
        ]);

        let indexer = CSharpSymbolIndexer::new();
        let err = indexer
            .rebuild(dir.path(), &db, RebuildScope::Full)
            .unwrap_err();

        // Attempt 1 pends (flag set), attempt 2 fails generically: the
        // surfaced error is the generic one (never the typed pending
        // outcome) and the flag must not outlive the loop that set it.
        assert!(
            !is_restore_pending(&err),
            "the generic failure must surface, got: {err:#}"
        );
        assert!(
            !indexer.restore_pending(&db),
            "the pending flag must be cleared when the retry loop ends on a non-pending failure"
        );
    }
}
