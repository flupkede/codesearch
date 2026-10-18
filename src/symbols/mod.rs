//! Symbol-aware reference lookups for codesearch.
//!
//! This module provides per-language symbol indexing behind a uniform
//! `SymbolIndexer` trait. The MVP ships a C# adapter (`csharp.rs`) that
//! invokes a bundled Roslyn-based helper and parses its SCIP output.
//!
//! Future languages (Python, TypeScript, Rust, etc.) register additional
//! `SymbolIndexer` impls here.

pub mod csharp;
pub mod resident;
pub mod scip_parse;
pub mod scip_proto;
pub mod typescript;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── Helper rendering ──────────────────────────────────────────────

/// Platform-stable rendering of a helper process's exit status.
///
/// `ExitStatus`'s `Display` prints `exit code: N` on Windows but
/// `exit status: N` on Unix — text that is persisted into index meta
/// tables and asserted by tests must not depend on the host OS (the
/// Linux CI red after #238 was exactly this drift). Signal-terminated
/// processes (Unix `code() == None`) have no portable code and fall
/// back to the platform's own rendering.
pub(crate) fn exit_status_text(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code: {code}"),
        None => status.to_string(),
    }
}

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
    /// The query as asked — a symbol name, `file:line`, or explicit key.
    pub symbol: String,
    /// The canonical SCIP key actually selected, e.g.
    /// `csharp . . . FieldDefinition#Validate().` Present only when the
    /// query resolved to exactly one stored symbol; absent on an ambiguous
    /// answer (a separate envelope) or when nothing matched. The selector
    /// the issue-specified contract turns on: echo is `symbol`, identity
    /// is this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_symbol: Option<String>,
    /// Resolved references.
    pub references: Vec<SymbolReference>,
    /// Completeness warnings for this answer. Non-empty means the
    /// reference list may be INCOMPLETE — a helper failure was survived
    /// rather than fatal (a project that would not compile, an exception
    /// during reference resolution, a non-zero scip-typescript exit) and
    /// the partial result was kept, OR the index itself was built out of a
    /// partially loaded solution (index-level rebuild warnings, e.g.
    /// MSBuild project-load failures; see `SymbolIndexer::index_warnings`).
    /// Entries name what failed. Omitted when empty: absent means the
    /// answer is as complete as the index knows.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Seconds since the symbol index was last rebuilt. `u64::MAX` is the
    /// internal "age unknown" sentinel `index_age` returns when the index
    /// cannot be opened or read; over the wire it serializes as null.
    #[serde(serialize_with = "serialize_index_age")]
    pub index_age_seconds: u64,
    /// Language that produced this result.
    pub language: String,
    /// Scope that was searched, e.g. `"project:example-org"`.
    pub scope: String,
    /// Git HEAD sha the symbol index was built for, when recorded. Compare
    /// with `current_head_sha` to spot index drift after a branch switch.
    /// Drift is surfaced, never auto-reindexed (deliberate — see
    /// `SymbolIndexer::index_head_sha`). Omitted when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_head_sha: Option<String>,
    /// Repository HEAD at response time, when readable. Omitted when the
    /// read fails (git unavailable, transient Windows handle race — must
    /// never fail the response).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_head_sha: Option<String>,
    /// Per-language symbol-index status for the searched repo (applicable
    /// languages only) — lets a consumer judge `references: []` against the
    /// state of the index that produced it (`absent`/`stale`/
    /// `restore-pending` mean the empty answer reflects the index, not the
    /// code) without a second round-trip to repo info. Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbol_index: Vec<SymbolIndexStatus>,
}

/// Serialize `index_age_seconds`, mapping the internal unknown-age sentinel
/// (`u64::MAX`) to JSON null — a consumer must not see
/// `18446744073709551615` and mistake it for an age. Real ages stay numbers.
fn serialize_index_age<S>(age: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if *age == u64::MAX {
        serializer.serialize_none()
    } else {
        serializer.serialize_u64(*age)
    }
}

