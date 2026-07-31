//! TypeScript symbol indexer adapter.
//!
//! Detects `scip-typescript` (Sourcegraph's Node CLI, invoked via `npx` unless
//! overridden by `CODESEARCH_SCIP_TYPESCRIPT`), invokes it against a repo's root
//! `tsconfig.json`, parses the standard SCIP protobuf output via
//! [`super::scip_proto::parse_scip_protobuf`], and stores references in LMDB.
//!
//! ## Single-pass reference model (simpler than the C# two-phase model)
//!
//! `scip-typescript` emits full occurrences (definitions AND references) in a
//! single indexing pass. Unlike the C# adapter (`csharp.rs`), there is no lazy
//! `find-refs` subprocess and no `scip_ref_cache` table: `rebuild()` populates
//! `scip_symbols` with everything up front, and `find_references()` /
//! `find_references_by_position()` only ever read LMDB.
//!
//! ## Incremental rebuild
//!
//! `scip-typescript` has no per-file filter flag, so `RebuildScope::Files` falls
//! back to a `Full` rebuild for this adapter (see `rebuild()` below).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::lmdb_registry::TrackedEnv;
use anyhow::{Context, Result};
use heed::types::{Bytes, Str};
use heed::{Database, EnvOpenOptions};
use serde::{Deserialize, Serialize};

use super::scip_proto;
use super::{RebuildScope, RebuildSummary, SymbolIndexer, SymbolReference};

use crate::constants::{
    LANG_TYPESCRIPT, SCIP_LMDB_DEFAULT_MAP_SIZE_MB, SCIP_LMDB_MAP_SIZE_MB_ENV,
    SCIP_POSITION_DB_NAME, SCIP_SIMPLE_NAMES_DB_NAME, SCIP_SYMBOLS_DB_NAME,
    SCIP_TYPESCRIPT_HELPER_ENV, SCIP_TYPESCRIPT_REBUILD_TIMESTAMP_KEY,
};

// ── Constants ─────────────────────────────────────────────────────

/// LMDB database name for the SCIP symbol table (definitions + references).
const SCIP_DB_NAME: &str = SCIP_SYMBOLS_DB_NAME;

/// LMDB database name for the rebuild timestamp / metadata table.
/// Shares the physical table with the C# adapter, but keys are namespaced
/// per-language (see `SCIP_TYPESCRIPT_REBUILD_TIMESTAMP_KEY`).
const SCIP_META_DB_NAME: &str = "scip_meta";

/// LMDB database name for the position-to-symbols index.
const SCIP_POS_DB_NAME: &str = SCIP_POSITION_DB_NAME;

/// LMDB database name for the simple-name-to-symbols index.
const SCIP_NAMES_DB_NAME: &str = SCIP_SIMPLE_NAMES_DB_NAME;

/// Key in the meta database storing the last rebuild timestamp for TypeScript.
const META_REBUILD_TS: &str = SCIP_TYPESCRIPT_REBUILD_TIMESTAMP_KEY;

/// Key in the meta database storing the count of indexed symbols.
const META_SYMBOL_COUNT: &str = "symbol_count:typescript";

// ── Serialized reference type (stored in LMDB via bincode) ────────

/// Schema version byte prepended to all bincode payloads stored in LMDB.
const STORED_REFERENCE_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredReference {
    file: PathBuf,
    start_line: u32,
    end_line: u32,
    kind: String,
}

