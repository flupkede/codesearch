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
//! `find_references()` resolves references on demand:
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
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use heed::types::{Bytes, Str};
use heed::{Database, Env, EnvOpenOptions};
use serde::{Deserialize, Serialize};

use super::scip_parse;
use super::{PrewarmSummary, RebuildScope, RebuildSummary, SymbolIndexer, SymbolReference};

// ── Constants ─────────────────────────────────────────────────────

/// LMDB database name for the SCIP symbol table (definitions only after Opt 2).
const SCIP_DB_NAME: &str = crate::constants::SCIP_SYMBOLS_DB_NAME;

/// LMDB database name for the rebuild timestamp.
const SCIP_META_DB_NAME: &str = "scip_meta";

/// LMDB database name for the position-to-symbols index.
const SCIP_POSITION_DB_NAME: &str = crate::constants::SCIP_POSITION_DB_NAME;

/// LMDB database name for the simple-name-to-symbols index.
const SCIP_SIMPLE_NAMES_DB_NAME: &str = crate::constants::SCIP_SIMPLE_NAMES_DB_NAME;

/// LMDB database name for the on-demand reference cache (populated by find-refs).
const SCIP_REF_CACHE_DB_NAME: &str = crate::constants::SCIP_REF_CACHE_DB_NAME;

