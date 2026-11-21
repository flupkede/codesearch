//! Helper stderr routing tests (`csharp.rs`). Sibling `_tests.rs` file
//! per repo convention.

use super::csharp::tests::{build_fake_csharp_helper, ref_cache_keys, FAKE_REFERENCED};
use super::csharp::{
    csharp_relevance, drain_pipe_to_tracing, is_helper_warning_line, is_restore_pending,
    is_transient_filtered_solution, kill_live_helper_children, restore_pending_flag_clear,
    restore_pending_flag_set, CSharpSymbolIndexer, LiveHelperChild, Relevance, RestorePendingError,
    HELPER_RESTORE_PENDING_EXIT,
};
use super::helper_gate::HelperGate;
use super::resident::{ClientLike, ResidentRefs, UpdateOutcome, WorkspacePool};
use super::{RebuildScope, SymbolIndexer};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn drained(input: &[u8]) -> Vec<String> {
    let out: Mutex<Vec<String>> = Mutex::new(Vec::new());
    drain_pipe_to_tracing(Cursor::new(input.to_vec()), |line| {
        out.lock().expect("test mutex").push(line.to_string());
    });
    out.into_inner().expect("test mutex")
}

#[test]
fn drain_survives_bad_bytes_and_strips_eol() {
    let mut raw = b"first line\n".to_vec();
    raw.extend_from_slice(b"bad \xFF\xFE bytes\n"); // invalid UTF-8 mid-stream
    raw.extend_from_slice(b"crlf line\r\n");
    raw.extend_from_slice(b"no trailing newline");

    let lines = drained(&raw);

    // The defect this pins: lines().map_while(Result::ok) ended the drain
    // permanently at the non-UTF-8 line, silently dropping lines 3-4 and
    // eventually stalling the pipe. All four lines must come through.
    assert_eq!(lines.len(), 4, "drain stopped early: {lines:?}");
    assert_eq!(lines[0], "first line");
    assert!(
        lines[1].starts_with("bad ") && lines[1].ends_with(" bytes"),
        "expected lossy-decoded line, got: {:?}",
        lines[1]
    );
    assert_eq!(lines[2], "crlf line");
    assert_eq!(lines[3], "no trailing newline");
}

#[test]
fn drain_stops_at_eof_and_handles_empty_pipe() {
    assert!(drained(b"").is_empty());
    assert_eq!(drained(b"only\n"), vec!["only".to_string()]);
}

/// Read impl whose first read fails with `Interrupted` (EINTR), then
/// behaves like a normal pipe.
struct InterruptedOnce {
    armed: bool,
    inner: Cursor<Vec<u8>>,
}

impl std::io::Read for InterruptedOnce {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.armed {
            self.armed = false;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "eintr",
            ));
        }
        self.inner.read(buf)
    }
}

#[test]
fn drain_retries_after_transient_interrupted_read() {
    // Contract: a transient Interrupted read must not end the drain. std's
    // own read_until already retries Interrupted internally (rustc 1.98,
    // library/std/src/io/mod.rs), so the local arm in drain_pipe_to_tracing
    // is belt-and-braces and unreachable via its internal BufReader — this
    // test pins the observable contract, not that arm.
    let mut out: Vec<String> = Vec::new();
    drain_pipe_to_tracing(
        InterruptedOnce {
            armed: true,
            inner: Cursor::new(b"before\nafter\n".to_vec()),
        },
        |line| out.push(line.to_string()),
    );
    assert_eq!(out, vec!["before".to_string(), "after".to_string()]);
}

#[test]
fn helper_warning_classification_table() {
    let cases: [(&str, bool); 12] = [
        // Helper's own workspace-failure warnings (WorkspaceFailed handler).
        (
            "[WARN] Workspace error: [Failure] Msbuild failed when processing the file 'C:\\r\\p.csproj' with message: Dependency specified was X but ended up with X 1.2.3",
            true,
        ),
        (
            "[WARN] Solution load partially failed (InvalidOperationException: boom); continuing with 13 loaded project(s).",
            true,
        ),
        (
            "serve: [WARN] no projects loaded — cannot serve.",
            true,
        ),
        // Bare MSBuildWorkspace diagnostic without the helper prefix.
        (
            "[Failure] Msbuild failed when processing the file 'C:\\r\\p.csproj'",
            true,
        ),
        ("[WARN] Project file not found: C:\\r\\missing.csproj", true),
        // Progress / info lines must NOT escalate to warn.
        (
            "[INFO] Skipping unsupported project type: C:\\r\\x.shproj",
            false,
        ),
        // A helper-demoted note renders the bare diagnostic MESSAGE: it must
        // not be captured. The trap this pins: a demotion that renders the
        // full diagnostic re-embeds "[Failure]" and re-enters this channel.
        (
            "[INFO] Workspace note: Msbuild failed when processing the file 'C:\\r\\p.csproj' with message: Grpc.Net.ClientFactory 2.63.0 or earlier could cause issues when used together with Microsoft.Extensions.Http.Resilience.",
            false,
        ),
        // The buggy shape, documented: the tag alone keeps the line captured
        // regardless of the [INFO] prefix.
        (
            "[INFO] Workspace note: [Failure] Msbuild failed when processing the file 'C:\\r\\p.csproj'",
            true,
        ),
        ("Loading solution: C:\\r\\x.sln", false),
        ("Loaded 13 project(s) from filtered solution", false),
        (
            "MSBuild: registering '.NET SDK' v10.0.303 at C:\\Program Files\\dotnet\\sdk\\10.0.303",
            false,
        ),
        ("Index written to: C:\\tmp\\out.json", false),
    ];

    for (line, expected) in cases {
        assert_eq!(
            is_helper_warning_line(line),
            expected,
            "misclassified line: {line}"
        );
    }
}

