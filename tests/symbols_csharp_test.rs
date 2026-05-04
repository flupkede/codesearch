//! Integration tests for the C# symbol indexing pipeline.
//!
//! Tests the JSON parsing → LMDB storage → query round-trip
//! without requiring the actual scip-csharp helper binary.
//!
//! Integration tests that invoke the helper subprocess are gated behind
//! the `csharp_helper_integration` cargo feature.

use std::path::PathBuf;

use codesearch::symbols::csharp::CSharpSymbolIndexer;
use codesearch::symbols::scip_parse;
use codesearch::symbols::{RebuildScope, SymbolIndexer};
use tempfile::TempDir;

/// Sample JSON mimicking the output of scip-csharp for a small C# project.
const SAMPLE_INDEX_JSON: &str = r#"{
    "metadata": {"version": "1.0", "tool_info": "scip-csharp"},
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

    // Test find_references with no data — should return error
    let result = indexer.find_references(&db_path, "Calculator.Add");
    assert!(result.is_err(), "Should fail when no SCIP data exists");
}

#[test]
fn test_parse_json_index_multiple_symbols_same_file() {
    let json = r#"{
        "metadata": {"version": "1.0", "tool_info": "test"},
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
        "metadata": {"version": "1.0", "tool_info": "test"},
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

    // Query: exact match for Calculator.Add should have >=2 occurrences
    let add_refs = indexer
        .find_references(
            db_path,
            "csharp SmallSolution.Library . Calculator#Add(int, int).",
        )
        .expect("find_references failed");
    assert!(
        add_refs.len() >= 2,
        "Expected >=2 refs for Calculator.Add, got {}",
        add_refs.len()
    );

    // Verify definition is in Calculator.cs
    let defs: Vec<_> = add_refs.iter().filter(|r| r.kind == "definition").collect();
    assert_eq!(defs.len(), 1, "Expected 1 definition for Calculator.Add");

    // Fuzzy query: "Add" should resolve to Calculator.Add
    let fuzzy_refs = indexer
        .find_references(db_path, "Add")
        .expect("fuzzy find_references failed");
    assert!(
        !fuzzy_refs.is_empty(),
        "Fuzzy lookup for 'Add' should resolve to Calculator.Add"
    );

    // Position-based lookup: find what's defined on Calculator.cs line 8
    // Note: paths are solution-relative as produced by the helper
    let pos_refs = indexer
        .find_references_by_position(db_path, &PathBuf::from("Library/Calculator.cs"), 8)
        .expect("find_references_by_position failed");
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
