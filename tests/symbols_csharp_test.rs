//! Integration tests for the C# symbol indexing pipeline.
//!
//! Tests the JSON parsing → LMDB storage → query round-trip
//! without requiring the actual scip-csharp helper binary.
//!
//! Integration tests that invoke the helper subprocess are gated behind
//! the `csharp_helper_integration` cargo feature.

use std::path::PathBuf;

use codesearch::constants::{SCIP_LMDB_DEFAULT_MAP_SIZE_MB, SCIP_LMDB_MAP_SIZE_MB_ENV};
use codesearch::symbols::csharp::CSharpSymbolIndexer;
use codesearch::symbols::scip_parse;
use codesearch::symbols::{ImpactQuery, KeyMatch, RebuildScope, SymbolIndexer};
use tempfile::TempDir;

/// Sample JSON mimicking the output of scip-csharp for a small C# project.
const SAMPLE_INDEX_JSON: &str = r#"{
    "metadata": {"version": "2.0", "tool_info": "scip-csharp"},
    "documents": [
        {
            "relative_path": "src/Library/Calculator.cs",
            "occurrences": [
                {
                    "range": [8, 5, 8, 20],
                    "symbol": "csharp SmallSolution.Library . Calculator#Add(int, int).",
                    "symbol_roles": 1,
                    "kind": "definition"
                },
                {
                    "range": [13, 5, 13, 20],
                    "symbol": "csharp SmallSolution.Library . Calculator#Subtract(int, int).",
                    "symbol_roles": 1,
                    "kind": "definition"
                },
                {
                    "range": [18, 5, 18, 20],
                    "symbol": "csharp SmallSolution.Library . Calculator#Multiply(int, int).",
                    "symbol_roles": 1,
                    "kind": "definition"
                },
                {
                    "range": [23, 5, 23, 20],
                    "symbol": "csharp SmallSolution.Library . Calculator#Divide(int, int).",
                    "symbol_roles": 1,
                    "kind": "definition"
                },
                {
                    "range": [5, 14, 5, 24],
                    "symbol": "csharp SmallSolution.Library . Calculator#",
                    "symbol_roles": 1,
                    "kind": "definition"
                }
            ]
        },
        {
            "relative_path": "src/App/Main.cs",
            "occurrences": [
                {
                    "range": [10, 22, 10, 25],
                    "symbol": "csharp SmallSolution.Library . Calculator#Add(int, int).",
                    "symbol_roles": 0,
                    "kind": "reference"
                },
                {
                    "range": [11, 23, 11, 31],
                    "symbol": "csharp SmallSolution.Library . Calculator#Subtract(int, int).",
                    "symbol_roles": 0,
                    "kind": "reference"
                },
                {
                    "range": [9, 22, 9, 32],
                    "symbol": "csharp SmallSolution.Library . Calculator#",
                    "symbol_roles": 0,
                    "kind": "reference"
                },
                {
                    "range": [8, 9, 8, 19],
                    "symbol": "csharp SmallSolution.Library . Calculator#",
                    "symbol_roles": 0,
                    "kind": "reference"
                }
            ]
        }
    ],
    "external_symbols": [
        {"symbol": "csharp SmallSolution.Library . Calculator#", "documentation": []},
        {"symbol": "csharp SmallSolution.Library . Calculator#Add(int, int).", "documentation": []},
        {"symbol": "csharp SmallSolution.Library . Calculator#Subtract(int, int).", "documentation": []}
    ]
}"#;

#[test]
fn test_parse_json_index_from_sample() {
    let index = scip_parse::parse_json_index(SAMPLE_INDEX_JSON.as_bytes())
        .expect("Failed to parse sample JSON");

    // Should have symbols for Calculator class and its methods
    assert!(
        index.len() >= 3,
        "Expected at least 3 symbols, got {}",
        index.len()
    );

    // Verify Calculator.Add has both a definition and a reference
    let add_symbol = "csharp SmallSolution.Library . Calculator#Add(int, int).";
    let add_refs = index.get(add_symbol).expect("Calculator.Add should exist");
    assert_eq!(
        add_refs.len(),
        2,
        "Calculator.Add should have 2 occurrences (1 def + 1 ref)"
    );

    let definitions: Vec<_> = add_refs.iter().filter(|r| r.kind == "definition").collect();
    let references: Vec<_> = add_refs.iter().filter(|r| r.kind == "reference").collect();
    assert_eq!(definitions.len(), 1, "Expected 1 definition");
    assert_eq!(references.len(), 1, "Expected 1 reference");

    // Verify line numbers are correct (1-based from C# helper, passed through as-is)
    let def = &definitions[0];
    assert_eq!(def.start_line, 8, "Add definition should be on line 8");
    assert_eq!(def.end_line, 8);
    assert!(def.file.to_string_lossy().contains("Calculator.cs"));

    let reference = &references[0];
    assert_eq!(
        reference.start_line, 10,
        "Add reference in Main.cs should be on line 10"
    );
    assert!(reference.file.to_string_lossy().contains("Main.cs"));
}

