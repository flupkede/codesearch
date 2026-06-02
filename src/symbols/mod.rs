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

use std::path::{Path, PathBuf};

use anyhow::Result;
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
            indexers: vec![Box::new(csharp::CSharpSymbolIndexer::new())],
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
