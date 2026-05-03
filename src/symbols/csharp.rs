//! C# symbol indexer adapter.
//!
//! Detects the `scip-csharp` helper binary, invokes it as a subprocess
//! to produce a SCIP protobuf index from a .sln/.csproj, parses the
//! output, and stores references in LMDB.

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
const SCIP_DB_NAME: &str = "scip_symbols";

/// LMDB database name for the rebuild timestamp.
const SCIP_META_DB_NAME: &str = "scip_meta";

/// Key in the meta database that stores the last rebuild timestamp (UNIX epoch seconds).
const META_REBUILD_TS: &str = "last_rebuild_ts";

/// Key in the meta database storing the count of indexed symbols.
#[allow(dead_code)]
const META_SYMBOL_COUNT: &str = "symbol_count";

/// Environment variable override for the helper binary path.
const HELPER_ENV_VAR: &str = "CODESEARCH_SCIP_CSHARP";

/// Helper binary name (without extension).
const HELPER_BIN_NAME: &str = "scip-csharp";

/// Debounce period for .cs file changes (seconds).
#[allow(dead_code)]
pub const CSHARP_REBUILD_DEBOUNCE_SECS: u64 = 60;

// ── Serialized reference type (stored in LMDB via bincode) ────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredReference {
    file: PathBuf,
    start_line: u32,
    end_line: u32,
    kind: String,
}

// ── CSharpSymbolIndexer ───────────────────────────────────────────

/// C# adapter: locates the Roslyn helper, invokes it, parses SCIP, stores
/// references in LMDB.
pub struct CSharpSymbolIndexer {
    /// Cached path to the helper binary (None = not yet detected).
    helper_path: std::sync::Mutex<Option<PathBuf>>,
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
    /// Search order:
    /// 1. `CODESEARCH_SCIP_CSHARP` env var
    /// 2. `<codesearch-exe-dir>/helpers/csharp/scip-csharp[.exe]`
    /// 3. `$PATH` lookup
    pub fn detect_helper(&self) -> Option<PathBuf> {
        // Fast path: already detected
        {
            let lock = self.helper_path.lock().unwrap();
            if lock.is_some() {
                return lock.clone();
            }
        }

        let resolved = self.resolve_helper_path();
        let mut lock = self.helper_path.lock().unwrap();
        *lock = resolved.clone();
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
                let local_path = exe_dir.join("helpers").join("csharp").join(&bin_name);
                if local_path.exists() {
                    tracing::debug!("scip-csharp helper found at {}", local_path.display());
                    return Some(local_path);
                }
            }
        }

        // 3. $PATH lookup (use `which` on Unix, `where` on Windows)
        let lookup_cmd = if cfg!(windows) { "where" } else { "which" };
        if let Ok(output) = Command::new(lookup_cmd)
            .arg(HELPER_BIN_NAME)
            .output()
        {
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
                .max_dbs(4)
                .open(&scip_dir)?
        };
        Ok(env)
    }
}

impl SymbolIndexer for CSharpSymbolIndexer {
    fn language(&self) -> &str {
        "csharp"
    }