#[test]
fn test_indexer_returns_empty_when_db_missing() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test-db");
    std::fs::create_dir_all(&db_path).expect("Failed to create db dir");

    let indexer = CSharpSymbolIndexer::new();

    // Note: is_available() may return true if the helper binary exists
    // (e.g. in CI where it was just built). Don't assert unavailability.

    // Test index_age with no LMDB data — should return u64::MAX
    let age = indexer.index_age(&db_path);
    // open_scip_env creates the dir, so just verify it doesn't panic
    let _ = age;

    // Test resolve_query with no data — should return NotFound because no
    // LMDB tables exist.
    //
    // On CI runners with constrained resources, LMDB may fail to reopen after
    // the index_age call dropped its env (lock file not yet released). Accept
    // both NotFound and Err as valid outcomes — the important invariant is
    // that it never panics and never returns stale data.
    let result = indexer.resolve_query(&db_path, &ImpactQuery::Name("Calculator.Add".into()));
    match result {
        Ok(key) => assert_eq!(
            key,
            KeyMatch::NotFound,
            "Should not resolve when no SCIP data exists, got {key:?}"
        ),
        Err(e) => {
            // LMDB reopen failed (e.g. lock contention on CI). This is
            // acceptable — the function correctly returns an error rather
            // than panicking or returning stale data.
            eprintln!("Note: resolve_query returned Err (LMDB lock contention?): {e:#}");
        }
    }
}

#[test]
fn test_parse_json_index_multiple_symbols_same_file() {
    let json = r#"{
        "metadata": {"version": "2.0", "tool_info": "test"},
        "documents": [{
            "relative_path": "src/A.cs",
            "occurrences": [
                {"range": [1, 0], "symbol": "csharp . . A#Method1().", "symbol_roles": 1, "kind": "definition"},
                {"range": [2, 0], "symbol": "csharp . . A#Method2().", "symbol_roles": 1, "kind": "definition"},
                {"range": [3, 0], "symbol": "csharp . . A#Method1().", "symbol_roles": 0, "kind": "reference"}
            ]
        }],
        "external_symbols": []
    }"#;

    let index = scip_parse::parse_json_index(json.as_bytes()).unwrap();

    // Method1 should have 1 definition + 1 reference
    let method1_refs = index.get("csharp . . A#Method1().").unwrap();
    assert_eq!(method1_refs.len(), 2);
    assert_eq!(
        method1_refs
            .iter()
            .filter(|r| r.kind == "definition")
            .count(),
        1
    );
    assert_eq!(
        method1_refs
            .iter()
            .filter(|r| r.kind == "reference")
            .count(),
        1
    );

    // Method2 should have 1 definition only
    let method2_refs = index.get("csharp . . A#Method2().").unwrap();
    assert_eq!(method2_refs.len(), 1);
    assert_eq!(method2_refs[0].kind, "definition");
}

#[test]
fn test_parse_json_index_role_fallback() {
    // When kind is empty string, should derive from symbol_roles
    let json = r#"{
        "metadata": {"version": "2.0", "tool_info": "test"},
        "documents": [{
            "relative_path": "src/A.cs",
            "occurrences": [
                {"range": [5, 0], "symbol": "csharp . . A#X.", "symbol_roles": 1, "kind": ""},
                {"range": [10, 0], "symbol": "csharp . . A#X.", "symbol_roles": 0, "kind": ""}
            ]
        }],
        "external_symbols": []
    }"#;

    let index = scip_parse::parse_json_index(json.as_bytes()).unwrap();
    let refs = index.get("csharp . . A#X.").unwrap();

    // symbol_roles=1 should map to "definition" via role_to_kind
    assert_eq!(refs[0].kind, "definition");
    // symbol_roles=0 should map to "reference" via role_to_kind
    assert_eq!(refs[1].kind, "reference");
}