/// The deserialize half of [`serialize_index_age`]: a JSON null (an
/// unknown-age index on the wire) lands back in the `u64::MAX` sentinel.
fn deserialize_index_age<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<u64>::deserialize(deserializer)?.unwrap_or(u64::MAX))
}

/// A `find_impact` query in the form the adapters resolve it.
///
/// The three input variants of `FindImpactRequest` map onto this one-to-one;
/// `ExactKey` is the explicit selection a caller makes after an ambiguous
/// answer (or with any full key it already has).
#[derive(Debug, Clone)]
pub enum ImpactQuery {
    /// Look up this exact canonical SCIP key. No fuzzy fallback: a key
    /// that is not in the index is a `KeyMatch::NotFound`, never a guess.
    ExactKey(String),
    /// Resolve a possibly-fuzzy symbol name (e.g. `Validate`,
    /// `FieldDefinition.Validate`) to one canonical key.
    Name(String),
    /// Resolve the symbol defined at a file position (1-based line).
    Position { file: PathBuf, line: u32 },
}

/// Outcome of resolving an [`ImpactQuery`] against the index.
///
/// Resolution is a plain LMDB read — fast, never invoking the SCIP
/// helper. Ambiguity is surfaced instead of silently picking the
/// shortest candidate (the pre-fix behaviour hid overloads this way).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyMatch {
    /// Exactly one stored symbol matches; this is the selection.
    Resolved(String),
    /// Several stored symbols match and the query alone cannot choose.
    /// Candidates are sorted and deduplicated — re-query with
    /// `ImpactQuery::ExactKey` for one of them.
    Ambiguous(Vec<String>),
    /// Nothing in the index matches.
    NotFound,
}

/// Typed answer for an ambiguous `find_impact` query, serialized as JSON
/// in the tool-result text (same convention as [`SymbolLookupBusy`]).
///
/// The MCP client re-calls with `symbol_key` set to one of `candidates`;
/// the server never silently picks on the client's behalf.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolAmbiguity {
    /// Always `true` — machine-branchable, like `busy`.
    pub ambiguous: bool,
    /// The query as asked.
    pub query: String,
    /// Every canonical key that matches, sorted. These are exact
    /// `symbol_key` values, not prose.
    pub candidates: Vec<String>,
    /// Actionable hint for the agent.
    pub hint_for_agent: String,
}