// ── Index-level completeness warnings (features/always-on-symbol-indexes) ──

/// The live MSBuild project-load failure format observed on a customer
/// worktree (NuGet restore broken: analyzer package version mismatch) —
/// the exact line class whose warnings silently disappeared into the log
/// while every find_impact answer read clean. Identifiers anonymized; the
/// shape is what the parser must match.
const LIVE_MSBUILD_FAILURE: &str = "[WARN] Workspace error: [Failure] Msbuild failed when processing the file 'C:\\repos\\demo-worktree\\src\\App.Dam\\App.Dam.csproj' with message: App.Dam depends on Analyzer.X (>= 1.2.3) but Analyzer.X 1.2.3 was not found. Analyzer.X 1.3.0 was resolved instead.";

/// Normalization collapses the project-load failure to `<file>: <msg>`;
/// duplicates (MSBuild repeats per project and per pass) dedupe; plain
/// warning lines pass through trimmed; non-warning lines are dropped.
#[test]
fn summarize_index_warnings_normalizes_dedupes_and_skips_noise() {
    let msbuild_second = "[WARN] Workspace error: [Failure] Msbuild failed when processing the file 'C:\\repos\\other\\src\\App.Web\\App.Web.csproj' with message: App.Web depends on Analyzer.X (>= 1.2.3) but Analyzer.X 1.2.3 was not found. Analyzer.X 1.3.0 was resolved instead.";
    let lines = vec![
        "MSBuild: registering '.NET Core SDK' v10.0.401".to_string(), // info, dropped
        LIVE_MSBUILD_FAILURE.to_string(),
        LIVE_MSBUILD_FAILURE.to_string(), // duplicate, deduped
        msbuild_second.to_string(),
        "[WARN] Grpc.Net.ClientFactory 2.63.0 or earlier could cause issues".to_string(),
        "".to_string(),
    ];

    let out = super::csharp::summarize_index_warnings(&lines, 10);

    assert_eq!(out.len(), 3, "expected 3 distinct warnings, got: {out:?}");
    assert!(
        out[0].starts_with("App.Dam.csproj: App.Dam depends on Analyzer.X"),
        "first entry must be the normalized project failure, got: {}",
        out[0]
    );
    assert!(
        out[1].starts_with("App.Web.csproj: "),
        "second entry must be the second project, got: {}",
        out[1]
    );
    assert!(
        out[2].starts_with("[WARN] Grpc"),
        "unrecognized warning format passes through verbatim, got: {}",
        out[2]
    );
    // The failure message is longer than the 160-char cap — pinned so a
    // persisted entry can never blow up the warnings array on answers.
    let msg = out[0].split_once(": ").expect("normalized shape").1;
    assert!(msg.chars().count() <= 160, "message must be capped");
}

/// The passthrough cap must cut at whitespace, never mid-word: a hard
/// 220-char cut landed inside the quoted CS0103 specimen name at the tail
/// edge of the line even after the helper trimmed its own specimen.
#[test]
fn normalize_passthrough_caps_at_whitespace_boundary() {
    let prefix = "[WARN] Compilation errors in Very.Long.Project.Name.Catalog: 12 error(s) — dominant CS0103 x10; first: C:\\src\\Cat\\ViewModels\\CustomerAccountViewModel.cs(42,17): error CS0103: The name '";
    let identifier = "CustomerOrderViewModelFactoryProviderCache";
    let line = format!("{prefix}{identifier}' does not exist in the current context");
    assert!(
        line.chars().count() > 220,
        "test setup: line must exceed the cap"
    );

    let out = super::csharp::normalize_index_warning_line(&line).expect("warning line");

    assert!(
        out.chars().count() <= 220,
        "cap must hold, got {}",
        out.chars().count()
    );
    assert_ne!(
        out.chars().count(),
        220,
        "a whitespace boundary exists before the cap — cut there, not at the hard cap"
    );
    // `out` is a byte-prefix of `line` (the cut lands on a char boundary),
    // so this checks the cut position in the original line directly.
    assert!(
        line[out.len()..].starts_with(' '),
        "cut must land on the original line's whitespace boundary, tail: ...{}",
        &line[out.len().saturating_sub(40)..]
    );
}

/// The cap kicks in with an explicit overflow entry — the list rides on
/// every find_impact answer and must stay bounded.
#[test]
fn summarize_index_warnings_caps_with_overflow_note() {
    let lines: Vec<String> = (0..5)
        .map(|i| format!("[WARN] distinct failure number {i}"))
        .collect();
    let out = super::csharp::summarize_index_warnings(&lines, 3);
    assert_eq!(out.len(), 4, "3 capped entries + overflow note: {out:?}");
    assert_eq!(out[3], "… and 2 more distinct warning(s)");
}

