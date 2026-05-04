//! C# symbol indexer adapter.
//!
//! Detects the `scip-csharp` helper binary, invokes it as a subprocess
//! to produce a JSON symbol index from a .sln/.csproj, parses the
//! output, and stores references in LMDB.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use heed::types::{Bytes, Str};
use heed::{Database, Env, EnvOpenOptions};
use serde::{Deserialize, Serialize};

use super::scip_parse;
use super::{RebuildScope, RebuildSummary, SymbolIndexer, SymbolReference};

// ── Constants ─────────────────────────────────────────────────────

/// LMDB database name for the SCIP symbol table.
const SCIP_DB_NAME: &str = crate::constants::SCIP_SYMBOLS_DB_NAME;

/// LMDB database name for the rebuild timestamp.
const SCIP_META_DB_NAME: &str = "scip_meta";

/// LMDB database name for the position-to-symbols index.
const SCIP_POSITION_DB_NAME: &str = crate::constants::SCIP_POSITION_DB_NAME;

/// LMDB database name for the simple-name-to-symbols index.
const SCIP_SIMPLE_NAMES_DB_NAME: &str = crate::constants::SCIP_SIMPLE_NAMES_DB_NAME;

/// Key in the meta database that stores the last rebuild timestamp (UNIX epoch seconds).
const META_REBUILD_TS: &str = crate::constants::SCIP_REBUILD_TIMESTAMP_KEY;

/// Key in the meta database storing the count of indexed symbols.
#[allow(dead_code)]
const META_SYMBOL_COUNT: &str = "symbol_count";

/// Environment variable override for the helper binary path.
const HELPER_ENV_VAR: &str = crate::constants::SCIP_CSHARP_HELPER_ENV;

/// Helper binary name (without extension).
const HELPER_BIN_NAME: &str = crate::constants::SCIP_CSHARP_HELPER_NAME;