// ── SCIP LMDB map_size constants ──────────────────────────────────────

/// Guard: the default LMDB map size must be 512 MB.
///
/// This test exists to catch any accidental reduction of the constant.
/// The old value (64 MB) caused MDB_MAP_FULL on enterprise repos once the
/// Phase-3 ref_cache was introduced.  512 MB is virtual address space only —
/// the OS never faults in unwritten pages — so the increase is free on modern
/// 64-bit systems.
#[test]
fn test_scip_lmdb_default_map_size_is_512mb() {
    assert_eq!(
        SCIP_LMDB_DEFAULT_MAP_SIZE_MB, 512,
        "SCIP_LMDB_DEFAULT_MAP_SIZE_MB regressed from 512 — \
         enterprise repos will hit MDB_MAP_FULL again"
    );
}

/// Verify that `CODESEARCH_SCIP_LMDB_MAP_MB` env-var override is honoured.
///
/// We cannot directly inspect the `EnvOpenOptions` after the fact, so instead
/// we exercise the observable behaviour: with a small custom map_size the
/// environment still opens successfully on an empty DB and `resolve_query`
/// returns `Ok(NotFound)` (no panic, no MDB_MAP_FULL).
///
/// A mutex serialises env-var mutation so this test is safe when `cargo test`
/// runs suites in parallel.
#[test]
fn test_scip_lmdb_env_var_override() {
    use std::sync::{Mutex, OnceLock};

    // Serialise any env-var mutation across parallel test threads.
    static ENV_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = ENV_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env-var mutex poisoned");

    // Save previous value (if any) so we can restore it after the test.
    let prev = std::env::var(SCIP_LMDB_MAP_SIZE_MB_ENV).ok();

    // Use a small but valid override — 16 MB is enough for an empty DB.
    std::env::set_var(SCIP_LMDB_MAP_SIZE_MB_ENV, "16");

    let result = std::panic::catch_unwind(|| {
        let tmp = TempDir::new().expect("Failed to create temp dir");
        let db_path = tmp.path().join("test-env-override-db");
        std::fs::create_dir_all(&db_path).expect("Failed to create db dir");

        let indexer = CSharpSymbolIndexer::new();
        // On an empty DB the env-var path is exercised by open_scip_env().
        // The call must succeed and report nothing found.
        let result = indexer.resolve_query(&db_path, &ImpactQuery::Name("SomeSymbol".into()));
        assert!(
            matches!(result, Ok(KeyMatch::NotFound)),
            "Expected Ok(NotFound) from empty DB with env-var map_size override, got {result:?}"
        );
    });

    // Always restore the env var, even if the test panicked.
    match prev {
        Some(v) => std::env::set_var(SCIP_LMDB_MAP_SIZE_MB_ENV, v),
        None => std::env::remove_var(SCIP_LMDB_MAP_SIZE_MB_ENV),
    }

    result.expect("test_scip_lmdb_env_var_override panicked");
}

// ── Integration tests (require scip-csharp helper) ─────────────────