/// The meta roundtrip: what rebuild persists is what `index_warnings`
/// returns; absent key and corrupt JSON both read as clean (never block an
/// answer), and the TypeScript adapter inherits the empty default.
#[test]
fn index_warnings_roundtrip_and_clean_absence() {
    use crate::symbols::SymbolIndexer as _;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("codesearch.db");

    let csharp = super::csharp::CSharpSymbolIndexer::new();
    assert!(
        csharp.index_warnings(&db_path).is_empty(),
        "absent meta must read as clean"
    );

    let env = crate::symbols::get_shared_scip_env(&db_path).unwrap();
    let meta = crate::lmdb_registry::create_named_db::<heed::types::Str, heed::types::Str>(
        &env,
        crate::constants::SCIP_META_DB_NAME,
    )
    .unwrap();
    let mut wtxn = env.write_txn().unwrap();
    let stored = vec![
        "summary line".to_string(),
        "App.Dam.csproj: Analyzer.X not found".to_string(),
    ];
    meta.put(
        &mut wtxn,
        crate::constants::SCIP_INDEX_WARNINGS_KEY,
        serde_json::to_string(&stored).unwrap().as_str(),
    )
    .unwrap();
    wtxn.commit().unwrap();

    assert_eq!(
        csharp.index_warnings(&db_path),
        stored,
        "persisted warnings must round-trip verbatim"
    );

    // Corrupt JSON reads as clean, not as an error.
    let mut wtxn = env.write_txn().unwrap();
    meta.put(
        &mut wtxn,
        crate::constants::SCIP_INDEX_WARNINGS_KEY,
        "not json [",
    )
    .unwrap();
    wtxn.commit().unwrap();
    assert!(
        csharp.index_warnings(&db_path).is_empty(),
        "corrupt meta must read as clean, never fail the answer"
    );

    let ts = super::typescript::TypeScriptSymbolIndexer::new();
    assert!(
        ts.index_warnings(&db_path).is_empty(),
        "non-tracking adapters inherit the empty default"
    );
}

// ── Builder-version stamp: the rebuild's meta-write site ─────────────────
//
// The gate tests hand-seed meta (consumer half); this one drives the
// producer. The write lives in `write_rebuild_meta` — the exact fn the
// rebuild pipeline calls from its meta txn — because a fake helper cannot
// execute on Windows: the validated filename must be a real PE, not a
// script. Deleting the stamp write must fail this test, or every serve
// start churns all inherited C# indexes (absent stamp reads as stale).

/// The C# freshness override mirrors the rebuild gate exactly: a current
/// stamp matches, an absent or foreign stamp does not — while adapters
/// without stamping take the trait default (`true`) and never read stale.
#[test]
fn index_matches_builder_mirrors_the_rebuild_gate() {
    use crate::constants::{INDEX_BUILDER_VERSION, SCIP_INDEX_BUILDER_VERSION_KEY};

    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("db");
    let indexer = super::csharp::CSharpSymbolIndexer::new();
    use crate::symbols::SymbolIndexer as _;

    // No meta at all: absent stamp reads as stale (the pre-stamp class).
    assert!(
        !indexer.index_matches_builder(&db),
        "an index with no builder stamp must not count as this build's"
    );

    // A foreign binary's stamp: stale.
    let env = crate::symbols::get_shared_scip_env(&db).unwrap();
    let meta_db = crate::lmdb_registry::create_named_db::<heed::types::Str, heed::types::Str>(
        &env,
        crate::constants::SCIP_META_DB_NAME,
    )
    .unwrap();
    let mut wtxn = env.write_txn().unwrap();
    meta_db
        .put(&mut wtxn, SCIP_INDEX_BUILDER_VERSION_KEY, "v0.0.0+1")
        .unwrap();
    wtxn.commit().unwrap();
    assert!(
        !indexer.index_matches_builder(&db),
        "another build's stamp must not count as this build's"
    );

    // This build's stamp but no reference-coverage stamp: stale.
    let env = crate::symbols::get_shared_scip_env(&db).unwrap();
    let meta_db = crate::lmdb_registry::open_named_db::<heed::types::Str, heed::types::Str>(
        &env,
        crate::constants::SCIP_META_DB_NAME,
    )
    .unwrap()
    .unwrap();
    let mut wtxn = env.write_txn().unwrap();
    meta_db
        .put(
            &mut wtxn,
            SCIP_INDEX_BUILDER_VERSION_KEY,
            INDEX_BUILDER_VERSION,
        )
        .unwrap();
    wtxn.commit().unwrap();
    assert!(
        !indexer.index_matches_builder(&db),
        "an index without a refs_coverage stamp must rebuild once"
    );

    // This build's stamp with an `incremental` coverage stamp: fresh — the
    // steady state between incrementals (project-scoped truth, on-demand
    // misses), never a trigger for a full rebuild.
    for coverage in [
        crate::constants::SCIP_REFS_COVERAGE_COMPLETE,
        crate::constants::SCIP_REFS_COVERAGE_PARTIAL,
        crate::constants::SCIP_REFS_COVERAGE_INCREMENTAL,
    ] {
        let env = crate::symbols::get_shared_scip_env(&db).unwrap();
        let meta_db = crate::lmdb_registry::open_named_db::<heed::types::Str, heed::types::Str>(
            &env,
            crate::constants::SCIP_META_DB_NAME,
        )
        .unwrap()
        .unwrap();
        let mut wtxn = env.write_txn().unwrap();
        super::csharp::CSharpSymbolIndexer::write_rebuild_meta(&meta_db, &mut wtxn, &[], coverage)
            .expect("meta write must succeed");
        wtxn.commit().unwrap();
        assert!(
            indexer.index_matches_builder(&db),
            "a rebuild by this binary must match (stamped {INDEX_BUILDER_VERSION}, {coverage})"
        );
        assert_eq!(indexer.refs_coverage(&db).as_deref(), Some(coverage));
    }

    // This build's stamp with a `lazy` coverage stamp: stale.
    let env = crate::symbols::get_shared_scip_env(&db).unwrap();
    let meta_db = crate::lmdb_registry::open_named_db::<heed::types::Str, heed::types::Str>(
        &env,
        crate::constants::SCIP_META_DB_NAME,
    )
    .unwrap()
    .unwrap();
    let mut wtxn = env.write_txn().unwrap();
    super::csharp::CSharpSymbolIndexer::write_rebuild_meta(
        &meta_db,
        &mut wtxn,
        &[],
        crate::constants::SCIP_REFS_COVERAGE_LAZY,
    )
    .expect("meta write must succeed");
    wtxn.commit().unwrap();
    assert!(
        !indexer.index_matches_builder(&db),
        "a lazy coverage stamp must read as stale — only a rebuild produces \
         precomputed references"
    );
}

