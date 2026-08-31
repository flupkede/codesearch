//! Symbol-aware reference lookups for codesearch.
//!
//! This module provides per-language symbol indexing behind a uniform
//! `SymbolIndexer` trait. The MVP ships a C# adapter (`csharp.rs`) that
//! invokes a bundled Roslyn-based helper and parses its SCIP output.
//!
//! Future languages (Python, TypeScript, Rust, etc.) register additional
//! `SymbolIndexer` impls here.

pub mod csharp;
pub mod scip_parse;
pub mod scip_proto;
pub mod typescript;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── Common types ──────────────────────────────────────────────────

/// A resolved reference to a symbol — file, line range, and kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolReference {
    /// File path relative to the project root.
    pub file: PathBuf,
    /// 1-based start line.
    pub start_line: u32,
    /// 1-based end line (inclusive).
    pub end_line: u32,
    /// Reference kind: `"definition"`, `"call"`, `"import"`, `"implementation"`, etc.
    pub kind: String,
}

/// Result of a `find_impact` query.
#[derive(Debug, Clone, Serialize)]
pub struct FindImpactResult {
    /// Canonical SCIP symbol string, e.g. `csharp . . . FieldDefinition#Validate().`
    pub symbol: String,
    /// Resolved references.
    pub references: Vec<SymbolReference>,
    /// Seconds since the symbol index was last rebuilt.
    pub index_age_seconds: u64,
    /// Language that produced this result.
    pub language: String,
    /// Scope that was searched, e.g. `"project:example-org"`.
    pub scope: String,
}

/// Error returned when the symbol index is unavailable.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolIndexError {
    /// Human-readable error.
    pub error: String,
    /// Languages that have a registered adapter (may not have an index).
    pub available_languages: Vec<String>,
    /// Suggestion for the agent.
    pub hint_for_agent: String,
}

/// Structured busy answer returned when a `find_impact` lookup exceeds its
/// internal wall-clock budget.
///
/// The MCP client must never be the timeout mechanism: when the budget
/// overruns, the server answers with this envelope (serialized as JSON in
/// the tool-result text) while the lookup keeps running in the background.
/// A caller can branch on `busy == true` instead of parsing an opaque
/// client-side timeout, and the retry hinted in `advice` is served from the
/// reference cache the running lookup will have populated.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolLookupBusy {
    /// Always `true`; makes the envelope self-describing.
    pub busy: bool,
    /// What is still running, e.g. `"resolving 'Ns.I.M' via the csharp SCIP helper"`.
    pub state: String,
    /// Wall-clock time the request waited before the budget overran. On the
    /// retry answer for a tracked lookup this is the background lookup's
    /// cumulative elapsed time instead.
    pub waited_ms: u64,
    /// Actionable retry hint, e.g. `"retry the same call in ~60s"`.
    pub advice: String,
}

/// Machine-branchable class of a failed `find_impact` lookup.
///
/// `busy` needs no class here: an overran lookup answers with the typed
/// `SymbolLookupFailure`-free `SymbolLookupBusy` envelope instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolLookupFailureClass {
    /// The helper ran and failed (non-zero exit, crash, unusable output)
    /// against a readable index. Retrying cannot succeed until the cause
    /// changes; fall back to text search.
    Failed,
    /// No readable symbol index (`index_age` reports unknown): retrying
    /// the lookup cannot succeed until the index is built or rebuilt.
    Stale,
}

/// Typed failure envelope for `find_impact` lookups, serialized as JSON in
/// the tool-result text (same convention as `SymbolIndexError`). Replaces
/// the former soft-string render (`Symbol lookup failed: ...`) so an agent
/// can branch on `class` instead of parsing prose.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolLookupFailure {
    /// Rendered `{:#}` error chain.
    pub error: String,
    /// Machine-branchable failure class.
    pub class: SymbolLookupFailureClass,
    /// Actionable hint for the agent.
    pub hint_for_agent: String,
}

impl SymbolLookupFailure {
    /// A failed lookup against a readable index.
    pub fn failed(error_chain: impl Into<String>) -> Self {
        Self {
            error: error_chain.into(),
            class: SymbolLookupFailureClass::Failed,
            hint_for_agent: "The SCIP helper failed for this lookup. Do not retry the same call \
                             immediately; fall back to `find` with kind=\"usages\" (text-based) \
                             and/or reindex the project to rebuild the symbol index."
                .to_string(),
        }
    }

    /// A failed lookup with an unreadable/absent symbol index.
    pub fn stale(error_chain: impl Into<String>) -> Self {
        Self {
            error: error_chain.into(),
            class: SymbolLookupFailureClass::Stale,
            hint_for_agent: "No readable symbol index for this project. Build or rebuild it \
                             first (`codesearch index` / `index reindex`), then retry the \
                             same call."
                .to_string(),
        }
    }