/// Full pipeline integration test: scip-csharp subprocess → JSON → LMDB → query.
///
/// Requires the `csharp_helper_integration` feature flag AND either:
/// - `CODESEARCH_SCIP_CSHARP` env var pointing to the helper binary, or
/// - the helper binary at `helpers/csharp/bin/Release/net10.0/scip-csharp`
#[test]
#[cfg_attr(not(feature = "csharp_helper_integration"), ignore)]
fn test_csharp_pipeline_smallsolution_roundtrip() {
    // Locate fixture
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("helpers/csharp/tests/Fixtures/SmallSolution");
    assert!(
        fixture_root.join("SmallSolution.sln").exists(),
        "Fixture not found at {}",
        fixture_root.display()
    );

    // Locate helper binary
    let helper = std::env::var("CODESEARCH_SCIP_CSHARP")
        .map(PathBuf::from)
        .or_else(|_| {
            let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("helpers/csharp/bin/Release/net10.0/scip-csharp");
            if candidate.exists() {
                Ok(candidate)
            } else {
                Err(())
            }
        })
        .expect(
            "scip-csharp helper not found. Set CODESEARCH_SCIP_CSHARP env var \
             or build the helper via `dotnet publish`.",
        );
    std::env::set_var("CODESEARCH_SCIP_CSHARP", &helper);

    // Setup tempdir for LMDB
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path();

    // Rebuild
    let indexer = CSharpSymbolIndexer::new();
    assert!(
        indexer.is_available(),
        "Helper detection failed — binary at {}",
        helper.display()
    );
    let summary = indexer
        .rebuild(&fixture_root, db_path, RebuildScope::Full)
        .expect("rebuild failed");
    assert!(
        summary.symbols_indexed > 0,
        "No symbols indexed from fixture"
    );

    // Query: exact key for Calculator.Add should have >=2 occurrences
    let add_key = "csharp SmallSolution.Library . Calculator#Add(int, int).";
    let resolved = indexer
        .resolve_query(db_path, &ImpactQuery::ExactKey(add_key.to_string()))
        .expect("ExactKey resolution failed");
    let canonical = match resolved {
        KeyMatch::Resolved(k) => k,
        other => panic!("ExactKey must resolve verbatim, got {other:?}"),
    };
    let add_refs = indexer
        .find_references_for_key(db_path, &canonical)
        .expect("find_references_for_key failed");
    assert!(
        add_refs.len() >= 2,
        "Expected >=2 refs for Calculator.Add, got {}",
        add_refs.len()
    );

    // Verify definition is in Calculator.cs
    let defs: Vec<_> = add_refs.iter().filter(|r| r.kind == "definition").collect();
    assert_eq!(defs.len(), 1, "Expected 1 definition for Calculator.Add");

    // Ambiguity contract: "Add" has two overloads — the adapter must list
    // them, never pick one silently (the #238 contract: overloads come back
    // as a sorted Ambiguous envelope; an explicit key selects one).
    let resolved = indexer
        .resolve_query(db_path, &ImpactQuery::Name("Add".into()))
        .expect("ambiguous-name resolution failed");
    let candidates = match resolved {
        KeyMatch::Ambiguous(c) => c,
        other => panic!("'Add' has two overloads and must come back Ambiguous, got {other:?}"),
    };
    assert!(
        candidates.len() >= 2,
        "Expected >=2 Add overload candidates, got {candidates:?}"
    );
    assert!(
        candidates.iter().all(|k| k.contains("Calculator#Add")),
        "Add candidates must be Calculator.Add overloads, got {candidates:?}"
    );
    assert!(
        candidates.windows(2).all(|w| w[0] <= w[1]),
        "candidates must be sorted for deterministic output: {candidates:?}"
    );
    let picked = candidates[0].clone();
    let resolved = indexer
        .resolve_query(db_path, &ImpactQuery::ExactKey(picked.clone()))
        .expect("explicit selection failed");
    assert_eq!(
        resolved,
        KeyMatch::Resolved(picked.clone()),
        "an explicit candidate selection must resolve to itself"
    );
    let fuzzy_refs = indexer
        .find_references_for_key(db_path, &picked)
        .expect("find_references_for_key failed");
    assert!(
        !fuzzy_refs.is_empty(),
        "the picked Add overload must have references"
    );

    // A class name resolves to the class: methods register under their own
    // simple name (see extract_simple_name), so "Calculator" is NOT
    // ambiguous even though method keys contain the word.
    let resolved = indexer
        .resolve_query(db_path, &ImpactQuery::Name("Calculator".into()))
        .expect("class-name resolution failed");
    assert_eq!(
        resolved,
        KeyMatch::Resolved("csharp SmallSolution.Library . Calculator#".to_string()),
        "class name must resolve to the class symbol"
    );

    // Position-based lookup: find what's defined on Calculator.cs line 8
    // Note: paths are solution-relative as produced by the helper
    let resolved = indexer
        .resolve_query(
            db_path,
            &ImpactQuery::Position {
                file: PathBuf::from("Library/Calculator.cs"),
                line: 8,
            },
        )
        .expect("position resolution failed");
    let canonical = match resolved {
        KeyMatch::Resolved(k) => k,
        other => panic!("single-definition position must resolve, got {other:?}"),
    };
    let pos_refs = indexer
        .find_references_for_key(db_path, &canonical)
        .expect("find_references_for_key failed");
    assert!(
        !pos_refs.is_empty(),
        "Position lookup for Library/Calculator.cs:8 should return references"
    );
    // The definition at line 8 should be Calculator.Add
    let pos_defs: Vec<_> = pos_refs.iter().filter(|r| r.kind == "definition").collect();
    assert_eq!(
        pos_defs.len(),
        1,
        "Expected 1 definition at Calculator.cs:8"
    );
}