#[test]
fn rebuild_meta_write_stamps_version_and_writes_or_clears_warnings() {
    use crate::constants::{SCIP_INDEX_BUILDER_VERSION, SCIP_INDEX_BUILDER_VERSION_KEY};

    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("db");
    let indexer = super::csharp::CSharpSymbolIndexer::new();

    // Degraded previous build: stale warnings + an old binary's stamp.
    let env = crate::symbols::get_shared_scip_env(&db).unwrap();
    let meta_db = crate::lmdb_registry::create_named_db::<heed::types::Str, heed::types::Str>(
        &env,
        crate::constants::SCIP_META_DB_NAME,
    )
    .unwrap();
    let mut wtxn = env.write_txn().unwrap();
    meta_db
        .put(
            &mut wtxn,
            crate::constants::SCIP_INDEX_WARNINGS_KEY,
            r#"["Old.csproj: Msbuild failed when processing the file"]"#,
        )
        .unwrap();
    meta_db
        .put(&mut wtxn, SCIP_INDEX_BUILDER_VERSION_KEY, "v0.0.0+1")
        .unwrap();
    wtxn.commit().unwrap();

    // A clean rebuild's meta write: stamp this build, clear the warnings.
    let env = crate::symbols::get_shared_scip_env(&db).unwrap();
    let meta_db = crate::lmdb_registry::open_named_db::<heed::types::Str, heed::types::Str>(
        &env,
        crate::constants::SCIP_META_DB_NAME,
    )
    .unwrap()
    .unwrap();
    let mut wtxn = env.write_txn().unwrap();
    super::csharp::CSharpSymbolIndexer::write_rebuild_meta(
        &meta_db,
        &mut wtxn,
        &[],
        crate::constants::SCIP_REFS_COVERAGE_LAZY,
    )
    .expect("meta write must succeed");
    wtxn.commit().unwrap();

    use crate::symbols::SymbolIndexer as _;
    assert_eq!(
        indexer.index_builder_version(&db).as_deref(),
        Some(SCIP_INDEX_BUILDER_VERSION),
        "the rebuild meta write must stamp the producing build"
    );
    assert!(
        indexer.index_warnings(&db).is_empty(),
        "a clean rebuild must clear the previous run's warnings"
    );

    // A degraded rebuild's meta write persists its warnings instead.
    let env = crate::symbols::get_shared_scip_env(&db).unwrap();
    let meta_db = crate::lmdb_registry::open_named_db::<heed::types::Str, heed::types::Str>(
        &env,
        crate::constants::SCIP_META_DB_NAME,
    )
    .unwrap()
    .unwrap();
    let mut wtxn = env.write_txn().unwrap();
    super::csharp::CSharpSymbolIndexer::write_rebuild_meta(
        &meta_db,
        &mut wtxn,
        &["Broken.csproj: Msbuild failed when processing the file".to_string()],
        crate::constants::SCIP_REFS_COVERAGE_LAZY,
    )
    .expect("meta write must succeed");
    wtxn.commit().unwrap();

    // index_warnings_stored prepends the summary entry (summary first).
    let warnings = indexer.index_warnings(&db);
    assert_eq!(
        warnings.len(),
        2,
        "summary entry + the raw failed-project line, got: {warnings:?}"
    );
    assert!(
        warnings[0].contains("1 distinct failure(s)"),
        "the summary entry must lead, got: {warnings:?}"
    );
    assert_eq!(
        warnings[1],
        "Broken.csproj: Msbuild failed when processing the file".to_string(),
        "a degraded rebuild's warnings must be persisted for the answer path"
    );
}

// ── Simple-name extraction (fuzzy-lookup keys) ────────────────────────────