    /// Classify a lookup failure by the index age reported for the same db:
    /// an unknown age (`u64::MAX`, what `index_age` returns whenever the
    /// index cannot be opened or read) means stale; anything else means the
    /// index was readable and the lookup itself failed.
    pub fn classify(error_chain: impl Into<String>, index_age_seconds: u64) -> Self {
        if index_age_seconds == u64::MAX {
            Self::stale(error_chain)
        } else {
            Self::failed(error_chain)
        }
    }
}

/// Which files/projects to reindex.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum RebuildScope {
    /// Reindex the entire solution/project tree.
    Full,
    /// Reindex a single project (e.g. one `.csproj`).
    Project(PathBuf),
    /// Incremental per-group rebuild for changed `.cs` files.
    ///
    /// `changed` — files that were modified/created; used to pick the `.csproj` to rebuild.
    /// `deleted` — files that were deleted; absent from the new index so their LMDB entries
    ///   must be explicitly included in `affected_files` for clean-up.
    Files {
        /// Modified or created `.cs` files.
        changed: Vec<PathBuf>,
        /// Deleted `.cs` files (not present in the new index output).
        deleted: Vec<PathBuf>,
    },
}

// ── Shared SCIP environment ──────────────────────────────────────

/// Open (or reuse) the process-wide shared SCIP LMDB environment for `db_path`.
///
/// Both the C# and TypeScript adapters store symbol data in the same
/// `db_path/scip` directory, and LMDB allows exactly ONE open environment per
/// directory per process. Historically every operation opened its own
/// short-lived env, so two overlapping operations — e.g. a watcher-triggered
/// rebuild starting while a lazy `find-refs` call held its env for minutes —
/// tripped the double-open guard and one side failed outright
/// (`LMDB double-open prevented`, surfaced as a red `C#!` in the TUI).
/// Routing every open through this getter hands all concurrent users the SAME
/// environment: writers serialise on LMDB's single-writer mutex, readers never
/// block, and the double-open error class cannot occur.
pub(crate) fn get_shared_scip_env(
    db_path: &Path,
) -> Result<std::sync::Arc<crate::lmdb_registry::TrackedEnv>> {
    let scip_dir = db_path.join("scip");
    std::fs::create_dir_all(&scip_dir)
        .with_context(|| format!("Failed to create SCIP directory: {}", scip_dir.display()))?;

    crate::lmdb_registry::get_or_open_shared_env(
        &scip_dir,
        &format!("SCIP({})", db_path.display()),
        |opts| {
            // map_size is virtual address space (not RSS); the OS only faults
            // in written pages. Read once per env lifetime.
            let map_size_mb = std::env::var(crate::constants::SCIP_LMDB_MAP_SIZE_MB_ENV)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(crate::constants::SCIP_LMDB_DEFAULT_MAP_SIZE_MB);
            opts.map_size(map_size_mb * 1024 * 1024).max_dbs(10);
            // SAFETY: `NO_TLS` only changes reader-slot tracking. See `BASE_ENV_FLAGS`.
            unsafe { opts.flags(crate::lmdb_registry::BASE_ENV_FLAGS) };
        },
        |env| {
            // Pre-create every named database (both languages') exactly once
            // per env session: LMDB requires named DBs to exist before they
            // can be opened in read txns.
            let mut wtxn = env.write_txn()?;
            env.create_database::<heed::types::Str, heed::types::Bytes>(
                &mut wtxn,
                Some(crate::constants::SCIP_SYMBOLS_DB_NAME),
            )?;
            env.create_database::<heed::types::Str, heed::types::Str>(
                &mut wtxn,
                Some(crate::constants::SCIP_META_DB_NAME),
            )?;
            env.create_database::<heed::types::Str, heed::types::Bytes>(
                &mut wtxn,
                Some(crate::constants::SCIP_POSITION_DB_NAME),
            )?;
            env.create_database::<heed::types::Str, heed::types::Bytes>(
                &mut wtxn,
                Some(crate::constants::SCIP_SIMPLE_NAMES_DB_NAME),
            )?;
            env.create_database::<heed::types::Str, heed::types::Bytes>(
                &mut wtxn,
                Some(crate::constants::SCIP_REF_CACHE_DB_NAME),
            )?;
            wtxn.commit()?;
            Ok(())
        },
    )
}

