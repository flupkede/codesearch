//! `resolve_query` semantics for the TypeScript adapter (hand-populated
//! LMDB — no `scip-typescript` helper). Mirrors the C# fixture: two
//! `add` "overloads" sharing a simple name, one unique `mul`.

use super::*;

/// Writes scip_symbols / scip_simple_names / scip_positions directly and
/// returns the three canonical keys it stored.
fn populate_ambiguity_fixture(db_path: &Path) -> (String, String, String) {
    let env = crate::symbols::get_shared_scip_env(db_path).expect("shared env");
    let mut wtxn = env.write_txn().expect("wtxn");

    let add1 = "scip-typescript npm mypkg 1.0.0 `src/math.ts`/add().".to_string();
    let add2 = "scip-typescript npm mypkg 1.0.0 `src/math.ts`/add(`x`: number).".to_string();
    let mul = "scip-typescript npm mypkg 1.0.0 `src/math.ts`/mul().".to_string();

    let symbols = crate::lmdb_registry::open_named_db::<Str, Bytes>(&env, SCIP_DB_NAME)
        .unwrap()
        .unwrap();
    for key in [&add1, &add2, &mul] {
        let refs = serialize_refs(&[StoredReference {
            file: PathBuf::from("src/math.ts"),
            start_line: 1,
            end_line: 1,
            kind: "definition".into(),
        }])
        .unwrap();
        symbols.put(&mut wtxn, key.as_str(), &refs).unwrap();
    }

    let names = crate::lmdb_registry::open_named_db::<Str, Bytes>(&env, SCIP_NAMES_DB_NAME)
        .unwrap()
        .unwrap();
    // Stored deliberately out of order: resolution must sort.
    names
        .put(
            &mut wtxn,
            "add",
            &serialize_keys_v1(&[add2.clone(), add1.clone()]).unwrap(),
        )
        .unwrap();
    names
        .put(
            &mut wtxn,
            "mul",
            &serialize_keys_v1(std::slice::from_ref(&mul)).unwrap(),
        )
        .unwrap();

    let positions = crate::lmdb_registry::open_named_db::<Str, Bytes>(&env, SCIP_POS_DB_NAME)
        .unwrap()
        .unwrap();
    positions
        .put(
            &mut wtxn,
            "src/math.ts:10",
            &serialize_keys_v1(&[add1.clone(), add2.clone()]).unwrap(),
        )
        .unwrap();
    positions
        .put(
            &mut wtxn,
            "src/math.ts:20",
            &serialize_keys_v1(std::slice::from_ref(&mul)).unwrap(),
        )
        .unwrap();

    wtxn.commit().unwrap();
    (add1, add2, mul)
}

#[test]
fn resolve_name_unique_fuzzy_resolves_the_single_candidate() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("db");
    let (_a1, _a2, mul) = populate_ambiguity_fixture(&db);
    let indexer = TypeScriptSymbolIndexer::new();
    assert_eq!(
        indexer
            .resolve_query(&db, &ImpactQuery::Name("mul".into()))
            .unwrap(),
        KeyMatch::Resolved(mul)
    );
}

#[test]
fn resolve_name_overloads_come_back_ambiguous_and_sorted() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("db");
    let (a1, a2, _mul) = populate_ambiguity_fixture(&db);
    let indexer = TypeScriptSymbolIndexer::new();
    // The pre-fix behaviour silently picked the shortest key here and
    // answered about the wrong overload.
    assert_eq!(
        indexer
            .resolve_query(&db, &ImpactQuery::Name("add".into()))
            .unwrap(),
        KeyMatch::Ambiguous(vec![a1, a2])
    );
}

#[test]
fn resolve_exact_key_is_verbatim_and_never_fuzzy() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("db");
    let (a1, _a2, _mul) = populate_ambiguity_fixture(&db);
    let indexer = TypeScriptSymbolIndexer::new();
    assert_eq!(
        indexer
            .resolve_query(&db, &ImpactQuery::ExactKey(a1.clone()))
            .unwrap(),
        KeyMatch::Resolved(a1)
    );
    // A key that is only a prefix of a stored one must miss.
    assert_eq!(
        indexer
            .resolve_query(
                &db,
                &ImpactQuery::ExactKey("scip-typescript npm mypkg 1.0.0 `src/math.ts`/add".into())
            )
            .unwrap(),
        KeyMatch::NotFound
    );
}

#[test]
fn resolve_position_single_resolves_two_symbols_are_ambiguous() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("db");
    let (a1, a2, mul) = populate_ambiguity_fixture(&db);
    let indexer = TypeScriptSymbolIndexer::new();

    assert_eq!(
        indexer
            .resolve_query(
                &db,
                &ImpactQuery::Position {
                    file: PathBuf::from("src/math.ts"),
                    line: 20
                }
            )
            .unwrap(),
        KeyMatch::Resolved(mul)
    );
    assert_eq!(
        indexer
            .resolve_query(
                &db,
                &ImpactQuery::Position {
                    file: PathBuf::from("src/math.ts"),
                    line: 10
                }
            )
            .unwrap(),
        KeyMatch::Ambiguous(vec![a1, a2])
    );
    assert_eq!(
        indexer
            .resolve_query(
                &db,
                &ImpactQuery::Position {
                    file: PathBuf::from("src/math.ts"),
                    line: 99
                }
            )
            .unwrap(),
        KeyMatch::NotFound
    );
}