/// The live defect this pins: key format 2.0 writes fully-qualified
/// parameter types, and the old extractor split on '.' BEFORE stripping the
/// parameter list — landing inside the parameters. Every parameterized
/// method then keyed `scip_simple_names` under a fragment like
/// "Activity, int)" and fuzzy find_impact resolved NOTHING repository-wide
/// after the version-gate rebuilt all indexes to format 2.0.
#[test]
fn extract_simple_name_strips_fqn_parameters_before_segmenting() {
    let cases = [
        // (canonical key, expected simple name)
        ("csharp App . FieldDefinition#Validate().", "Validate"),
        (
            "csharp SmallSolution.Library . Calculator#Add(int, int).",
            "Add",
        ),
        (
            "csharp Acme.Catalog.Azure.TableStorage.DataStore . ActivityVersionStore#SaveBatchAsync(int, System.Collections.Generic.IReadOnlyList<global::Acme.Catalog.Azure.TableStorage.DataStore.Entities.ActivityVersion>).",
            "SaveBatchAsync",
        ),
        (
            "csharp Acme.Catalog.Import.CmdInfra . Arguments#GetVariableValue`1(string, Acme.Catalog.Import.CmdInfra.ArgumentValidator<T>).",
            "GetVariableValue",
        ),
        ("csharp . . . Namespace.TopLevel", "TopLevel"),
        ("csharp App . MyService#", "MyService"),
        ("csharp Ns.Sub . Class#_field", "_field"),
        // Generic TYPE arity lives on the type segment — stripped too, not
        // just the method-side backtick.
        ("csharp Ns . List`1#", "List"),
    ];
    for (key, expected) in cases {
        assert_eq!(
            super::csharp::extract_simple_name(key),
            expected,
            "key: {key}"
        );
    }
}

// ── Restore-pending gate (issue #225) ────────────────────────────────

#[test]
fn helper_version_line_parses_to_the_version_token() {
    use super::csharp::parse_helper_version;

    let cases = [
        ("scip-csharp 1.5.4", "1.5.4"),
        ("scip-csharp 0.0.0-fake", "0.0.0-fake"),
        ("scip-csharp 1.5.4+1352", "1.5.4+1352"),
        // Unparseable → "unknown": the startup entry still logs (as a
        // mismatch) instead of silently skipping.
        ("", "unknown"),
        ("garbage", "unknown"),
        ("scip-csharp", "unknown"),
    ];
    for (line, expected) in cases {
        assert_eq!(parse_helper_version(line), expected, "line: {line:?}");
    }
}

#[test]
fn restore_pending_error_downcasts_from_anyhow() {
    let pending = anyhow::Error::new(RestorePendingError);
    assert!(is_restore_pending(&pending));

    let generic = anyhow::anyhow!("Failed to read symbol index at /tmp/x.json");
    assert!(!is_restore_pending(&generic));
}

#[test]
fn restore_pending_flag_round_trips_through_the_trait_method() {
    use super::SymbolIndexer as _;

    let db_path = PathBuf::from(format!(
        "{}cs-restore-flag-test-{}",
        std::env::temp_dir().display(),
        std::process::id()
    ));
    let indexer = super::csharp::CSharpSymbolIndexer::new();
    restore_pending_flag_clear(&db_path); // isolate from concurrent tests
    assert!(!indexer.restore_pending(&db_path));

    restore_pending_flag_set(&db_path);
    assert!(indexer.restore_pending(&db_path));

    restore_pending_flag_clear(&db_path);
    assert!(!indexer.restore_pending(&db_path));
}

#[test]
fn symbol_index_status_reports_restore_pending_over_a_ready_index() {
    use super::SymbolIndexer as _;
    use crate::constants::{
        SCIP_INDEX_BUILDER_VERSION, SCIP_INDEX_BUILDER_VERSION_KEY, SCIP_KEY_FORMAT,
        SCIP_KEY_FORMAT_KEY, SCIP_META_DB_NAME, SCIP_REBUILD_TIMESTAMP_KEY,
        SCIP_REFS_COVERAGE_COMPLETE, SCIP_REFS_COVERAGE_KEY,
    };

    struct PendingFlag(PathBuf);
    impl Drop for PendingFlag {
        fn drop(&mut self) {
            restore_pending_flag_clear(&self.0);
        }
    }

    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("Status.sln"), "").unwrap();
    let db = dir.path().join("db");
    {
        let env = crate::symbols::get_shared_scip_env(&db).unwrap();
        let meta = crate::lmdb_registry::open_named_db::<heed::types::Str, heed::types::Str>(
            &env,
            SCIP_META_DB_NAME,
        )
        .unwrap()
        .unwrap();
        let mut wtxn = env.write_txn().unwrap();
        for (key, value) in [
            (SCIP_REBUILD_TIMESTAMP_KEY, "0"),
            (SCIP_KEY_FORMAT_KEY, SCIP_KEY_FORMAT),
            (SCIP_INDEX_BUILDER_VERSION_KEY, SCIP_INDEX_BUILDER_VERSION),
            (SCIP_REFS_COVERAGE_KEY, SCIP_REFS_COVERAGE_COMPLETE),
        ] {
            meta.put(&mut wtxn, key, value).unwrap();
        }
        wtxn.commit().unwrap();
    }
    assert!(super::csharp::CSharpSymbolIndexer::new().index_matches_builder(&db));

    let registry = super::SymbolIndexerRegistry::new();
    let csharp_state = || {
        super::symbol_index_status(&registry, dir.path(), &db)
            .into_iter()
            .find(|s| s.language == "csharp")
            .expect("csharp status entry")
            .state
    };

    let flag = PendingFlag(db.clone());
    restore_pending_flag_set(&db);
    assert_eq!(csharp_state(), "restore-pending");

    drop(flag);
    assert_eq!(csharp_state(), "ready");
}