/// Summary returned after a rebuild completes.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RebuildSummary {
    /// Number of symbols indexed.
    pub symbols_indexed: usize,
    /// Number of references stored.
    pub references_stored: usize,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Summary returned after a Phase 3 pre-warm completes.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PrewarmSummary {
    /// Total number of uncached symbols available.
    pub total_symbols: usize,
    /// Number of symbols resolved (may be less than total if capped).
    pub resolved: usize,
    /// Number of symbols successfully cached.
    pub cached: usize,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

// ── Trait ─────────────────────────────────────────────────────────

/// Per-language symbol indexer.
///
/// Implementations discover a language-specific helper (if bundled),
/// invoke it to produce a SCIP index, parse that index into LMDB, and
/// answer reference queries.
pub trait SymbolIndexer: Send + Sync {
    /// Language identifier (e.g. `"csharp"`).
    fn language(&self) -> &str;

    /// Run the indexer for this language over the repo. Writes results to LMDB.
    /// Idempotent: safe to re-run after file changes.
    #[allow(dead_code)]
    fn rebuild(
        &self,
        repo_path: &Path,
        db_path: &Path,
        scope: RebuildScope,
    ) -> Result<RebuildSummary>;

    /// Return the symbol's references from the LMDB store.
    fn find_references(&self, db_path: &Path, symbol: &str) -> Result<Vec<SymbolReference>>;

    /// Look up references by file-position instead of symbol name.
    /// Resolves the position to a canonical SCIP symbol first.
    fn find_references_by_position(
        &self,
        db_path: &Path,
        file: &Path,
        line: u32,
    ) -> Result<Vec<SymbolReference>>;

    /// How old is the current symbol index (seconds since last rebuild)?
    fn index_age(&self, db_path: &Path) -> u64;

    /// Whether the helper binary for this language is available.
    fn is_available(&self) -> bool;

    /// Whether a symbol index exists for the given database path.
    /// Returns `true` if the LMDB symbol tables have been populated.
    fn has_index(&self, db_path: &Path) -> bool;

    /// Whether this indexer is applicable to the given repo.
    ///
    /// Returns `false` when the repo lacks the language-specific entrypoint
    /// (e.g. no `.sln`/`.csproj` for the C# indexer). Callers should skip
    /// `rebuild()` — and avoid setting any error status — when this returns
    /// `false`, so non-applicable repos don't get flagged red in the TUI.
    ///
    /// Default: `true` (assume applicable). Adapters override when they have
    /// a cheap, deterministic applicability test.
    fn applies_to(&self, _repo_path: &Path) -> bool {
        true
    }

    /// Downcast to `Any` for concrete-type method access (e.g. `prewarm_ref_cache`).
    ///
    /// This is needed because some adapter-specific methods (like Phase 3 pre-warm)
    /// don't belong on the generic trait but still need to be called from serve code
    /// that holds a `&dyn SymbolIndexer`.
    fn as_any(&self) -> &dyn std::any::Any;
}

// ── Language dispatch ─────────────────────────────────────────────

/// Registry of all known per-language symbol indexers.
pub struct SymbolIndexerRegistry {
    indexers: Vec<Box<dyn SymbolIndexer>>,
}

impl SymbolIndexerRegistry {
    /// Create a registry with default (MVP) indexers.
    pub fn new() -> Self {
        Self {
            indexers: vec![
                Box::new(csharp::CSharpSymbolIndexer::new()),
                Box::new(typescript::TypeScriptSymbolIndexer::new()),
            ],
        }
    }

    /// Look up the indexer for a given language.
    pub fn get(&self, language: &str) -> Option<&dyn SymbolIndexer> {
        self.indexers
            .iter()
            .find(|i| i.language().eq_ignore_ascii_case(language))
            .map(|b| b.as_ref())
    }

    /// List languages that have a registered adapter.
    pub fn available_languages(&self) -> Vec<String> {
        self.indexers
            .iter()
            .map(|i| i.language().to_string())
            .collect()
    }

    /// List languages where the helper is actually installed.
    pub fn installed_languages(&self) -> Vec<String> {
        self.indexers
            .iter()
            .filter(|i| i.is_available())
            .map(|i| i.language().to_string())
            .collect()
    }

    /// Check whether a specific language has a built index for the given db_path.
    pub fn has_index_for(&self, language: &str, db_path: &Path) -> bool {
        self.get(language)
            .map(|i| i.has_index(db_path))
            .unwrap_or(false)
    }

    /// List languages that have a built index for the given db_path.
    #[allow(dead_code)]
    pub fn indexed_languages(&self, db_path: &Path) -> Vec<String> {
        self.indexers
            .iter()
            .filter(|i| i.has_index(db_path))
            .map(|i| i.language().to_string())
            .collect()
    }
}

impl Default for SymbolIndexerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scip getter must hand out ONE shared env per `db_path` — the exact
    /// property that stops a rebuild and an in-flight lazy find-refs (or the
    /// TypeScript adapter) from failing each other with the double-open error.
    #[test]
    fn shared_scip_env_is_shared_across_calls() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("codesearch.db");

        let env1 = get_shared_scip_env(&db_path).unwrap();
        let env2 = get_shared_scip_env(&db_path).unwrap();
        assert!(std::sync::Arc::ptr_eq(&env1, &env2));
    }
}
