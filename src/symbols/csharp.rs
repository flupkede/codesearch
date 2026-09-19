//! C# symbol indexer adapter.
//!
//! Detects the `scip-csharp` helper binary, invokes it as a subprocess
//! to produce a JSON symbol index from a .sln/.csproj, parses the
//! output, and stores references in LMDB.
//!
//! ## Two-phase reference model (Opt 2 — lazy FindReferencesAsync)
//!
//! `rebuild()` calls `scip-csharp index` which now emits **definitions only**
//! (no `FindReferencesAsync` loop). This makes a full rebuild 10–50× faster.
//!
//! `find_references_for_key()` resolves references on demand:
//! 1. Return definitions from `scip_symbols` (always populated after rebuild).
//! 2. Check `scip_ref_cache` for previously resolved references — return if present.
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
//! - Selectively invalidates `scip_ref_cache` for affected symbols only.
//!
//! ## Phase 3 pre-warm (background ref cache filling)
//!
//! After startup Phase 2 completes (definitions indexed), Phase 3 runs
//! `scip-csharp batch-find-refs` to resolve references for all symbols
//! in one workspace session. This amortizes the 30-60s workspace open cost
//! across thousands of symbols, making subsequent `find_impact` calls instant.

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

/// LMDB database name for the SCIP symbol table (definitions only after Opt 2).
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
fn extract_simple_name(scip_symbol: &str) -> String {
    // Strip trailing suffix chars: "Validate()." → "Validate", "MyService#" → "MyService"
    let cleaned = scip_symbol
        .trim_end_matches('.')
        .trim_end_matches("()")
        .trim_end_matches('#');
    // Take last non-empty segment after '#' or '.'
    let last_segment = cleaned
        .rsplit(['#', '.'])
        .find(|s| !s.trim().is_empty())
        .unwrap_or(cleaned)
        .trim();
    // Strip method parameters (e.g. "Add(int, int)" → "Add")
    last_segment
        .split('(')
        .next()
        .unwrap_or(last_segment)
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
        let mut lock = self.helper_path.lock().unwrap();
        *lock = Some(resolved.clone()); // cache both Some and None
        resolved
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
    fn invoke_index_helper(
        &self,
        helper: &Path,
        solution: &Path,
        output_path: &Path,
        project_filter: Option<&Path>,
    ) -> Result<()> {
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

        let stderr_handle = child.stderr.take().map(|stderr| {
            let label = solution_short.clone();
            thread::spawn(move || {
                drain_pipe_to_tracing(stderr, |line| {
                    if !line.is_empty() {
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

        if !status.success() {
            tracing::warn!(
                "scip-csharp exited with {} for {}",
                super::exit_status_text(&status),
                solution_short
            );
            // Don't bail — partial output is acceptable per AGENTS.md spec
        }

        Ok(())
    }

    /// Invoke `scip-csharp find-refs` for a single symbol and return its
    /// references plus the completeness warnings the helper reported.
    ///
    /// This is the "lazy" half of Opt 2: called on first `find_impact` for a
    /// symbol that has not yet been resolved. Result is cached in `scip_ref_cache`.
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
            // scip_symbols (pre-Opt2). Treat those as cache hits — no helper call needed.
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
                    self.invoke_find_refs_helper(&helper, &solution, canonical)?
                }
            };

        // ── Write phase — cache the resolved references ────────────
        {
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
        self.invoke_batch_find_refs_helper(&helper, &solution, &symbols_file, &output_path)?;

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

        // Invoke helper with stderr streaming
        self.invoke_index_helper(&helper, &solution, &output_path, project.as_deref())?;

        // Parse the JSON output
        let index_data = std::fs::read(&output_path)
            .with_context(|| format!("Failed to read symbol index at {}", output_path.display()))?;

        let index = scip_parse::parse_json_index(&index_data)?;

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

            // Selective ref cache invalidation:
            //
            // Pass 1 — definition-site: purge cached refs for symbols whose *definition*
            // is in an affected file. (Original logic — symbols in `stale_symbol_keys`.)
            //
            // Pass 2 — reference-site: also purge any cache entry that has a *reference*
            // in an affected file, even if the symbol's definition lives elsewhere.
            // Without this pass, moving/deleting call sites leaves stale `start_line` /
            // `end_line` values in the cache until the next full rebuild.
            let mut cache_invalidated = 0usize;

            // Pass 1
            for stale_key in &stale_symbol_keys {
                if ref_cache_db.delete(&mut wtxn, stale_key.as_str())? {
                    cache_invalidated += 1;
                }
            }

            // Pass 2 — scan all cached entries for reference-site staleness
            {
                let mut ref_site_stale_keys: Vec<String> = Vec::new();
                let cache_iter = ref_cache_db.iter(&wtxn)?;
                for result in cache_iter {
                    let (key, val) = result?;
                    // Skip entries already invalidated by Pass 1
                    if stale_symbol_keys.contains(key) {
                        continue;
                    }
                    if let Ok(refs) = deserialize_refs(val) {
                        let has_stale_ref = refs.iter().any(|r| {
                            affected_files.contains(&r.file.to_string_lossy().replace('\\', "/"))
                        });
                        if has_stale_ref {
                            ref_site_stale_keys.push(key.to_string());
                        }
                    }
                }
                for key in &ref_site_stale_keys {
                    if ref_cache_db.delete(&mut wtxn, key.as_str())? {
                        cache_invalidated += 1;
                    }
                }
                if !ref_site_stale_keys.is_empty() {
                    tracing::debug!(
                        "Incremental: reference-site invalidated {} additional cache entries",
                        ref_site_stale_keys.len()
                    );
                }
            }

            tracing::debug!(
                "Incremental: invalidated {} ref cache entries total ({} definition-site + reference-site scan)",
                cache_invalidated,
                stale_symbol_keys.len()
            );

            // symbols_db and simple_names_db are merged below, not cleared here.
        }

        // ── Write symbol entries (definitions only after Opt 2) ────
        let mut total_defs = 0usize;
        let mut total_symbols = 0usize;

        for (symbol_name, references) in index.iter() {
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

        // ── Build position index ───────────────────────────────────
        // scip_positions: "<file>:<line>" → [symbol_keys]
        // Maps each definition occurrence to the symbols defined at that position.
        // For incremental rebuilds, old position entries for affected files were
        // already deleted above; here we only write new ones.
        let mut positions: HashMap<String, Vec<String>> = HashMap::new();
        for (symbol_name, references) in index.iter() {
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
        // Unconditional: has_index refuses an index whose key-format stamp is
        // absent or stale, so a key-format change forces exactly one rebuild.
        meta_db.put(
            &mut wtxn,
            META_KEY_FORMAT,
            crate::constants::SCIP_KEY_FORMAT,
        )?;

        wtxn.commit()?;

        let duration_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            "scip-csharp rebuild complete: {} symbols, {} definition entries in {}ms (incremental={})",
            total_symbols,
            total_defs,
            duration_ms,
            is_incremental
        );

        Ok(RebuildSummary {
            symbols_indexed: total_symbols,
            references_stored: total_defs, // definitions only; refs resolved lazily
            duration_ms,
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
    const FAKE_HELPER_SRC: &str = r#"fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("serve") {
        return;
    }
    let out = args
        .iter()
        .position(|a| a == "--output")
        .and_then(|i| args.get(i + 1))
        .expect("usage: find-refs ... --output <path>");
    let json = "{\"version\": \"2.0\", \"symbol\": \"csharp Ns . V#Validate().\", \"references\": [{\"file\": \"src/generated.cs\", \"start_line\": 11, \"end_line\": 11, \"kind\": \"reference\"}], \"warnings\": [\"FindReferencesAsync failed for Validate: InvalidOperationException: boom\"]}";
    std::fs::write(out, json).unwrap();
}
"#;

    fn build_fake_csharp_helper(dir: &Path) -> PathBuf {
        let src_path = dir.join("fake_helper.rs");
        std::fs::write(&src_path, FAKE_HELPER_SRC).unwrap();
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
}