/// Key in the meta database that stores the last rebuild timestamp (UNIX epoch seconds).
const META_REBUILD_TS: &str = crate::constants::SCIP_REBUILD_TIMESTAMP_KEY;

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
                tracing::debug!("scip-csharp helper found via {}={}", HELPER_ENV_VAR, path);
                return Some(p);
            }
            tracing::warn!(
                "{}={} does not exist, falling back to default search",
                HELPER_ENV_VAR,
                path
            );
        }

        // 2. Next to the codesearch binary
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
                    tracing::debug!("scip-csharp helper found on PATH: {}", path_str);
                    return Some(p);
                }
            }
        }

        None
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

    /// Open or create the SCIP LMDB environment for a given repo database path.
    ///
    /// Pre-opens ALL named databases so they exist before first use.
    /// LMDB requires named DBs to be created (or opened) in a write txn
    /// before they can be read in later read txns within the same env session.
    fn open_scip_env(&self, db_path: &Path) -> Result<Env> {
        let scip_dir = db_path.join("scip");
        std::fs::create_dir_all(&scip_dir)
            .with_context(|| format!("Failed to create SCIP directory: {}", scip_dir.display()))?;

        // SAFETY: same pattern as vectordb/store.rs — LMDB mmap contract.
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(64 * 1024 * 1024) // 64MB — symbol indexes are small
                .max_dbs(10) // 5 named DBs + headroom
                .open(&scip_dir)?
        };

        // Eagerly create / re-open all named databases.
        let mut wtxn = env.write_txn()?;
        env.create_database::<Str, Bytes>(&mut wtxn, Some(SCIP_DB_NAME))?;
        env.create_database::<Str, Str>(&mut wtxn, Some(SCIP_META_DB_NAME))?;
        env.create_database::<Str, Bytes>(&mut wtxn, Some(SCIP_POSITION_DB_NAME))?;
        env.create_database::<Str, Bytes>(&mut wtxn, Some(SCIP_SIMPLE_NAMES_DB_NAME))?;
        env.create_database::<Str, Bytes>(&mut wtxn, Some(SCIP_REF_CACHE_DB_NAME))?;
        wtxn.commit()?;

        Ok(env)
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
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.is_empty() {
                        tracing::info!("[scip-csharp:{}] {}", label, line);
                    }
                }
            })
        });

        let stdout_handle = child.stdout.take().map(|stdout| {
            let label = solution_short.clone();
            thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if !line.is_empty() {
                        tracing::debug!("[scip-csharp:{}] {}", label, line);
                    }
                }
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
            tracing::warn!("scip-csharp exited with {} for {}", status, solution_short);
            // Don't bail — partial output is acceptable per AGENTS.md spec
        }

        Ok(())
    }

    /// Invoke `scip-csharp find-refs` for a single symbol and return its references.
    ///
    /// This is the "lazy" half of Opt 2: called on first `find_impact` for a symbol
    /// that has not yet been resolved. Result is cached in `scip_ref_cache`.
    fn invoke_find_refs_helper(
        &self,
        helper: &Path,
        solution: &Path,
        symbol: &str,
    ) -> Result<Vec<StoredReference>> {
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
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.is_empty() {
                        tracing::info!("[scip-csharp find-refs:{}] {}", label, line);
                    }
                }
            })
        });

        let stdout_handle = child.stdout.take().map(|stdout| {
            let label = solution_short.clone();
            thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if !line.is_empty() {
                        tracing::debug!("[scip-csharp find-refs:{}] {}", label, line);
                    }
                }
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
                status,
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

        Ok(stored)
    }

    // ── Internal lookup helpers ────────────────────────────────────

    /// Resolve a (possibly fuzzy) symbol name to the canonical SCIP key stored
    /// in `scip_symbols`. Returns `None` if no matching symbol is found.
    fn resolve_canonical_key(&self, env: &Env, symbol: &str) -> Result<Option<String>> {
        let rtxn = env.read_txn()?;

        let symbols_db: Database<Str, Bytes> = match env.open_database(&rtxn, Some(SCIP_DB_NAME))? {
            Some(db) => db,
            None => return Ok(None),
        };

        // Exact match first
        if symbols_db.get(&rtxn, symbol)?.is_some() {
            return Ok(Some(symbol.to_string()));
        }

        // Fuzzy via simple-name index
        let simple_names_db: Database<Str, Bytes> =
            match env.open_database(&rtxn, Some(SCIP_SIMPLE_NAMES_DB_NAME))? {
                Some(db) => db,
                None => return Ok(None),
            };

        let simple = extract_simple_name(symbol);
        let candidates: Vec<String> = match simple_names_db.get(&rtxn, &simple as &str)? {
            Some(b) => deserialize_keys_v1(b)?,
            None => return Ok(None),
        };

        let chosen = candidates
            .iter()
            .filter(|k| fuzzy_symbol_match(symbol, k))
            .min_by_key(|k| k.len())
            .cloned();

        Ok(chosen)
    }

    /// Inner implementation: fetch references for an EXACT (canonical) symbol key.
    ///
    /// - Returns definitions from `scip_symbols` (always present after rebuild).
    /// - Returns cached references from `scip_ref_cache` if present.
    /// - On cache miss: invokes `scip-csharp find-refs`, stores in `scip_ref_cache`.
    ///
    /// Inner implementation: fetch references for an EXACT (canonical) symbol key.
    ///
    /// Opens its own LMDB environment so the caller's env handle (if any) is not
    /// held concurrently with the internal write txn that caches lazy results.
    /// This avoids the "two Env objects on the same path" footgun.
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

        let lazy_refs = self.invoke_find_refs_helper(&helper, &solution, canonical)?;

        // ── Write phase — cache the resolved references ────────────
        {
            let mut wtxn = env.write_txn()?;
            let ref_cache_db: Database<Str, Bytes> =
                env.create_database(&mut wtxn, Some(SCIP_REF_CACHE_DB_NAME))?;
            let cached_bytes = serialize_refs(&lazy_refs)
                .with_context(|| format!("Failed to serialize refs for cache: {}", canonical))?;
            ref_cache_db.put(&mut wtxn, canonical, &cached_bytes)?;
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
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.is_empty() {
                        tracing::info!("[scip-csharp batch-find-refs:{}] {}", label, line);
                    }
                }
            })
        });

        let stdout_handle = child.stdout.take().map(|stdout| {
            let label = solution_short.clone();
            thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if !line.is_empty() {
                        tracing::debug!("[scip-csharp batch-find-refs:{}] {}", label, line);
                    }
                }
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
                status,
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
            ref_cache_db.put(&mut wtxn, result.symbol.as_str(), &bytes)?;
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
                            symbols_db.put(&mut wtxn, key.as_str(), &b)?;
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

            symbols_db.put(&mut wtxn, symbol_name.as_str(), &value_bytes)?;
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
            positions_db.put(&mut wtxn, key.as_str(), &bytes)?;
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
            simple_names_db.put(&mut wtxn, key.as_str(), &bytes)?;
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

    fn find_references(&self, db_path: &Path, symbol: &str) -> Result<Vec<SymbolReference>> {
        // Resolve to canonical key in a short-lived env scope, then drop it before
        // entering find_refs_for_canonical_key (which opens its own env).
        // This ensures no two Env handles are live on the same path simultaneously.
        let canonical = {
            let env = self.open_scip_env(db_path)?;
            match self.resolve_canonical_key(&env, symbol)? {
                Some(k) => k,
                None => {
                    tracing::debug!("Symbol '{}' not found in index", symbol);
                    return Ok(vec![]);
                }
            }
            // env dropped here
        };

        self.find_refs_for_canonical_key(db_path, &canonical)
    }

    fn find_references_by_position(
        &self,
        db_path: &Path,
        file: &Path,
        line: u32,
    ) -> Result<Vec<SymbolReference>> {
        let env = self.open_scip_env(db_path)?;
        let rtxn = env.read_txn()?;

        let positions_db: Database<Str, Bytes> = env
            .open_database(&rtxn, Some(SCIP_POSITION_DB_NAME))?
            .ok_or_else(|| anyhow::anyhow!("Position index not found. Run a rebuild first."))?;

        // Normalize file path to forward-slash (Windows compat)
        let pos_key = format!("{}:{}", file.to_string_lossy().replace('\\', "/"), line);

        let candidate_keys: Vec<String> = match positions_db.get(&rtxn, &pos_key as &str)? {
            Some(b) => deserialize_keys_v1(b)?,
            None => return Ok(vec![]),
        };

        // Pick shortest (most specific) symbol defined at this position
        let chosen = candidate_keys.iter().min_by_key(|k| k.len()).cloned();
        drop(rtxn);
        drop(env); // must drop before find_refs_for_canonical_key opens its own env

        match chosen {
            Some(k) => self.find_refs_for_canonical_key(db_path, &k),
            None => Ok(vec![]),
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

    fn has_index(&self, db_path: &Path) -> bool {
        let scip_dir = db_path.join("scip");
        if !scip_dir.exists() {
            return false;
        }
        // Quick check: if index_age is finite, the index exists
        self.index_age(db_path) != u64::MAX
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
}
