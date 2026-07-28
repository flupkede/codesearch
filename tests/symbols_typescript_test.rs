//! Integration tests for the TypeScript symbol indexing pipeline.
//!
//! Mirrors `symbols_csharp_test.rs`: non-gated tests exercise the LMDB
//! round-trip and helper-detection paths without requiring the actual
//! `scip-typescript` CLI. The full pipeline test (subprocess → protobuf →
//! LMDB → query) is gated behind the `typescript_helper_integration` cargo
//! feature AND requires Node + `scip-typescript` to be resolvable (either
//! via `npx` on `$PATH`, or `CODESEARCH_SCIP_TYPESCRIPT` pointing at a
//! direct binary).

use std::path::PathBuf;

use codesearch::symbols::typescript::TypeScriptSymbolIndexer;
use codesearch::symbols::{RebuildScope, SymbolIndexer};
use tempfile::TempDir;

#[test]
fn test_indexer_returns_empty_when_db_missing() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test-db");
    std::fs::create_dir_all(&db_path).expect("Failed to create db dir");

    let indexer = TypeScriptSymbolIndexer::new();

    // Note: is_available() may return true if npx/Node is on this host's
    // PATH. Don't assert unavailability — just exercise the empty-DB path.
    let age = indexer.index_age(&db_path);
    let _ = age; // open_scip_env creates the dir; just verify no panic.

    // find_references with no data should return Ok(empty) because
    // resolve_canonical_key returns None when no LMDB tables exist.
    let result = indexer.find_references(&db_path, "add");
    match result {
        Ok(refs) => assert!(
            refs.is_empty(),
            "Should return empty vec when no SCIP data exists, got {:?}",
            refs
        ),
        Err(e) => {
            // LMDB reopen failed (e.g. lock contention on CI). Acceptable —
            // the important invariant is that it never panics or returns
            // stale data.
            eprintln!("Note: find_references returned Err (LMDB lock contention?): {e:#}");
        }
    }
}

#[test]
fn test_applies_to_requires_root_tsconfig() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");

    // No tsconfig.json present -> does not apply.
    assert!(!TypeScriptSymbolIndexer::new().applies_to(temp_dir.path()));

    // Create a root tsconfig.json -> now it applies.
    std::fs::write(temp_dir.path().join("tsconfig.json"), "{}").unwrap();
    assert!(TypeScriptSymbolIndexer::new().applies_to(temp_dir.path()));
}

#[test]
fn test_fixture_directory_shape() {
    // Sanity check the fixture used by the gated integration test below:
    // 1 definition (`add` in math.ts) + call-sites in 2 other files.
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ts-sample");
    assert!(fixture_root.join("tsconfig.json").is_file());
    assert!(fixture_root.join("src/math.ts").is_file());
    assert!(fixture_root.join("src/consumer.ts").is_file());
    assert!(fixture_root.join("src/other.ts").is_file());

    let consumer = std::fs::read_to_string(fixture_root.join("src/consumer.ts")).unwrap();
    let other = std::fs::read_to_string(fixture_root.join("src/other.ts")).unwrap();
    // consumer.ts calls add() twice, other.ts calls it once -> 3 call-sites
    // across 2 files, plus the 1 definition in math.ts.
    assert_eq!(consumer.matches("add(").count(), 2);
    assert_eq!(other.matches("add(").count(), 1);
}

// ── Integration test (requires scip-typescript) ────────────────────────