    fn rebuild(&self, repo_path: &Path, db_path: &Path, scope: RebuildScope) -> Result<RebuildSummary> {
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
                    anyhow::anyhow!(
                        "No .sln file found in {}",
                        repo_path.display()
                    )
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
            "index-{}-{:x}.scip",
            repo_path.file_name().unwrap_or_default().to_string_lossy(),
            start.elapsed().as_nanos()
        ));

        // Invoke helper
        let mut cmd = Command::new(&helper);
        cmd.arg("index")
            .arg("--solution").arg(&solution)
            .arg("--output").arg(&output_path);

        if let Some(ref proj) = project {
            cmd.arg("--project").arg(proj);
        }

        tracing::info!("Running scip-csharp: {:?}", cmd);

        let output = cmd.output()
            .with_context(|| format!("Failed to execute scip-csharp at {}", helper.display()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("scip-csharp exited with {}: {}", output.status, stderr);
            // Don't bail — partial output is acceptable per AGENTS.md spec
        }

        // Parse the SCIP output
        let scip_data = std::fs::read(&output_path)
            .with_context(|| format!("Failed to read SCIP output at {}", output_path.display()))?;

        let index = scip_parse::parse_scip(&scip_data)?;

        // Open LMDB and write
        let env = self.open_scip_env(db_path)?;
        let mut wtxn = env.write_txn()?;

        // Create/open databases — Str keys, Bytes values for symbol data
        let symbols_db: Database<Str, Bytes> = env
            .create_database(&mut wtxn, Some(SCIP_DB_NAME))?;

        // Meta DB: Str keys, Str values
        let meta_db: Database<Str, Str> = env
            .create_database(&mut wtxn, Some(SCIP_META_DB_NAME))?;

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

            let value_bytes = bincode::serialize(&stored)
                .with_context(|| format!("Failed to serialize references for {}", symbol_name))?;

            symbols_db.put(&mut wtxn, symbol_name.as_str(), &value_bytes)?;
            total_refs += stored.len();
            total_symbols += 1;
        }

        // Write metadata as string values
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        meta_db.put(&mut wtxn, META_REBUILD_TS, now.to_string().as_str())?;
        meta_db.put(&mut wtxn, META_SYMBOL_COUNT, total_symbols.to_string().as_str())?;

        wtxn.commit()?;

        // Cleanup temp file
        let _ = std::fs::remove_file(&output_path);

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
            .ok_or_else(|| anyhow::anyhow!("SCIP symbol database not found. Run a rebuild first."))?;

        // Exact match first
        if let Some(bytes) = symbols_db.get(&rtxn, symbol)? {
            let stored: Vec<StoredReference> = bincode::deserialize(bytes)
                .with_context(|| "Failed to deserialize stored references")?;
            return Ok(stored.into_iter().map(|r| SymbolReference {
                file: r.file,
                start_line: r.start_line,
                end_line: r.end_line,
                kind: r.kind,
            }).collect());
        }

        // Fuzzy match: try to find a symbol that contains the query
        let mut candidates = Vec::new();
        let iter = symbols_db.iter(&rtxn)?;
        for result in iter {
            let (key, value) = result?;
            // Check if the symbol name is contained in the key
            if key.contains(symbol) || fuzzy_symbol_match(symbol, key) {
                let stored: Vec<StoredReference> = bincode::deserialize(value)
                    .with_context(|| "Failed to deserialize stored references")?;
                let refs: Vec<SymbolReference> = stored.into_iter().map(|r| SymbolReference {
                    file: r.file,
                    start_line: r.start_line,
                    end_line: r.end_line,
                    kind: r.kind,
                }).collect();
                candidates.push((key.to_string(), refs));
            }
        }

        // Return the best (shortest key = most specific) match
        candidates.sort_by(|a, b| a.0.len().cmp(&b.0.len()));
        Ok(candidates.into_iter().next().map(|(_, refs)| refs).unwrap_or_default())
    }

    fn find_references_by_position(&self, db_path: &Path, file: &Path, line: u32) -> Result<Vec<SymbolReference>> {
        let env = self.open_scip_env(db_path)?;
        let rtxn = env.read_txn()?;

        let symbols_db: Database<Str, Bytes> = env
            .open_database(&rtxn, Some(SCIP_DB_NAME))?
            .ok_or_else(|| anyhow::anyhow!("SCIP symbol database not found. Run a rebuild first."))?;

        // Walk all symbols looking for one defined at the given position
        let file_str = file.to_string_lossy();
        let mut best_match: Option<(&str, Vec<SymbolReference>)> = None;

        let iter = symbols_db.iter(&rtxn)?;
        for result in iter {
            let (key, value) = result?;
            let stored: Vec<StoredReference> = match bincode::deserialize(value) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Check if any reference in this symbol matches the file+line
            for r in &stored {
                if r.file.to_string_lossy() == *file_str
                    && r.kind == "definition"
                    && line >= r.start_line
                    && line <= r.end_line
                {
                    let refs: Vec<SymbolReference> = stored.into_iter().map(|r| SymbolReference {
                        file: r.file,
                        start_line: r.start_line,
                        end_line: r.end_line,
                        kind: r.kind,
                    }).collect();
                    // Prefer the most specific (shortest key) match
                    if best_match.as_ref().is_none_or(|(k, _)| key.len() < k.len()) {
                        best_match = Some((key, refs));
                    }
                    break;
                }
            }
        }

        Ok(best_match.map(|(_, refs)| refs).unwrap_or_default())
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

        let meta_db: Database<Str, Str> =
            match env.open_database(&rtxn, Some(SCIP_META_DB_NAME)) {
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