#[test]
fn helper_restore_pending_exit_matches_csharp_exit_codes() {
    // A drifted exit code would demote restore-pending to a generic helper failure.
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../helpers/csharp/RestoreGate.cs"
    );
    let source = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read {path} to check HELPER_RESTORE_PENDING_EXIT: {e}"));
    let line = source
        .lines()
        .find(|l| l.contains("const int RestorePending"))
        .unwrap_or_else(|| {
            panic!("{path} has no `const int RestorePending` line to compare with HELPER_RESTORE_PENDING_EXIT")
        });
    let raw = line
        .split_once('=')
        .and_then(|(_, rhs)| rhs.split_once(';'))
        .map(|(value, _)| value.trim())
        .unwrap_or_else(|| {
            panic!(
                "cannot find `= <value>;` in {line:?} to compare with HELPER_RESTORE_PENDING_EXIT"
            )
        });
    let parsed: i32 = raw.parse().unwrap_or_else(|e| {
        panic!("C# RestorePending value {raw:?} is not an integer ({e}); compare with HELPER_RESTORE_PENDING_EXIT")
    });
    assert_eq!(
        parsed, HELPER_RESTORE_PENDING_EXIT,
        "C# ExitCodes.RestorePending ({parsed}) and Rust HELPER_RESTORE_PENDING_EXIT ({HELPER_RESTORE_PENDING_EXIT}) must stay in sync"
    );
}