/// Debounce period for .cs file changes (seconds).
#[allow(dead_code)]
pub const CSHARP_REBUILD_DEBOUNCE_SECS: u64 = crate::constants::SCIP_CSHARP_DEBOUNCE_MS / 1000;

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
    // Strip trailing parens/period (e.g. "Validate()." → "Validate")
    let cleaned = scip_symbol.trim_end_matches('.').trim_end_matches("()");
    // Take last segment after '#' or '.'
    let last_segment = cleaned.rsplit(['#', '.']).next().unwrap_or(cleaned).trim();
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    fn find_csproj_for_file(repo_path: &Path, file_path: &Path) -> Option<PathBuf> {
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
    fn open_scip_env(&self, db_path: &Path) -> Result<Env> {
        let scip_dir = db_path.join("scip");
        std::fs::create_dir_all(&scip_dir)
            .with_context(|| format!("Failed to create SCIP directory: {}", scip_dir.display()))?;

        // SAFETY: same pattern as vectordb/store.rs — LMDB mmap contract.
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(64 * 1024 * 1024) // 64MB — symbol indexes are small
                .max_dbs(8)
                .open(&scip_dir)?
        };
        Ok(env)
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

        // Determine solution/project from scope
        let (solution, project) = match &scope {
            RebuildScope::Full => {
                let sln = Self::find_solution(repo_path).ok_or_else(|| {
                    anyhow::anyhow!("No .sln file found in {}", repo_path.display())
                })?;
                (sln, None)
            }
            RebuildScope::Project(csproj) => {
                let sln = Self::find_solution(repo_path).unwrap_or_else(|| csproj.clone());
                (sln, Some(csproj.clone()))
            }
            RebuildScope::Files(files) => {
                if let Some(first_file) = files.first() {
                    let csproj = Self::find_csproj_for_file(repo_path, first_file)
                        .unwrap_or_else(|| first_file.clone());
                    let sln = Self::find_solution(repo_path).unwrap_or_else(|| csproj.clone());
                    (sln, Some(csproj))
                } else {
                    bail!("RebuildScope::Files is empty");
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

        // Invoke helper
        let mut cmd = Command::new(&helper);
        cmd.arg("index")
            .arg("--solution")
            .arg(&solution)
            .arg("--output")
            .arg(&output_path);

        if let Some(ref proj) = project {
            cmd.arg("--filter-project").arg(proj);
        }

        tracing::info!("Running scip-csharp: {:?}", cmd);

        let output = cmd
            .output()
            .with_context(|| format!("Failed to execute scip-csharp at {}", helper.display()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("scip-csharp exited with {}: {}", output.status, stderr);
            // Don't bail — partial output is acceptable per AGENTS.md spec
        }

        // Parse the JSON output
        let index_data = std::fs::read(&output_path)
            .with_context(|| format!("Failed to read symbol index at {}", output_path.display()))?;

        let index_result = scip_parse::parse_json_index(&index_data);

        // Cleanup temp file regardless of parse success
        let _ = std::fs::remove_file(&output_path);

        let index = index_result?;

        // Open LMDB and write
        let env = self.open_scip_env(db_path)?;
        let mut wtxn = env.write_txn()?;

        // Create/open databases — Str keys, Bytes values for symbol data
        let symbols_db: Database<Str, Bytes> =
            env.create_database(&mut wtxn, Some(SCIP_DB_NAME))?;

        // Meta DB: Str keys, Str values
        let meta_db: Database<Str, Str> =
            env.create_database(&mut wtxn, Some(SCIP_META_DB_NAME))?;

        // Clear previous data
        symbols_db.clear(&mut wtxn)?;

        // Write symbol references
        let mut total_refs = 0usize;
        let mut total_symbols = 0usize;

        for (symbol_name, references) in index.iter() {
            let stored: Vec<StoredReference> = references
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

        // ── Build position index ──────────────────────────────────────
        // scip_positions: "<file>:<line>" -> [symbol_keys]
        // Maps each definition occurrence to the symbols defined at that position.
        // NOTE: only `start_line` is indexed, so queries for lines in the *middle*
        // of a multi-line definition will not match. This is an intentional trade-off
        // for O(1) lookup — multi-line definitions are rare in C# (mostly constructors
        // with long signatures), and the start-line is the canonical anchor.
        let positions_db: Database<Str, Bytes> =
            env.create_database(&mut wtxn, Some(SCIP_POSITION_DB_NAME))?;
        positions_db.clear(&mut wtxn)?;

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

        tracing::debug!("scip-csharp position index: {} entries", positions.len());

        // ── Build simple-name index ───────────────────────────────────
        // scip_simple_names: simple_name -> [full_symbol_keys]
        // Enables O(1) fuzzy lookup by extracting the last segment of each symbol.
        let simple_names_db: Database<Str, Bytes> =
            env.create_database(&mut wtxn, Some(SCIP_SIMPLE_NAMES_DB_NAME))?;
        simple_names_db.clear(&mut wtxn)?;

        let mut simple_names: HashMap<String, Vec<String>> = HashMap::new();
        for (symbol_name, _references) in index.iter() {
            let simple = extract_simple_name(symbol_name);
            if !simple.is_empty() {
                simple_names
                    .entry(simple)
                    .or_default()
                    .push(symbol_name.clone());
            }
        }

        for (key, keys) in &simple_names {
            let bytes = serialize_keys_v1(keys)
                .with_context(|| format!("Failed to serialize simple name key: {}", key))?;
            simple_names_db.put(&mut wtxn, key.as_str(), &bytes)?;
        }

        tracing::debug!(
            "scip-csharp simple-name index: {} entries",
            simple_names.len()
        );

        // Write metadata as string values
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
            "scip-csharp rebuild complete: {} symbols, {} refs in {}ms",
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
        let rtxn = env.read_txn()?;

        let symbols_db: Database<Str, Bytes> = env
            .open_database(&rtxn, Some(SCIP_DB_NAME))?
            .ok_or_else(|| {
                anyhow::anyhow!("SCIP symbol database not found. Run a rebuild first.")
            })?;

        // Exact match first
        if let Some(bytes) = symbols_db.get(&rtxn, symbol)? {
            let stored = deserialize_refs(bytes)?;
            return Ok(stored
                .into_iter()
                .map(|r| SymbolReference {
                    file: r.file,
                    start_line: r.start_line,
                    end_line: r.end_line,
                    kind: r.kind,
                })
                .collect());
        }

        // Fuzzy via simple-name index (O(1) lookup instead of full-table scan)
        let simple_names_db: Database<Str, Bytes> =
            match env.open_database(&rtxn, Some(SCIP_SIMPLE_NAMES_DB_NAME))? {
                Some(db) => db,
                None => return Ok(vec![]),
            };

        let simple = extract_simple_name(symbol);
        let candidates: Vec<String> = match simple_names_db.get(&rtxn, &simple as &str)? {
            Some(b) => deserialize_keys_v1(b)?,
            None => return Ok(vec![]),
        };

        // Filter candidates through fuzzy_symbol_match as safety net,
        // then pick shortest (most specific) key.
        let chosen = candidates
            .iter()
            .filter(|k| fuzzy_symbol_match(symbol, k))
            .min_by_key(|k| k.len())
            .cloned();
        drop(rtxn);

        match chosen {
            Some(k) => self.find_references(db_path, &k),
            None => Ok(vec![]),
        }
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

    fn is_available(&self) -> bool {
        self.detect_helper().is_some()
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
            extract_simple_name("csharp SmallSolution.Library . Calculator#Add(int, int)."),
            "Add"
        );
        assert_eq!(
            extract_simple_name("csharp . . . Namespace.TopLevel"),
            "TopLevel"
        );
        assert_eq!(extract_simple_name("csharp . . . Foo"), "Foo");
        assert_eq!(extract_simple_name(""), "");
    }
}