fn serialize_refs(refs: &[StoredReference]) -> Result<Vec<u8>> {
    let payload = bincode::serialize(refs).with_context(|| "bincode serialize failed")?;
    let mut buf = Vec::with_capacity(1 + payload.len());
    buf.push(STORED_REFERENCE_SCHEMA_VERSION);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

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

const KEYS_LIST_SCHEMA_VERSION: u8 = 1;

fn serialize_keys_v1(keys: &[String]) -> Result<Vec<u8>> {
    let payload = bincode::serialize(keys).with_context(|| "bincode serialize keys failed")?;
    let mut buf = Vec::with_capacity(1 + payload.len());
    buf.push(KEYS_LIST_SCHEMA_VERSION);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

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

/// Extracts the last segment of a canonical SCIP symbol as a simple name.
///
/// SCIP-typescript symbols look like:
/// `scip-typescript npm mypkg 1.0.0 src/math/`add`().` or similar path-scoped
/// forms; we take the last `/` or `.`-delimited, non-empty segment and strip
/// trailing `()`/backtick noise.
fn extract_simple_name(scip_symbol: &str) -> String {
    let cleaned = scip_symbol
        .trim_end_matches('.')
        .trim_end_matches("()")
        .trim_end_matches('#');
    let last_segment = cleaned
        .rsplit(['#', '.', '/'])
        .find(|s| !s.trim().is_empty())
        .unwrap_or(cleaned)
        .trim();
    last_segment
        .trim_matches('`')
        .split('(')
        .next()
        .unwrap_or(last_segment)
        .trim_matches('`')
        .trim()
        .to_string()
}

/// Fuzzy matching heuristic for symbol names (mirrors the C# adapter's version).
fn fuzzy_symbol_match(query: &str, candidate: &str) -> bool {
    let query_parts: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .collect();

    if query_parts.is_empty() {
        return false;
    }

    query_parts.iter().all(|part| candidate.contains(part))
}

// ── TypeScriptSymbolIndexer ────────────────────────────────────────

/// TypeScript adapter: locates `scip-typescript` (bundled override or `npx`),
/// invokes it against the repo's root `tsconfig.json`, parses the resulting
/// SCIP protobuf index, and stores all definitions + references in LMDB in
/// a single pass.
pub struct TypeScriptSymbolIndexer {
    /// Cached detection result.
    /// `None` = not yet attempted.
    /// `Some(None)` = attempted, `scip-typescript` not resolvable.
    /// `Some(Some(invocation))` = resolved invocation (helper path or `npx`).
    helper: std::sync::Mutex<Option<Option<HelperInvocation>>>,
}

/// How to invoke `scip-typescript`: either a direct binary path (env override)
/// or via `npx @sourcegraph/scip-typescript` (the default, no bundled binary required).
#[derive(Debug, Clone)]
enum HelperInvocation {
    /// Direct path to a `scip-typescript` executable (env override).
    Direct(PathBuf),
    /// Invoke via `npx @sourcegraph/scip-typescript` (requires Node/npm on PATH).
    Npx,
}

impl Default for TypeScriptSymbolIndexer {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeScriptSymbolIndexer {
    pub fn new() -> Self {
        Self {
            helper: std::sync::Mutex::new(None),
        }
    }

    /// Locate how to invoke `scip-typescript`.
    ///
    /// Search order:
    /// 1. `CODESEARCH_SCIP_TYPESCRIPT` env var — direct path to a `scip-typescript` binary.
    /// 2. `npx` on `$PATH` — Node's package runner resolves/installs `scip-typescript`
    ///    on demand. Requires Node + npm; no bundled binary is shipped for TS (MVP).
    ///
    /// Results are cached — both positive (found) and negative (not found).
    fn detect_helper(&self) -> Option<HelperInvocation> {
        {
            let lock = self.helper.lock().unwrap();
            if let Some(cached) = lock.as_ref() {
                return cached.clone();
            }
        }

        let resolved = self.resolve_helper();
        let mut lock = self.helper.lock().unwrap();
        *lock = Some(resolved.clone());
        resolved
    }

    fn resolve_helper(&self) -> Option<HelperInvocation> {
        // 1. Environment variable override — direct binary path.
        if let Ok(path) = std::env::var(SCIP_TYPESCRIPT_HELPER_ENV) {
            let p = PathBuf::from(&path);
            if p.is_file() {
                tracing::debug!(
                    "scip-typescript helper found via {}={}",
                    SCIP_TYPESCRIPT_HELPER_ENV,
                    path
                );
                return Some(HelperInvocation::Direct(p));
            }
            tracing::warn!(
                "{}={} does not point to a regular file, falling back to npx",
                SCIP_TYPESCRIPT_HELPER_ENV,
                path
            );
        }

        // 2. `npx` on PATH.
        let lookup_cmd = if cfg!(windows) { "where" } else { "which" };
        if let Ok(output) = Command::new(lookup_cmd).arg("npx").output() {
            if output.status.success() {
                tracing::debug!("scip-typescript will be invoked via npx");
                return Some(HelperInvocation::Npx);
            }
        }

        None
    }

    /// Find the root `tsconfig.json` for a repo (MVP: top-level only, no
    /// monorepo multi-tsconfig resolution).
    fn find_tsconfig(repo_path: &Path) -> Option<PathBuf> {
        let candidate = repo_path.join("tsconfig.json");
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    }

    /// Open or create the SCIP LMDB environment for a given repo database path.
    /// Shares the same on-disk tables as the C# adapter (`db_path/scip/`),
    /// distinguished by namespaced keys/values where needed.
    fn open_scip_env(&self, db_path: &Path) -> Result<TrackedEnv> {
        let scip_dir = db_path.join("scip");
        std::fs::create_dir_all(&scip_dir)
            .with_context(|| format!("Failed to create SCIP directory: {}", scip_dir.display()))?;

        let map_size_mb = std::env::var(SCIP_LMDB_MAP_SIZE_MB_ENV)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(SCIP_LMDB_DEFAULT_MAP_SIZE_MB);
        let mut opts = EnvOpenOptions::new();
        opts.map_size(map_size_mb * 1024 * 1024).max_dbs(10);
        let env =
            unsafe { TrackedEnv::open(&opts, &scip_dir, &format!("SCIP({})", db_path.display()))? };

        let mut wtxn = env.write_txn()?;
        env.create_database::<Str, Bytes>(&mut wtxn, Some(SCIP_DB_NAME))?;
        env.create_database::<Str, Str>(&mut wtxn, Some(SCIP_META_DB_NAME))?;
        env.create_database::<Str, Bytes>(&mut wtxn, Some(SCIP_POS_DB_NAME))?;
        env.create_database::<Str, Bytes>(&mut wtxn, Some(SCIP_NAMES_DB_NAME))?;
        wtxn.commit()?;

        Ok(env)
    }

    /// Invoke `scip-typescript index` against `project_root`, writing the SCIP
    /// protobuf index to `output_path`.
    fn invoke_index_helper(
        &self,
        invocation: &HelperInvocation,
        project_root: &Path,
        output_path: &Path,
    ) -> Result<()> {
        let mut cmd = match invocation {
            HelperInvocation::Direct(path) => Command::new(path),
            HelperInvocation::Npx => {
                // On Windows, `npx` is a shell shim (`npx.cmd`/`npx.ps1`), not a bare
                // `.exe` — `std::process::Command` does NOT consult `PATHEXT` the way
                // `cmd.exe` does, so `Command::new("npx")` fails with "program not
                // found" even though `where npx` (used in `resolve_helper`) succeeds.
                // Route through `cmd /C` on Windows so the shell resolves the shim.
                // NOTE: the unscoped npm name `scip-typescript` is a squatted
                // security placeholder (0.0.1-security, no functionality) — the
                // real Sourcegraph package is published as the scoped package
                // `@sourcegraph/scip-typescript` (bin name `scip-typescript`).
                // `-y` avoids an interactive "ok to install?" prompt.
                if cfg!(windows) {
                    let mut c = Command::new("cmd");
                    c.arg("/C")
                        .arg("npx")
                        .arg("-y")
                        .arg("@sourcegraph/scip-typescript");
                    c
                } else {
                    let mut c = Command::new("npx");
                    c.arg("-y").arg("@sourcegraph/scip-typescript");
                    c
                }
            }
        };

        cmd.arg("index")
            .arg("--output")
            .arg(output_path)
            .current_dir(project_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        tracing::info!(
            "Running scip-typescript index in {:?}: {:?}",
            project_root,
            cmd
        );

        let output = cmd.output().with_context(|| {
            format!(
                "Failed to execute scip-typescript for {}",
                project_root.display()
            )
        })?;

        for line in String::from_utf8_lossy(&output.stderr).lines() {
            if !line.is_empty() {
                tracing::info!("[scip-typescript] {}", line);
            }
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if !line.is_empty() {
                tracing::debug!("[scip-typescript] {}", line);
            }
        }

        if !output.status.success() {
            tracing::warn!(
                "scip-typescript exited with {} for {}",
                output.status,
                project_root.display()
            );
            // Don't bail — partial output is acceptable, mirroring the C# adapter.
        }

        Ok(())
    }

    /// Resolve a user-supplied symbol query to a canonical SCIP symbol key.
    /// Exact match first, then fuzzy match via the simple-name index.
    fn resolve_canonical_key(&self, env: &TrackedEnv, symbol: &str) -> Result<Option<String>> {
        let rtxn = env.read_txn()?;

        let symbols_db: Database<Str, Bytes> = match env.open_database(&rtxn, Some(SCIP_DB_NAME))? {
            Some(db) => db,
            None => return Ok(None),
        };

        if symbols_db.get(&rtxn, symbol)?.is_some() {
            return Ok(Some(symbol.to_string()));
        }

        let simple_names_db: Database<Str, Bytes> =
            match env.open_database(&rtxn, Some(SCIP_NAMES_DB_NAME))? {
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
}

impl SymbolIndexer for TypeScriptSymbolIndexer {
    fn language(&self) -> &str {
        LANG_TYPESCRIPT
    }

    fn rebuild(
        &self,
        repo_path: &Path,
        db_path: &Path,
        scope: RebuildScope,
    ) -> Result<RebuildSummary> {
        let invocation = self.detect_helper().ok_or_else(|| {
            anyhow::anyhow!(
                "scip-typescript not resolvable (no npx on PATH and {} unset). \
                 Install Node.js or set {} to a scip-typescript binary path.",
                SCIP_TYPESCRIPT_HELPER_ENV,
                SCIP_TYPESCRIPT_HELPER_ENV
            )
        })?;

        // RebuildScope::Files has no per-file equivalent for scip-typescript
        // (no --filter-project style flag), so we always do a Full rebuild.
        // RebuildScope::Project(_) falls through the same way: TS MVP only
        // supports a single root tsconfig.json, so a "project scope" rebuild
        // is indistinguishable from a Full one.
        if let RebuildScope::Files { .. } = scope {
            tracing::debug!(
                "TypeScript adapter: RebuildScope::Files requested, falling back to Full \
                 (scip-typescript has no incremental/file-filter mode)"
            );
        }

        if Self::find_tsconfig(repo_path).is_none() {
            anyhow::bail!("No tsconfig.json found in {}", repo_path.display());
        }
        // scip-typescript discovers tsconfig.json from project_root itself,
        // so we only need to confirm one exists above - the path itself is
        // never passed to the CLI.

        let start = std::time::Instant::now();

        let temp_dir = std::env::temp_dir().join("codesearch-scip-ts");
        std::fs::create_dir_all(&temp_dir)?;
        // Include PID + wall-clock nanoseconds (not Instant::elapsed(), which
        // is tiny/low-entropy right after creation) to avoid filename
        // collisions when multiple TS rebuilds for repos sharing a directory
        // basename are in flight concurrently - mirrors the same pattern in
        // csharp.rs's temp-file naming.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let output_path = temp_dir.join(format!(
            "index-{}-{}-{:x}.scip",
            repo_path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            nanos
        ));
        struct TempFileGuard(PathBuf);
        impl Drop for TempFileGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _output_guard = TempFileGuard(output_path.clone());

        self.invoke_index_helper(&invocation, repo_path, &output_path)?;

        let index_data = std::fs::read(&output_path)
            .with_context(|| format!("Failed to read SCIP index at {}", output_path.display()))?;

        let index = scip_proto::parse_scip_protobuf(&index_data)?;

        let env = self.open_scip_env(db_path)?;
        let mut wtxn = env.write_txn()?;

        let symbols_db: Database<Str, Bytes> =
            env.create_database(&mut wtxn, Some(SCIP_DB_NAME))?;
        let meta_db: Database<Str, Str> =
            env.create_database(&mut wtxn, Some(SCIP_META_DB_NAME))?;
        let positions_db: Database<Str, Bytes> =
            env.create_database(&mut wtxn, Some(SCIP_POS_DB_NAME))?;
        let simple_names_db: Database<Str, Bytes> =
            env.create_database(&mut wtxn, Some(SCIP_NAMES_DB_NAME))?;

        // Full rebuild only (MVP): wipe and repopulate this language's entries.
        // NOTE: scip_symbols/scip_positions/scip_simple_names are shared physical
        // tables with the C# adapter. Clearing them here would destroy C#'s data
        // if both languages share the same db_path. In practice each repo has at
        // most one applicable language's tsconfig.json/.sln, so this is safe for
        // the MVP; a follow-up should namespace keys by language if repos ever
        // mix both indexers against the same db_path.
        symbols_db.clear(&mut wtxn)?;
        positions_db.clear(&mut wtxn)?;
        simple_names_db.clear(&mut wtxn)?;

        let mut total_symbols = 0usize;
        let mut total_refs = 0usize;

        for (symbol_name, refs) in index.iter() {
            let stored: Vec<StoredReference> = refs
                .iter()
                .map(|r| StoredReference {
                    file: r.file.clone(),
                    start_line: r.start_line,
                    end_line: r.end_line,
                    kind: r.kind.clone(),
                })
                .collect();

            let value_bytes = serialize_refs(&stored)
                .with_context(|| format!("Failed to serialize references for {}", symbol_name))?;
            symbols_db.put(&mut wtxn, symbol_name.as_str(), &value_bytes)?;

            total_refs += stored.len();
            total_symbols += 1;
        }

        // ── Build position index (definitions only) ────────────────
        let mut positions: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (symbol_name, refs) in index.iter() {
            for r in refs.iter().filter(|r| r.kind == "definition") {
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

        // ── Build simple-name index ─────────────────────────────────
        let mut all_simple_names: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for symbol_name in index.keys() {
            let simple = extract_simple_name(symbol_name);
            if !simple.is_empty() {
                all_simple_names
                    .entry(simple)
                    .or_default()
                    .push(symbol_name.clone());
            }
        }
        for (key, keys) in &all_simple_names {
            let bytes = serialize_keys_v1(keys)
                .with_context(|| format!("Failed to serialize simple name key: {}", key))?;
            simple_names_db.put(&mut wtxn, key.as_str(), &bytes)?;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        meta_db.put(&mut wtxn, META_REBUILD_TS, now.to_string().as_str())?;
        meta_db.put(
            &mut wtxn,
            META_SYMBOL_COUNT,
            total_symbols.to_string().as_str(),
        )?;

        wtxn.commit()?;

        let duration_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            "scip-typescript rebuild complete: {} symbols, {} reference entries in {}ms",
            total_symbols,
            total_refs,
            duration_ms
        );

        Ok(RebuildSummary {
            symbols_indexed: total_symbols,
            references_stored: total_refs,
            duration_ms,
        })
    }

    fn find_references(&self, db_path: &Path, symbol: &str) -> Result<Vec<SymbolReference>> {
        let env = self.open_scip_env(db_path)?;

        let canonical = match self.resolve_canonical_key(&env, symbol)? {
            Some(k) => k,
            None => {
                tracing::debug!("Symbol '{}' not found in TypeScript index", symbol);
                return Ok(vec![]);
            }
        };

        let rtxn = env.read_txn()?;
        let symbols_db: Database<Str, Bytes> = match env.open_database(&rtxn, Some(SCIP_DB_NAME))? {
            Some(db) => db,
            None => return Ok(vec![]),
        };

        let stored = match symbols_db.get(&rtxn, &canonical)? {
            Some(bytes) => deserialize_refs(bytes)?,
            None => return Ok(vec![]),
        };

        Ok(stored
            .into_iter()
            .map(|r| SymbolReference {
                file: r.file,
                start_line: r.start_line,
                end_line: r.end_line,
                kind: r.kind,
            })
            .collect())
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
            .open_database(&rtxn, Some(SCIP_POS_DB_NAME))?
            .ok_or_else(|| anyhow::anyhow!("Position index not found. Run a rebuild first."))?;

        let pos_key = format!("{}:{}", file.to_string_lossy().replace('\\', "/"), line);

        let candidate_keys: Vec<String> = match positions_db.get(&rtxn, &pos_key as &str)? {
            Some(b) => deserialize_keys_v1(b)?,
            None => return Ok(vec![]),
        };

        let chosen = candidate_keys.iter().min_by_key(|k| k.len()).cloned();
        drop(rtxn);
        drop(env);

        match chosen {
            Some(k) => self.find_references(db_path, &k),
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
        self.index_age(db_path) != u64::MAX
    }

    fn is_available(&self) -> bool {
        self.detect_helper().is_some()
    }

    /// TypeScript adapter is only applicable when a top-level `tsconfig.json` exists.
    fn applies_to(&self, repo_path: &Path) -> bool {
        Self::find_tsconfig(repo_path).is_some()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_simple_name() {
        assert_eq!(
            extract_simple_name("scip-typescript npm mypkg 1.0.0 `src/math.ts`/add()."),
            "add"
        );
        assert_eq!(extract_simple_name("MyClass#method()."), "method");
        assert_eq!(extract_simple_name(""), "");
    }

    #[test]
    fn test_fuzzy_symbol_match() {
        assert!(fuzzy_symbol_match(
            "add",
            "scip-typescript npm mypkg 1.0.0 `src/math.ts`/add()."
        ));
        assert!(!fuzzy_symbol_match(
            "subtract",
            "scip-typescript npm mypkg 1.0.0 `src/math.ts`/add()."
        ));
        assert!(!fuzzy_symbol_match("", "anything"));
    }

    #[test]
    fn test_serialize_refs_round_trip() {
        let refs = vec![StoredReference {
            file: PathBuf::from("a.ts"),
            start_line: 1,
            end_line: 1,
            kind: "definition".into(),
        }];
        let bytes = serialize_refs(&refs).unwrap();
        assert_eq!(bytes[0], STORED_REFERENCE_SCHEMA_VERSION);
        let decoded = deserialize_refs(&bytes).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].kind, "definition");
    }

    #[test]
    fn test_deserialize_refs_rejects_empty() {
        assert!(deserialize_refs(&[]).is_err());
    }

    #[test]
    fn test_deserialize_refs_rejects_bad_version() {
        assert!(deserialize_refs(&[99, 1, 2, 3]).is_err());
    }

    #[test]
    fn test_find_tsconfig_requires_root_file() {
        // Use tempfile::TempDir so cleanup runs on panic too. The previous
        // manual std::env::temp_dir().join(unique) + bare last-line
        // remove_dir_all leaked the dir on any mid-test assertion failure.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        assert!(TypeScriptSymbolIndexer::find_tsconfig(dir).is_none());
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        assert!(TypeScriptSymbolIndexer::find_tsconfig(dir).is_some());
        // `tmp` dropped at end of scope → dir removed even on panic.
    }
}
