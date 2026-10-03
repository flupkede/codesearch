//! Helper stderr routing tests (`csharp.rs`). Sibling `_tests.rs` file
//! per repo convention.

use super::csharp::{drain_pipe_to_tracing, is_helper_warning_line};
use std::io::Cursor;
use std::sync::Mutex;

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
    let cases: [(&str, bool); 10] = [
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
    let mut wtxn = env.write_txn().unwrap();
    let meta: heed::Database<heed::types::Str, heed::types::Str> = env
        .create_database(&mut wtxn, Some(crate::constants::SCIP_META_DB_NAME))
        .unwrap();
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

#[test]
fn rebuild_meta_write_stamps_version_and_writes_or_clears_warnings() {
    use crate::constants::{INDEX_BUILDER_VERSION, SCIP_INDEX_BUILDER_VERSION_KEY};

    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("db");
    let indexer = super::csharp::CSharpSymbolIndexer::new();

    // Degraded previous build: stale warnings + an old binary's stamp.
    let env = crate::symbols::get_shared_scip_env(&db).unwrap();
    let mut wtxn = env.write_txn().unwrap();
    let meta_db: heed::Database<heed::types::Str, heed::types::Str> = env
        .create_database(&mut wtxn, Some(crate::constants::SCIP_META_DB_NAME))
        .unwrap();
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
    let mut wtxn = env.write_txn().unwrap();
    let meta_db: heed::Database<heed::types::Str, heed::types::Str> = env
        .open_database(&wtxn, Some(crate::constants::SCIP_META_DB_NAME))
        .unwrap()
        .unwrap();
    super::csharp::CSharpSymbolIndexer::write_rebuild_meta(&meta_db, &mut wtxn, &[])
        .expect("meta write must succeed");
    wtxn.commit().unwrap();

    use crate::symbols::SymbolIndexer as _;
    assert_eq!(
        indexer.index_builder_version(&db).as_deref(),
        Some(INDEX_BUILDER_VERSION),
        "the rebuild meta write must stamp the producing build"
    );
    assert!(
        indexer.index_warnings(&db).is_empty(),
        "a clean rebuild must clear the previous run's warnings"
    );

    // A degraded rebuild's meta write persists its warnings instead.
    let env = crate::symbols::get_shared_scip_env(&db).unwrap();
    let mut wtxn = env.write_txn().unwrap();
    let meta_db: heed::Database<heed::types::Str, heed::types::Str> = env
        .open_database(&wtxn, Some(crate::constants::SCIP_META_DB_NAME))
        .unwrap()
        .unwrap();
    super::csharp::CSharpSymbolIndexer::write_rebuild_meta(
        &meta_db,
        &mut wtxn,
        &["Broken.csproj: Msbuild failed when processing the file".to_string()],
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