/// serve shutdown must kill in-flight helper children: an orphaned
/// scip-csharp holds the helper exe locked for minutes (deploy blocker).
/// The registry is PID-generic, so a stand-in long-running child verifies
/// the real kill path (taskkill /T /F on Windows, kill -9 elsewhere).
#[test]
fn shutdown_kills_registered_helper_children() {
    #[cfg(windows)]
    let mut child = std::process::Command::new("cmd")
        .args(["/c", "ping", "-n", "30", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("stand-in child spawned");
    #[cfg(not(windows))]
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("stand-in child spawned");

    let pid = child.id();
    {
        let _live = LiveHelperChild::register(&child);
        kill_live_helper_children();
    }

    let status = child.wait().expect("child waited");
    assert!(
        !status.success(),
        "a killed helper child must not report success (pid {pid})"
    );
}

/// A `lazy` stamp still writes fine via `write_rebuild_meta` (the helper
/// accepts any coverage value — filtered runs produce it), but the rebuild
/// gate no longer accepts it as fresh: see `index_matches_builder`.
#[test]
fn refs_coverage_follows_precompute_and_index_warnings() {
    use super::csharp::refs_coverage_for;
    use crate::constants::{
        SCIP_REFS_COVERAGE_COMPLETE, SCIP_REFS_COVERAGE_LAZY, SCIP_REFS_COVERAGE_PARTIAL,
    };
    for (precomputed, clean, expected) in [
        (true, true, SCIP_REFS_COVERAGE_COMPLETE),
        (true, false, SCIP_REFS_COVERAGE_PARTIAL),
        (false, true, SCIP_REFS_COVERAGE_LAZY),
        (false, false, SCIP_REFS_COVERAGE_LAZY),
    ] {
        assert_eq!(
            refs_coverage_for(precomputed, clean),
            expected,
            "precomputed={precomputed} clean={clean}"
        );
    }
}

/// The helper writes `<stem>-filtered-{Guid:N}.sln` next to the real
/// solution and keeps it on disk for the whole minutes-long workspace
/// load; on NTFS it enumerates before `ACME.dam.sln` ('-' < '.'). The
/// defect this pins: `find_solution` returned that transient file, so
/// every lookup racing an in-flight load chained another filter
/// generation — each a distinct workspace-pool key with its own ~600 MB
/// resident `scip-csharp serve`.
#[test]
fn find_solution_skips_transient_filtered_solutions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let real = root.join("ACME.dam.sln");
    std::fs::write(&real, b"").expect("write real sln");
    let guid = "0123456789abcdef0123456789abcdef";
    std::fs::write(root.join(format!("ACME.dam-filtered-{guid}.sln")), b"")
        .expect("write transient filtered sln");
    std::fs::write(
        root.join(format!("ACME.dam-filtered-{guid}-filtered-{guid}.sln")),
        b"",
    )
    .expect("write chained filtered sln");

    let found = CSharpSymbolIndexer::find_solution(root).expect("real solution found");
    assert_eq!(found, real);
}

#[test]
fn transient_filtered_solution_predicate_is_precise() {
    let guid = "0123456789abcdef0123456789abcdef";
    let is = |name: &str| is_transient_filtered_solution(std::path::Path::new(name));
    assert!(is(&format!("a-filtered-{guid}.sln")));
    assert!(is(&format!("a-filtered-{guid}-filtered-{guid}.sln")));
    assert!(!is("ACME.dam.sln"));
    assert!(!is("a-filtered-short.sln"));
    assert!(!is(&format!("a-filtered-{}.sln", "z".repeat(32))));
    assert!(!is("no-marker.sln"));
}

#[derive(Clone, Copy)]
enum Reply {
    Applied,
    NeedsReload,
    Fail,
    /// Blocks until killed, like a helper that stopped answering.
    Hang,
}

/// What the resident saw: per `update`, the changed paths and the ref-cache
/// keys committed at that moment.
#[derive(Default)]
struct Trace {
    updates: Mutex<Vec<(Vec<PathBuf>, Vec<String>)>>,
    /// Per `update`, the deleted paths.
    deletions: Mutex<Vec<Vec<PathBuf>>>,
    spawns: AtomicUsize,
    kills: AtomicUsize,
}

struct TraceClient {
    reply: Reply,
    db: PathBuf,
    trace: Arc<Trace>,
    killed: AtomicBool,
}

impl ClientLike for TraceClient {
    fn update(&self, changed: &[PathBuf], deleted: &[PathBuf]) -> anyhow::Result<UpdateOutcome> {
        self.trace.deletions.lock().unwrap().push(deleted.to_vec());
        let committed = ref_cache_keys(&self.db);
        self.trace
            .updates
            .lock()
            .unwrap()
            .push((changed.to_vec(), committed));
        match self.reply {
            Reply::Applied => Ok(UpdateOutcome::Applied(changed.len())),
            Reply::NeedsReload => Ok(UpdateOutcome::NeedsReload(
                "needs-reload: src/New.cs".to_string(),
            )),
            Reply::Fail => anyhow::bail!("helper update crashed"),
            Reply::Hang => {
                while !self.killed.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                anyhow::bail!("killed mid-update")
            }
        }
    }

    fn find_refs(&self, _symbol: &str) -> anyhow::Result<ResidentRefs> {
        Ok(ResidentRefs::default())
    }

    fn kill(&self) {
        if !self.killed.swap(true, Ordering::SeqCst) {
            self.trace.kills.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn trace_pool(reply: Reply, db: PathBuf, trace: Arc<Trace>) -> &'static WorkspacePool {
    let spawn = Box::new(
        move |_: &Path, _: &Path, _: u64| -> anyhow::Result<Arc<dyn ClientLike>> {
            trace.spawns.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(TraceClient {
                reply,
                db: db.clone(),
                trace: Arc::clone(&trace),
                killed: AtomicBool::new(false),
            }))
        },
    );
    let gate = Arc::new(HelperGate::new(1));
    Box::leak(Box::new(WorkspacePool::new(
        2,
        Duration::from_secs(600),
        1,
        spawn,
        gate,
    )))
}

/// A `Files` rebuild applies the edit to the resident workspace before the
/// commit that drops the stale ref-cache key, and evicts it only when that
/// update fails; a full rebuild always evicts.
#[test]
#[serial_test::serial]
fn files_rebuild_updates_the_resident_before_the_cache_commit_and_evicts_only_on_failure() {
    // (case, resident reply, full scope, killed)
    let cases: &[(&str, Reply, bool, usize)] = &[
        ("applied", Reply::Applied, false, 0),
        ("needs reload", Reply::NeedsReload, false, 1),
        ("helper error", Reply::Fail, false, 1),
        ("no reply in time", Reply::Hang, false, 1),
        ("full scope", Reply::Applied, true, 1),
    ];
    let helper_home = tempfile::TempDir::new().unwrap();
    let helper = build_fake_csharp_helper(helper_home.path());
    let _env = crate::testing::EnvRestore::set(&[
        (
            crate::constants::SCIP_CSHARP_HELPER_ENV,
            helper.to_string_lossy().as_ref(),
        ),
        ("CS_FAKE_INDEX", "2.1"),
        (crate::constants::SCIP_RESIDENT_UPDATE_TIMEOUT_SECS_ENV, "1"),
    ]);
    for (case, reply, full, kills) in cases {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, db) = (dir.path(), dir.path().join("db"));
        let sln = root.join("FakeSolution.sln");
        std::fs::write(&sln, "").unwrap();
        let trace = Arc::new(Trace::default());
        let pool = trace_pool(*reply, db.clone(), Arc::clone(&trace));
        let indexer = CSharpSymbolIndexer::with_pool(pool);
        indexer.rebuild(root, &db, RebuildScope::Full).unwrap();
        pool.find_refs(&helper, &sln, root, "Sym").unwrap();

        std::fs::create_dir_all(root.join("src")).unwrap();
        let edited = root.join("src/C.cs");
        std::fs::write(&edited, "class Unrelated { void Other() { } }").unwrap();
        let scope = if *full {
            RebuildScope::Full
        } else {
            RebuildScope::Files {
                changed: vec![edited.clone()],
                deleted: vec![],
            }
        };
        indexer.rebuild(root, &db, scope).unwrap();

        let updates = trace.updates.lock().unwrap().clone();
        if *full {
            assert!(updates.is_empty(), "{case}: {updates:?}");
        } else {
            let [(changed, committed)] = updates.as_slice() else {
                panic!("{case}: expected one update, got {updates:?}");
            };
            assert_eq!(changed, &vec![edited.clone()], "{case}");
            assert!(
                committed.iter().any(|k| k == FAKE_REFERENCED),
                "{case}: update ran after the ref-cache commit: {committed:?}"
            );
            assert!(
                !ref_cache_keys(&db).iter().any(|k| k == FAKE_REFERENCED),
                "{case}: the edit must invalidate the referencing key"
            );
        }
        assert_eq!(trace.kills.load(Ordering::SeqCst), *kills, "{case}: kills");
        pool.find_refs(&helper, &sln, root, "Sym").unwrap();
        assert_eq!(
            trace.spawns.load(Ordering::SeqCst),
            1 + kills,
            "{case}: respawns only after an eviction"
        );
    }
}

/// Paths the resident cannot use never reach it: non-C# files are dropped
/// (an all-ignored change set does not even run a symbol rebuild),
/// project-graph files evict it without an update, the rest is forwarded.
#[test]
#[serial_test::serial]
fn files_rebuild_forwards_only_csharp_relevant_paths_to_the_resident() {
    type Forwarded = Option<(&'static [&'static str], &'static [&'static str])>;
    // (case, changed, deleted, forwarded to update, kills, index helper runs)
    let cases: &[(&str, &[&str], &[&str], Forwarded, usize, usize)] = &[
        (
            "source beside documents",
            &["src/X.cs", "AGENTS.md", "README.md", "notes.txt"],
            &[],
            Some((&["src/X.cs"], &[])),
            0,
            1,
        ),
        (
            "documents only",
            &["AGENTS.md", "README.md"],
            &[],
            None,
            0,
            0,
        ),
        (
            "documents changed and deleted",
            &["notes.txt"],
            &["old.md"],
            None,
            0,
            0,
        ),
        (
            "razor",
            &["Pages/Index.razor"],
            &[],
            Some((&["Pages/Index.razor"], &[])),
            0,
            1,
        ),
        ("project file", &["App.csproj"], &[], None, 1, 1),
        (
            "project file beside source",
            &["src/X.cs", "App.csproj"],
            &[],
            None,
            1,
            1,
        ),
        (
            "deleted source beside deleted document",
            &["src/X.cs"],
            &["src/Old.cs", "old.md"],
            Some((&["src/X.cs"], &["src/Old.cs"])),
            0,
            1,
        ),
    ];
    let helper_home = tempfile::TempDir::new().unwrap();
    let helper = build_fake_csharp_helper(helper_home.path());
    let index_log = helper_home.path().join("index.log");
    let index_runs = || std::fs::read_to_string(&index_log).map_or(0, |log| log.lines().count());
    let _env = crate::testing::EnvRestore::set(&[
        (
            crate::constants::SCIP_CSHARP_HELPER_ENV,
            helper.to_string_lossy().as_ref(),
        ),
        ("CS_FAKE_INDEX", "2.1"),
        ("CS_FAKE_INDEX_LOG", index_log.to_string_lossy().as_ref()),
        (crate::constants::SCIP_RESIDENT_UPDATE_TIMEOUT_SECS_ENV, "1"),
    ]);
    let mut problems = Vec::new();
    for (case, changed, deleted, forwarded, kills, runs) in cases {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, db) = (dir.path(), dir.path().join("db"));
        let sln = root.join("FakeSolution.sln");
        std::fs::write(&sln, "").unwrap();
        let trace = Arc::new(Trace::default());
        let pool = trace_pool(Reply::Applied, db.clone(), Arc::clone(&trace));
        let indexer = CSharpSymbolIndexer::with_pool(pool);
        indexer.rebuild(root, &db, RebuildScope::Full).unwrap();
        pool.find_refs(&helper, &sln, root, "Sym").unwrap();

        let absolute =
            |names: &[&str]| -> Vec<PathBuf> { names.iter().map(|n| root.join(n)).collect() };
        for path in absolute(changed) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "class Unrelated { void Other() { } }").unwrap();
        }
        let (runs_before, cache_before) = (index_runs(), ref_cache_keys(&db));
        let scope = RebuildScope::Files {
            changed: absolute(changed),
            deleted: absolute(deleted),
        };
        indexer.rebuild(root, &db, scope).unwrap();

        let received: Vec<(Vec<PathBuf>, Vec<PathBuf>)> = trace
            .updates
            .lock()
            .unwrap()
            .iter()
            .map(|(changed, _)| changed.clone())
            .zip(trace.deletions.lock().unwrap().iter().cloned())
            .collect();
        let expected: Vec<(Vec<PathBuf>, Vec<PathBuf>)> = forwarded
            .iter()
            .map(|(changed, deleted)| (absolute(changed), absolute(deleted)))
            .collect();
        if received != expected {
            problems.push(format!(
                "{case}: update received {received:?}, expected {expected:?}"
            ));
        }
        let killed = trace.kills.load(Ordering::SeqCst);
        if killed != *kills {
            problems.push(format!("{case}: kills {killed}, expected {kills}"));
        }
        let ran = index_runs() - runs_before;
        if ran != *runs {
            problems.push(format!("{case}: index helper runs {ran}, expected {runs}"));
        }
        if *runs == 0 && ref_cache_keys(&db) != cache_before {
            problems.push(format!("{case}: the ref-cache was written"));
        }
        pool.find_refs(&helper, &sln, root, "Sym").unwrap();
        let spawned = trace.spawns.load(Ordering::SeqCst);
        if spawned != 1 + kills {
            problems.push(format!("{case}: spawns {spawned}, expected {}", 1 + kills));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn csharp_relevance_classifies_by_file_name_suffix() {
    use Relevance::{Ignore, ReloadDirectly, Send};
    let cases: &[(&str, Relevance)] = &[
        ("src/X.cs", Send),
        ("src/X.CS", Send),
        ("Pages/Index.razor", Send),
        ("Views/Home.cshtml", Send),
        (".editorconfig", Send),
        ("src/Sub/.editorconfig", Send),
        (".globalconfig", Send),
        ("Strings.resx", Send),
        ("App.csproj", ReloadDirectly),
        ("App.sln", ReloadDirectly),
        ("App.slnx", ReloadDirectly),
        ("Directory.Packages.props", ReloadDirectly),
        ("Directory.Build.targets", ReloadDirectly),
        ("packages.lock.json", ReloadDirectly),
        ("sub/global.json", ReloadDirectly),
        ("AGENTS.md", Ignore),
        ("README.md", Ignore),
        ("notes.txt", Ignore),
        ("appsettings.json", Ignore),
        ("package.json", Ignore),
        ("NuGet.config", Ignore),
        ("data.csv", Ignore),
        ("my.global.json", Ignore),
        ("X.cs.bak", Ignore),
    ];
    for (path, expected) in cases {
        assert_eq!(csharp_relevance(Path::new(path)), *expected, "{path}");
    }
}