/// Resolve the repository HEAD sha for `repo_root` (40-hex), or `None`
/// when git is unavailable, the path is not a work tree, or the read
/// fails. Non-fatal BY DESIGN: a transient Windows git failure (see the
/// `GitHeadWatcher` note in `src/watch/mod.rs`) must never take down a
/// response that only wanted the fingerprint.
pub(crate) fn current_git_head(repo_root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        // Repo discovery must key off repo_root alone: a caller running under
        // git (e.g. a push hook) exports GIT_DIR/GIT_WORK_TREE, which would
        // otherwise override discovery and report the WRONG repo's HEAD.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let is_40_hex = sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit());
    is_40_hex.then_some(sha)
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
    /// Machine-branchable retry interval in seconds, mirroring the prose in
    /// `advice`: how long a harness with auto-retry semantics should sleep
    /// before repeating the SAME call. Busy is progress, not failure.
    pub retry_after_seconds: u64,
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
    /// Index-level context (heal running, partially loaded index): without
    /// it the failure would advise an action that is already in progress.
    /// Absent from the wire when empty — the historical failure shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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
            warnings: Vec::new(),
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
            warnings: Vec::new(),
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
            env.create_database::<heed::types::Str, heed::types::Bytes>(
                &mut wtxn,
                Some(crate::constants::SCIP_REF_WARNINGS_DB_NAME),
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
    /// Index-level completeness warnings from this build (build-environment
    /// failures the helper survived; see [`SymbolIndexer::index_warnings`]).
    /// Empty = built clean. Drives the TUI's degraded indicator.
    pub index_warnings: Vec<String>,
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

    /// Resolve a `find_impact` query to canonical SCIP key(s), exposing
    /// ambiguity instead of silently choosing among candidates.
    ///
    /// Plain LMDB reads only — never invokes the SCIP helper, so callers
    /// can (and the find_impact handler does) run this BEFORE the
    /// budget-tracked reference fetch: an ambiguous answer is instant.
    /// `KeyMatch::Resolved` is the selected canonical identity; the
    /// historical behaviour picked the shortest fuzzy match silently,
    /// which hid overloads from the caller.
    fn resolve_query(&self, db_path: &Path, query: &ImpactQuery) -> Result<KeyMatch>;

    /// Return the symbol's references for one EXACT canonical key.
    ///
    /// This is the expensive half of a lookup: on the C# adapter a cold
    /// reference cache triggers the `scip-csharp find-refs` subprocess
    /// (minutes on a large solution), which is why the find_impact
    /// handler budgets and tracks THIS call and not `resolve_query`.
    fn find_references_for_key(
        &self,
        db_path: &Path,
        canonical_key: &str,
    ) -> Result<Vec<SymbolReference>>;

    /// Completeness warnings for one canonical key's stored answer — a
    /// plain LMDB read, NEVER invoking a helper. Non-empty means the
    /// references `find_references_for_key` returns for this key may be
    /// INCOMPLETE: a partial helper run was survived and cached, and the
    /// warnings were persisted with it so the partial answer can never
    /// pass for complete. Entries name what failed.
    fn lookup_warnings(&self, db_path: &Path, canonical: &str) -> Vec<String>;

    /// How old is the current symbol index (seconds since last rebuild)?
    fn index_age(&self, db_path: &Path) -> u64;

    /// Index-level completeness warnings from the last rebuild — build
    /// environment failures the language helper survived while still
    /// producing an index (e.g. MSBuild project-load failures for C#).
    /// Non-empty means reference lists from this index may be incomplete
    /// EVEN WHEN a lookup succeeds, and callers should attach these
    /// warnings to answers served from this index. Plain read; empty is
    /// the default for adapters that do not track build-environment state.
    fn index_warnings(&self, _db_path: &Path) -> Vec<String> {
        Vec::new()
    }

    /// True while this index's rebuild is waiting out a still-running NuGet
    /// restore (C# restore gate, issue #225). Callers attach one
    /// caller-facing warning to answers served from the previous index;
    /// the flag lifts when the retry loop ends either way. Default false —
    /// adapters without a restore gate are never pending.
    fn restore_pending(&self, _db_path: &Path) -> bool {
        false
    }

    /// Which codesearch build produced the current symbol index
    /// (`scip_meta[index_builder_version]`). `None` means absent or
    /// unreadable — the C# rebuild gate treats that as stale, so a newly
    /// deployed binary rebuilds the indexes it inherited itself at startup
    /// instead of serving an index it never produced.
    fn index_builder_version(&self, _db_path: &Path) -> Option<String> {
        None
    }

    /// Whether the current symbol index was produced by THIS build — the
    /// same freshness check the rebuild gate applies before serving without
    /// a rebuild (absent stamp or a stamp from another build ⇒ stale).
    /// Default `true`: adapters that do not stamp their indexes are never
    /// stale by builder alone (there is no stamp to compare against).
    fn index_matches_builder(&self, _db_path: &Path) -> bool {
        true
    }

    /// The git HEAD sha the current symbol index was built for, when
    /// recorded. `None` means unknown (pre-fingerprint index, or git was
    /// unreadable at build time). Compare with the repository's current
    /// HEAD to make index drift after a branch switch visible — drift is
    /// surfaced, never auto-reindexed (deliberate: reindexing a large
    /// solution on every branch switch would thrash; refresh stays an
    /// explicit operator action).
    fn index_head_sha(&self, _db_path: &Path) -> Option<String> {
        None
    }

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

    /// Why `applies_to` would refuse a repository, in one clause. Surfaced
    /// by find_impact's missing-index warning so a repo that can never get
    /// a symbol index says so — instead of promising a rebuild that will
    /// not happen. Adapters override next to their `applies_to`.
    fn applicability_hint(&self) -> &'static str {
        "the repository has no entrypoint this symbol indexer recognizes"
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

/// One symbol language's index status for a repository — the on-disk truth
/// the rebuild gates act on, composed from plain meta reads (never a
/// helper). Info surfaces (REST `/repos/:alias/info`, both TUI Info
/// overlays) and `find_impact` answers carry it so a consumer can judge an
/// answer against the state of the index that produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolIndexStatus {
    /// Symbol language this entry describes (e.g. `csharp`).
    pub language: String,
    /// `ready` | `degraded` | `stale` | `restore-pending` | `absent` |
    /// `not-applicable`.
    pub state: String,
    /// Seconds since the last rebuild; the unknown-age sentinel
    /// (`u64::MAX`) serializes as null and a null payload deserializes back
    /// into the sentinel — the remote TUI consumes REST JSON where an
    /// unreadable index arrives as null.
    #[serde(
        serialize_with = "serialize_index_age",
        deserialize_with = "deserialize_index_age"
    )]
    pub age_seconds: u64,
    /// The repo lacks this language's entrypoint — no index is expected
    /// here. `find_impact` filters these out; info surfaces keep them so
    /// the operator sees why a language is missing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub not_applicable: bool,
}