#[test]
fn references_for_key_returns_stored_definitions() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("db");
    let (a1, _a2, _mul) = populate_ambiguity_fixture(&db);
    let indexer = TypeScriptSymbolIndexer::new();
    let refs = indexer.find_references_for_key(&db, &a1).unwrap();
    assert_eq!(refs.len(), 1, "definitions only, got {refs:?}");
    assert_eq!(refs[0].kind, "definition");
    assert_eq!(refs[0].file, PathBuf::from("src/math.ts"));
}

#[test]
fn lookup_warnings_reads_the_index_warnings_meta_entry() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("db");
    let (a1, _a2, _mul) = populate_ambiguity_fixture(&db);
    let indexer = TypeScriptSymbolIndexer::new();

    // No entry yet (pre-warnings index) → empty = complete.
    assert!(indexer.lookup_warnings(&db, &a1).is_empty());

    // Hand-populate the meta entry exactly the way the rebuild flow writes
    // it: a JSON array under the namespaced meta key.
    let env = crate::symbols::get_shared_scip_env(&db).unwrap();
    let meta = crate::lmdb_registry::open_named_db::<Str, Str>(&env, SCIP_META_DB_NAME)
        .unwrap()
        .unwrap();
    let mut wtxn = env.write_txn().unwrap();
    meta.put(
        &mut wtxn,
        META_INDEX_WARNINGS,
        r#"["scip-typescript exited with 1 for /repo — index may be incomplete"]"#,
    )
    .unwrap();
    wtxn.commit().unwrap();

    let warnings = indexer.lookup_warnings(&db, &a1);
    assert_eq!(warnings.len(), 1, "the stored warning must be read back");
    assert!(
        warnings[0].contains("scip-typescript exited with 1"),
        "warning text must round-trip, got: {}",
        warnings[0]
    );

    // The entry is per-index, not per-symbol: every key reports it.
    assert_eq!(
        indexer
            .lookup_warnings(&db, "scip-typescript npm mypkg 1.0.0 `src/other.ts`/x().")
            .len(),
        1
    );

    // An empty array (what a clean reindex writes) must read as complete.
    let meta = crate::lmdb_registry::open_named_db::<Str, Str>(&env, SCIP_META_DB_NAME)
        .unwrap()
        .unwrap();
    let mut wtxn = env.write_txn().unwrap();
    meta.put(&mut wtxn, META_INDEX_WARNINGS, "[]").unwrap();
    wtxn.commit().unwrap();
    assert!(
        indexer.lookup_warnings(&db, &a1).is_empty(),
        "an empty warnings array must mean complete"
    );
}

// ── B3 warnings: the rebuild's meta-write site, driven for real ──────
//
// The test above hand-writes the META_INDEX_WARNINGS entry (consumer
// half). This one drives the producer: a helper that exits non-zero, so
// the rebuild flow must persist the completeness warning itself.

/// A fake `scip-typescript` with the partial-output contract: exits
/// non-zero AFTER writing the `--output` file, so the rebuild must both
/// capture the warning and parse the (here empty — a valid default index)
/// output. `resolve_helper` only checks `is_file()`, so a shell script is
/// accepted as the env-var helper.
fn write_failing_helper(dir: &Path) -> PathBuf {
    let (path, script) = if cfg!(windows) {
        (
            dir.join("fake-scip-typescript.cmd"),
            "@echo off\r\ntype nul > \"%~3\"\r\nexit /b 3\r\n".to_string(),
        )
    } else {
        (
            dir.join("fake-scip-typescript"),
            "#!/bin/sh\n: > \"$3\"\nexit 3\n".to_string(),
        )
    };
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

#[test]
#[serial_test::serial]
fn rebuild_persists_the_nonzero_exit_warning_into_the_meta_table() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    let db = dir.path().join("db");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("tsconfig.json"), "{}").unwrap();

    let helper_bin = dir.path().join("helper-bin");
    std::fs::create_dir(&helper_bin).unwrap();
    let helper = write_failing_helper(&helper_bin);
    let _guard = crate::testing::EnvRestore::set(&[(
        SCIP_TYPESCRIPT_HELPER_ENV,
        helper.to_string_lossy().as_ref(),
    )]);

    let indexer = TypeScriptSymbolIndexer::new();
    let any_key = "scip-typescript npm mypkg 1.0.0 `src/x.ts`/f().";
    assert!(indexer.lookup_warnings(&db, any_key).is_empty());

    // Non-zero helper exit must NOT fail the rebuild — partial output is
    // acceptable — but the warning must ride into the meta table.
    let summary = indexer
        .rebuild(&repo, &db, RebuildScope::Full)
        .expect("a failing helper must still complete the rebuild");
    assert_eq!(
        summary.symbols_indexed, 0,
        "the fake helper wrote an empty index, nothing else"
    );

    // THE PRODUCER ASSERTION: the warning from the non-zero exit was
    // persisted by the rebuild's META_INDEX_WARNINGS write.
    let warnings = indexer.lookup_warnings(&db, any_key);
    assert_eq!(
        warnings.len(),
        1,
        "the rebuild must persist the non-zero-exit warning"
    );
    assert!(
        warnings[0].contains("exit code: 3"),
        "warning text must name the exit status, got: {}",
        warnings[0]
    );
}

#[test]
fn exit_status_text_is_platform_stable() {
    // ExitStatus's Display prints "exit code: N" on Windows but
    // "exit status: N" on Unix — persisted warning text (and the test
    // above) must not drift with the host OS. Pin the helper on both.
    #[cfg(unix)]
    let status = std::os::unix::process::ExitStatusExt::from_raw(3 << 8);
    #[cfg(windows)]
    let status = std::os::windows::process::ExitStatusExt::from_raw(3);
    assert_eq!(crate::symbols::exit_status_text(&status), "exit code: 3");
}