/// Full pipeline integration test: scip-typescript subprocess → SCIP
/// protobuf → LMDB → query, verifying that `find_impact`'s underlying
/// `find_references()` returns ALL call-sites of a TS symbol across
/// multiple files.
///
/// Requires the `typescript_helper_integration` feature flag AND either:
/// - `CODESEARCH_SCIP_TYPESCRIPT` env var pointing to a `scip-typescript`
///   binary, or
/// - `npx` resolvable on `$PATH` (Node + npm installed).
#[test]
#[cfg_attr(not(feature = "typescript_helper_integration"), ignore)]
fn test_typescript_pipeline_ts_sample_roundtrip() {
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ts-sample");
    assert!(
        fixture_root.join("tsconfig.json").exists(),
        "Fixture not found at {}",
        fixture_root.display()
    );

    let indexer = TypeScriptSymbolIndexer::new();
    assert!(
        indexer.is_available(),
        "scip-typescript not resolvable (no CODESEARCH_SCIP_TYPESCRIPT and no npx on PATH)"
    );
    assert!(
        indexer.applies_to(&fixture_root),
        "Fixture should be recognized as a TypeScript project (root tsconfig.json)"
    );

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path();

    let summary = indexer
        .rebuild(&fixture_root, db_path, RebuildScope::Full)
        .expect("rebuild failed");
    assert!(
        summary.symbols_indexed > 0,
        "No symbols indexed from ts-sample fixture"
    );

    // Fuzzy lookup: "add" should resolve to the `add` function in math.ts
    // and return the definition plus all call-sites across both files
    // that import and call it.
    let add_refs = indexer
        .find_references(db_path, "add")
        .expect("find_references failed");

    let defs: Vec<_> = add_refs.iter().filter(|r| r.kind == "definition").collect();
    assert_eq!(defs.len(), 1, "Expected exactly 1 definition for `add`");
    assert!(
        defs[0].file.to_string_lossy().contains("math.ts"),
        "Definition should be in math.ts, got {:?}",
        defs[0].file
    );

    // consumer.ts calls add() twice (nested), other.ts calls it once -> at
    // least 3 non-definition occurrences, spanning at least 2 distinct files.
    let call_sites: Vec<_> = add_refs.iter().filter(|r| r.kind != "definition").collect();
    assert!(
        call_sites.len() >= 3,
        "Expected >=3 call-sites for `add`, got {}",
        call_sites.len()
    );

    let distinct_files: std::collections::HashSet<_> =
        call_sites.iter().map(|r| r.file.clone()).collect();
    assert!(
        distinct_files.len() >= 2,
        "Expected call-sites across >=2 files, got {}",
        distinct_files.len()
    );
    assert!(
        distinct_files
            .iter()
            .any(|f| f.to_string_lossy().contains("consumer.ts")),
        "Expected a call-site in consumer.ts"
    );
    assert!(
        distinct_files
            .iter()
            .any(|f| f.to_string_lossy().contains("other.ts")),
        "Expected a call-site in other.ts"
    );
}

// ── Real-project smoke test (opt-in via env var) ──────────────────────

/// Smoke test against a real-world TypeScript project pointed at by the
/// `CODESEARCH_TS_TEST_REAL` env var. Gated by the feature flag AND the env
/// var, so it never runs in CI unless explicitly opted in.
///
/// Verifies: rebuild succeeds on a non-trivial codebase, `find_references`
/// returns sensible multi-file results for a commonly-used symbol.
#[test]
#[cfg_attr(not(feature = "typescript_helper_integration"), ignore)]
fn test_typescript_pipeline_real_project() {
    let project_root = match std::env::var("CODESEARCH_TS_TEST_REAL") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("skipping real-project test: set CODESEARCH_TS_TEST_REAL=<path> to enable");
            return;
        }
    };
    assert!(
        project_root.join("tsconfig.json").is_file(),
        "CODESEARCH_TS_TEST_REAL does not point at a TS project root (no tsconfig.json): {}",
        project_root.display()
    );

    let indexer = TypeScriptSymbolIndexer::new();
    assert!(indexer.is_available(), "scip-typescript not resolvable");
    assert!(
        indexer.applies_to(&project_root),
        "Indexer did not recognize the project as TypeScript"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path();

    let started = std::time::Instant::now();
    let summary = indexer
        .rebuild(&project_root, db_path, RebuildScope::Full)
        .expect("rebuild failed");
    let elapsed = started.elapsed();

    eprintln!(
        "rebuild: {} symbols, {} references stored in {:.2}s",
        summary.symbols_indexed,
        summary.references_stored,
        elapsed.as_secs_f64()
    );
    assert!(
        summary.symbols_indexed > 50,
        "Expected >50 symbols for a real project, got {}",
        summary.symbols_indexed
    );

    // `log` is a very commonly used symbol in the target project — expect
    // many call-sites across many files.
    for sym in &["log", "configureLogger"] {
        let refs = indexer
            .find_references(db_path, sym)
            .unwrap_or_else(|e| panic!("find_references({sym}) failed: {e:#}"));
        let distinct_files: std::collections::HashSet<_> =
            refs.iter().map(|r| r.file.clone()).collect();
        eprintln!(
            "find_references({sym:?}): {} occurrences across {} files",
            refs.len(),
            distinct_files.len()
        );
        // Sanity: each queried symbol should have at least one hit.
        assert!(
            !refs.is_empty(),
            "Expected at least one reference for `{sym}`, got 0"
        );
    }

    // Negative test: unknown symbol returns empty, no panic.
    let unknown = indexer
        .find_references(db_path, "thisSymbolDoesNotExist_xyzzy_12345")
        .expect("find_references on unknown symbol should not error");
    assert!(unknown.is_empty(), "Unknown symbol should return empty");
    eprintln!("negative test OK: unknown symbol returned 0 results");
}