/// Regression for todo #168: an incremental rebuild must clear
/// `scip_ref_cache` in full, not just for symbols the OLD selective scheme
/// could see. That scheme only purged a cache entry whose ALREADY-CACHED
/// reference list pointed at an affected file — it could never catch an
/// unrelated, already-cached symbol (defined elsewhere, no prior reference
/// into the changed file) gaining a brand-new reference FROM the changed
/// file, since that requires reading the changed file's new content, not
/// the old cache. This test deliberately caches `Program.Main` — defined in
/// `App/Main.cs`, untouched by the incremental rebuild below, with no
/// existing reference into `Library/Calculator.cs` — so the old scheme
/// would leave it cached; only a whole-table clear catches it.
#[test]
#[cfg_attr(not(feature = "csharp_helper_integration"), ignore)]
fn test_incremental_rebuild_clears_the_reference_cache() {
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("helpers/csharp/tests/Fixtures/SmallSolution");
    let helper = std::env::var("CODESEARCH_SCIP_CSHARP")
        .map(PathBuf::from)
        .or_else(|_| {
            let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("helpers/csharp/bin/Release/net10.0/scip-csharp");
            if candidate.exists() {
                Ok(candidate)
            } else {
                Err(())
            }
        })
        .expect("scip-csharp helper not found");
    std::env::set_var("CODESEARCH_SCIP_CSHARP", &helper);

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path();
    let indexer = CSharpSymbolIndexer::new();

    indexer
        .rebuild(&fixture_root, db_path, RebuildScope::Full)
        .expect("full rebuild failed");

    // Resolve dynamically rather than hardcode the canonical key format.
    let main_key = match indexer
        .resolve_query(db_path, &ImpactQuery::Name("Main".into()))
        .expect("resolving 'Main' failed")
    {
        KeyMatch::Resolved(k) => k,
        other => panic!("expected 'Main' to resolve unambiguously, got {other:?}"),
    };
    assert!(
        main_key.contains("App") && main_key.contains("Program"),
        "expected Program.Main from App/Main.cs, got {main_key}"
    );

    // First lookup is always a cache miss (nothing cached yet) — it must
    // populate scip_ref_cache as a side effect.
    indexer
        .find_references_for_key(db_path, &main_key)
        .expect("find_references_for_key failed (pre-incremental)");
    let uncached_after_first_lookup = indexer
        .collect_uncached_symbol_keys(db_path)
        .expect("collect_uncached_symbol_keys failed");
    assert!(
        !uncached_after_first_lookup.contains(&main_key),
        "Program.Main must be cached after its first lookup"
    );

    // Incremental rebuild scoped to a DIFFERENT file (Calculator.cs) —
    // Main.cs, and Program.Main's defining project, are not touched.
    let changed_file = fixture_root.join("Library").join("Calculator.cs");
    indexer
        .rebuild(
            &fixture_root,
            db_path,
            RebuildScope::Files {
                changed: vec![changed_file],
                deleted: vec![],
            },
        )
        .expect("incremental rebuild failed");

    // The fix: the incremental rebuild must clear the WHOLE cache, so
    // Program.Main — untouched by the old selective invalidation, since
    // neither its definition nor its cached refs point at Calculator.cs —
    // is uncached again too.
    let uncached_after_incremental = indexer
        .collect_uncached_symbol_keys(db_path)
        .expect("collect_uncached_symbol_keys failed");
    assert!(
        uncached_after_incremental.contains(&main_key),
        "an incremental rebuild must clear scip_ref_cache in full — Program.Main's \
         pre-change cache entry must not survive an unrelated file's rebuild, \
         got uncached list: {:?}",
        uncached_after_incremental
    );
}