/// Compose [`SymbolIndexStatus`] for every registered symbol language, in
/// registration order. Priority when several states apply:
/// `restore-pending` (a live gate) > `stale` (a rebuild is imminent) >
/// `degraded` (serving but incomplete) > `ready`.
pub fn symbol_index_status(
    registry: &SymbolIndexerRegistry,
    project_path: &Path,
    db_path: &Path,
) -> Vec<SymbolIndexStatus> {
    registry
        .available_languages()
        .iter()
        .filter_map(|lang| registry.get(lang))
        .map(|indexer| {
            let age = indexer.index_age(db_path);
            let base = SymbolIndexStatus {
                language: indexer.language().to_string(),
                state: String::new(),
                age_seconds: age,
                not_applicable: false,
            };
            if !indexer.applies_to(project_path) {
                return SymbolIndexStatus {
                    state: "not-applicable".to_string(),
                    not_applicable: true,
                    ..base
                };
            }
            let state = if !indexer.has_index(db_path) {
                "absent"
            } else if indexer.restore_pending(db_path) {
                "restore-pending"
            } else if !indexer.index_matches_builder(db_path) {
                "stale"
            } else if !indexer.index_warnings(db_path).is_empty() {
                "degraded"
            } else {
                "ready"
            };
            SymbolIndexStatus {
                state: state.to_string(),
                ..base
            }
        })
        .collect()
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

/// Lock-visibility verification (todo #96 step 5): readers never block on
/// an open write transaction — the property that lets `find_impact` serve
/// during an indexer rebuild without needing a `lock_status` in the busy
/// envelope. Sibling `_tests.rs` file per repo convention.
#[cfg(test)]
#[path = "lock_visibility_tests.rs"]
mod lock_visibility_tests;

/// Resident-helper WorkspacePool tests (todo #115). Sibling `_tests.rs`
/// file per repo convention.
#[cfg(test)]
#[path = "resident_tests.rs"]
mod resident_tests;

/// Helper stderr routing tests (`csharp.rs`). Sibling `_tests.rs` file
/// per repo convention.
#[cfg(test)]
#[path = "csharp_tests.rs"]
mod csharp_tests;
