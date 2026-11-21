//! Tests for the `find_impact` wall-clock budget.
//!
//! `find_impact_with_budget` is generic over the lookup future, so these
//! tests plant a *sleeping handler* (a future that sleeps past the budget,
//! the same busy-serve simulation the proxy tests use) instead of a real
//! SCIP helper — the budget race is exercised in milliseconds. The env
//! resolution is tested separately against `resolve_find_impact_budget_secs`
//! with `#[serial]` + `EnvRestore` per the repo rule for env mutation.

use super::{
    find_impact_with_budget, missing_index_warning, resolve_find_impact_budget_secs,
    retain_owned_keys, ImpactLookupOutcome, SymbolIndexHeal,
};
use crate::constants::{DEFAULT_FIND_IMPACT_BUDGET_SECS, FIND_IMPACT_BUDGET_SECS_ENV};
use crate::symbols::{SymbolLookupBusy, SymbolReference};
use std::path::PathBuf;
use std::time::Duration;

/// A lookup future that sleeps then succeeds — the planted busy handler.
async fn sleeping_lookup(delay: Duration) -> anyhow::Result<Vec<SymbolReference>> {
    tokio::time::sleep(delay).await;
    Ok(vec![SymbolReference {
        file: PathBuf::from("src/x.rs"),
        start_line: 1,
        end_line: 2,
        kind: "definition".to_string(),
    }])
}

fn sample_state() -> String {
    "resolving 'Ns.I.M' via the csharp SCIP helper".to_string()
}

#[tokio::test]
async fn budget_overrun_returns_busy_with_wait_time() {
    // Handler sleeps 2s, budget is 1s → the race must fire busy at ~1s,
    // well before the handler completes.
    let outcome = find_impact_with_budget(
        1,
        sample_state(),
        sleeping_lookup(Duration::from_millis(2_000)),
    )
    .await;
    match outcome {
        ImpactLookupOutcome::Busy { state, waited_ms } => {
            assert_eq!(state, sample_state());
            assert!(
                (900..=2_000).contains(&waited_ms),
                "busy must fire at ~the 1s budget, waited_ms={waited_ms}"
            );
        }
        ImpactLookupOutcome::Done(_) => panic!("a 2s handler must overrun a 1s budget"),
    }
}

#[tokio::test]
async fn fast_lookup_completes_within_budget_passes_through() {
    let outcome = find_impact_with_budget(
        60,
        sample_state(),
        sleeping_lookup(Duration::from_millis(10)),
    )
    .await;
    match outcome {
        ImpactLookupOutcome::Done(Ok(refs)) => {
            assert_eq!(refs.len(), 1);
            assert_eq!(refs[0].file, PathBuf::from("src/x.rs"));
            assert_eq!(refs[0].kind, "definition");
        }
        _ => panic!("a 10ms handler must complete inside a 60s budget"),
    }
}

#[tokio::test]
async fn lookup_failure_passes_through_with_error_chain() {
    let outcome = find_impact_with_budget(60, sample_state(), async {
        tokio::time::sleep(Duration::from_millis(5)).await;
        Err(anyhow::anyhow!("helper exited 1").context("scip-csharp find-refs failed"))
    })
    .await;
    match outcome {
        // The soft-string failure render is Step 3 scope; here we only pin
        // that Done(Err) — not busy, not swallowed — reaches the handler.
        ImpactLookupOutcome::Done(Err(e)) => {
            let rendered = format!("{e:#}");
            assert!(
                rendered.contains("scip-csharp find-refs failed")
                    && rendered.contains("helper exited 1"),
                "error chain must survive: {rendered}"
            );
        }
        _ => panic!("a failing handler must surface as Done(Err)"),
    }
}

#[tokio::test]
async fn zero_budget_disables_the_race() {
    // 0 disables the budget (repo-wide convention), so even a slow handler
    // completes instead of being answered busy.
    let outcome = find_impact_with_budget(
        0,
        sample_state(),
        sleeping_lookup(Duration::from_millis(50)),
    )
    .await;
    assert!(matches!(outcome, ImpactLookupOutcome::Done(Ok(_))));
}

#[test]
fn busy_envelope_serializes_the_five_documented_fields() {
    let busy = SymbolLookupBusy {
        busy: true,
        state: "resolving 'Ns.I.M' via the csharp SCIP helper".to_string(),
        waited_ms: 60_012,
        advice: "retry the same call in ~60s".to_string(),
        retry_after_seconds: 45,
    };
    let json: serde_json::Value = serde_json::to_value(&busy).unwrap();
    assert_eq!(json["busy"], serde_json::Value::Bool(true));
    assert!(json["state"].is_string());
    assert_eq!(json["waited_ms"], serde_json::Value::from(60_012));
    assert_eq!(
        json["retry_after_seconds"],
        serde_json::Value::from(45),
        "retry_after_seconds must be present: harnesses branch on it instead of parsing prose"
    );
    let advice = json["advice"].as_str().unwrap();
    assert!(
        advice.contains("retry the same call in ~"),
        "advice must carry the retry hint: {advice}"
    );
    // Exactly the documented envelope shape — the five fields, no extras.
    // (Key ORDER is deliberately not asserted: serde_json::to_value routes
    // through a BTreeMap and re-sorts keys, so an order assertion here would
    // test serde's map type, not the handler. Field order on the wire comes
    // from struct serialization and is irrelevant to JSON consumers.)
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "advice",
            "busy",
            "retry_after_seconds",
            "state",
            "waited_ms"
        ]
    );
}

#[test]
#[serial_test::serial]
fn budget_env_overrides_parses_and_falls_back() {
    // Table-driven: (raw env value, expected seconds). Garbage and empty
    // fall back to the default rather than erroring or clamping to 0 —
    // 0 is a meaningful value ("disable"), so it must only ever come from
    // an explicit "0".
    let cases: &[(&str, u64)] = &[
        ("90", 90),
        ("0", 0),
        (" 45 ", 45),
        ("junk", DEFAULT_FIND_IMPACT_BUDGET_SECS),
        ("", DEFAULT_FIND_IMPACT_BUDGET_SECS),
        ("-5", DEFAULT_FIND_IMPACT_BUDGET_SECS),
    ];
    for (raw, expected) in cases {
        let _guard = crate::testing::EnvRestore::set(&[(FIND_IMPACT_BUDGET_SECS_ENV, raw)]);
        assert_eq!(
            resolve_find_impact_budget_secs(),
            *expected,
            "env value {raw:?} must resolve to {expected}"
        );
    }
    // Absent → documented default.
    let _guard = crate::testing::EnvRestore::remove(&[FIND_IMPACT_BUDGET_SECS_ENV]);
    assert_eq!(
        resolve_find_impact_budget_secs(),
        DEFAULT_FIND_IMPACT_BUDGET_SECS
    );
}

#[test]
fn failure_envelope_serializes_exactly_the_three_documented_fields() {
    let failure = crate::symbols::SymbolLookupFailure::failed("helper exited 1");
    let json: serde_json::Value = serde_json::to_value(&failure).unwrap();
    assert_eq!(json["class"], "failed");
    assert_eq!(json["error"], "helper exited 1");
    let hint = json["hint_for_agent"].as_str().unwrap();
    assert!(!hint.is_empty(), "hint must be non-empty: {hint}");
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["class", "error", "hint_for_agent"]);
}

#[test]
fn classify_maps_unknown_index_age_to_stale_and_readability_to_failed() {
    use crate::symbols::SymbolLookupFailureClass as Class;
    // (index_age_seconds, expected class) — u64::MAX is what `index_age`
    // returns whenever the index cannot be opened or read.
    let cases: &[(u64, Class)] = &[
        (u64::MAX, Class::Stale),
        (0, Class::Failed),
        (3_600, Class::Failed),
    ];
    for (age, expected) in cases {
        let failure = crate::symbols::SymbolLookupFailure::classify("chain", *age);
        assert_eq!(failure.class, *expected, "age {age} must classify");
        assert!(!failure.error.is_empty());
    }
}

#[test]
fn failure_hints_are_actionable_per_class() {
    let failed = crate::symbols::SymbolLookupFailure::failed("boom");
    let stale = crate::symbols::SymbolLookupFailure::stale("gone");
    assert!(
        failed.hint_for_agent.contains("usages"),
        "failed hint must point at the text-search fallback: {}",
        failed.hint_for_agent
    );
    assert!(
        stale.hint_for_agent.contains("index"),
        "stale hint must point at (re)building the index: {}",
        stale.hint_for_agent
    );
}

#[test]
fn fingerprint_fields_present_when_set_and_omitted_when_none() {
    use crate::symbols::SymbolReference;
    let base = |resolved_symbol: Option<String>,
                index_head_sha: Option<String>,
                current_head_sha: Option<String>| {
        crate::symbols::FindImpactResult {
            symbol: "FieldDefinition.Validate".to_string(),
            resolved_symbol,
            references: vec![SymbolReference {
                file: PathBuf::from("a.cs"),
                start_line: 1,
                end_line: 1,
                kind: "definition".to_string(),
            }],
            warnings: Vec::new(),
            index_age_seconds: 12,
            language: "csharp".to_string(),
            scope: "project:p".to_string(),
            index_head_sha,
            current_head_sha,
            symbol_index: Vec::new(),
        }
    };
    let all: serde_json::Value = serde_json::to_value(base(
        Some("csharp Ns . FieldDefinition#Validate().".to_string()),
        Some("a".repeat(40)),
        Some("b".repeat(40)),
    ))
    .unwrap();
    assert_eq!(all["index_head_sha"], "a".repeat(40));
    assert_eq!(all["current_head_sha"], "b".repeat(40));
    assert_eq!(
        all["resolved_symbol"], "csharp Ns . FieldDefinition#Validate().",
        "a resolved answer must name the selected canonical identity"
    );

    // None must OMIT the keys, not serialize null — old consumers see an
    // unchanged shape when the identity/fingerprint is unknown (ambiguous
    // and not-found answers travel in their own envelopes).
    let neither: serde_json::Value = serde_json::to_value(base(None, None, None)).unwrap();
    let keys: Vec<&str> = neither
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(!keys.contains(&"index_head_sha"), "keys: {keys:?}");
    assert!(!keys.contains(&"current_head_sha"), "keys: {keys:?}");
    assert!(!keys.contains(&"resolved_symbol"), "keys: {keys:?}");
}

#[test]
fn index_age_sentinel_serializes_as_null_and_real_ages_as_numbers() {
    use crate::symbols::{FindImpactResult, SymbolReference};

    let result = |age: u64| FindImpactResult {
        symbol: "FieldDefinition.Validate".to_string(),
        resolved_symbol: None,
        references: vec![SymbolReference {
            file: PathBuf::from("a.cs"),
            start_line: 1,
            end_line: 1,
            kind: "definition".to_string(),
        }],
        warnings: Vec::new(),
        index_age_seconds: age,
        language: "csharp".to_string(),
        scope: "project:p".to_string(),
        index_head_sha: None,
        current_head_sha: None,
        symbol_index: Vec::new(),
    };

    // u64::MAX is `index_age`'s unreadable-index sentinel; a consumer must
    // see "unknown" (null), not 18446744073709551615 seconds.
    let unknown: serde_json::Value = serde_json::to_value(result(u64::MAX)).unwrap();
    assert_eq!(unknown["index_age_seconds"], serde_json::Value::Null);

    let known: serde_json::Value = serde_json::to_value(result(12)).unwrap();
    assert_eq!(known["index_age_seconds"], serde_json::json!(12));
}

#[test]
fn ambiguity_envelope_serializes_the_four_documented_fields() {
    let ambiguity = crate::symbols::SymbolAmbiguity {
        ambiguous: true,
        query: "'Validate'".to_string(),
        candidates: vec![
            "csharp Ns . V#Validate().".to_string(),
            "csharp Ns . V#Validate(System.String).".to_string(),
        ],
        hint_for_agent: "re-call with symbol_key".to_string(),
    };
    let json: serde_json::Value = serde_json::to_value(&ambiguity).unwrap();
    assert_eq!(json["ambiguous"], serde_json::Value::Bool(true));
    assert_eq!(json["query"], "'Validate'");
    assert_eq!(json["candidates"].as_array().unwrap().len(), 2);
    assert!(!json["hint_for_agent"].as_str().unwrap().is_empty());
    // Exactly the documented envelope shape — the four fields, no extras.
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["ambiguous", "candidates", "hint_for_agent", "query"]
    );
}

// ── Handler-arm tests: the resolution contract through the tool method ──
//
// The adapter layer pins KeyMatch semantics; these pin the arms a caller
// actually sees: Ambiguous → typed candidates envelope, explicit-key miss
// → loud failure (never the empty answer), fuzzy NotFound → the
// historical empty-references contract, and the symbol_key combination
// rejection. All early-return before the budget race, so no busy paths
// are in play.

use super::CodesearchService;
use crate::mcp::types::FindImpactRequest;
use rmcp::handler::server::wrapper::Parameters;

async fn tool_text(service: &CodesearchService, req: FindImpactRequest) -> String {
    let result = service
        .find_impact(Parameters(req))
        .await
        .expect("tool result");
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .expect("text content")
        .text
        .clone()
}

fn find_impact_request() -> FindImpactRequest {
    FindImpactRequest {
        symbol_name: None,
        file: None,
        line: None,
        symbol_key: None,
        language: Some("csharp".to_string()),
        project: None,
        group: None,
        cwd: None,
    }
}

/// A service whose db is a valid-but-empty index at `<root>/.codesearch.db`.
fn build_service() -> (CodesearchService, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("tempdir");
    let db = root.path().join(".codesearch.db");
    std::fs::create_dir_all(&db).expect("db dir");
    std::fs::write(
        db.join("metadata.json"),
        r#"{"schema_version":1,"dimensions":2,"model_short_name":"minilm-l6-q"}"#,
    )
    .expect("metadata");
    let stores =
        std::sync::Arc::new(crate::index::SharedStores::new(&db, 2).expect("shared stores"));
    let service = CodesearchService::new_with_stores(Some(root.path().to_path_buf()), Some(stores))
        .expect("service");
    (service, root)
}

/// [`build_service`] plus a solution file, so the csharp indexer is
/// APPLICABLE and answers carry a real csharp `symbol_index` entry (the
/// plain fixture repo has no entrypoints, so both languages filter out).
fn build_applicable_service() -> (CodesearchService, tempfile::TempDir) {
    let (service, root) = build_service();
    std::fs::write(root.path().join("test.sln"), b"Solution File").expect("sln");
    (service, root)
}

/// Two `Validate` overloads in scip_symbols + the simple-name index —
/// enough for every resolution arm (only key-presence is read).
fn populate_overload_fixture(db_path: &std::path::Path) {
    let env = crate::symbols::get_shared_scip_env(db_path).expect("shared env");
    let v1 = "csharp Ns . V#Validate().".to_string();
    let v2 = "csharp Ns . V#Validate(System.String).".to_string();
    let symbols = crate::lmdb_registry::open_named_db::<heed::types::Str, heed::types::Bytes>(
        &env,
        crate::constants::SCIP_SYMBOLS_DB_NAME,
    )
    .unwrap()
    .unwrap();
    let names = crate::lmdb_registry::open_named_db::<heed::types::Str, heed::types::Bytes>(
        &env,
        crate::constants::SCIP_SIMPLE_NAMES_DB_NAME,
    )
    .unwrap()
    .unwrap();
    let mut wtxn = env.write_txn().expect("wtxn");
    for key in [&v1, &v2] {
        symbols.put(&mut wtxn, key.as_str(), &[1u8]).unwrap();
    }
    let mut payload = vec![1u8];
    payload.extend_from_slice(
        &bincode::serde::encode_to_vec(vec![v1.clone(), v2.clone()], bincode::config::legacy())
            .unwrap(),
    );
    names.put(&mut wtxn, "Validate", &payload).unwrap();
    wtxn.commit().unwrap();
}

/// The C# indexer must pass the `is_available` gate deterministically:
/// a dummy file with the expected helper filename, wired via the env
/// override (serialised — env mutation, per the repo rule).
fn make_helper_available(root: &tempfile::TempDir) -> crate::testing::EnvRestore {
    let helper = root.path().join(if cfg!(windows) {
        "scip-csharp.exe"
    } else {
        "scip-csharp"
    });
    std::fs::write(&helper, b"dummy").expect("dummy helper file");
    crate::testing::EnvRestore::set(&[(
        crate::constants::SCIP_CSHARP_HELPER_ENV,
        helper.to_string_lossy().as_ref(),
    )])
}

#[test]
fn current_git_head_resolves_the_crate_checkout() {
    // The crate dir is always a git checkout (build.rs already depends on
    // git metadata), so this is deterministic in dev and CI alike.
    let head = crate::symbols::current_git_head(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    let sha = head.expect("crate checkout must resolve a HEAD sha");
    assert_eq!(sha.len(), 40, "full sha expected: {sha}");
    assert!(
        sha.chars().all(|c| c.is_ascii_hexdigit()),
        "hex sha expected: {sha}"
    );
}

#[test]
fn current_git_head_is_none_outside_a_repo() {
    // A directory that is not a git repo must yield None, not an error —
    // the fingerprint is best-effort by design.
    let tmp = std::env::temp_dir().join("codesearch_no_git_head_check");
    let _ = std::fs::create_dir_all(&tmp);
    assert!(crate::symbols::current_git_head(&tmp).is_none());
}

#[test]
fn dedupe_references_collapses_identical_entries_and_keeps_distinct_ones() {
    // Live observation (2026-08-31): a definition arrived 5× — the SCIP
    // find-refs output emits multiple occurrences at the same file:line for
    // the declaring symbol. Identical (file, line range, kind) entries carry
    // no separately-actionable information and collapse to the first.
    let r = |file: &str, start: u32, end: u32, kind: &str| crate::symbols::SymbolReference {
        file: std::path::PathBuf::from(file),
        start_line: start,
        end_line: end,
        kind: kind.to_string(),
    };
    let input = vec![
        r("src/A.cs", 9, 9, "definition"),
        r("src/A.cs", 9, 9, "definition"),
        r("src/A.cs", 9, 9, "definition"),
        r("src/B.cs", 192, 192, "reference"),
        r("src/A.cs", 9, 9, "definition"),
        r("src/A.cs", 40, 44, "reference"),
        r("src/A.cs", 9, 9, "reference"), // same line, DIFFERENT kind → kept
    ];
    let out = super::dedupe_references(input);
    let summary: Vec<(String, u32, u32, &str)> = out
        .iter()
        .map(|x| {
            (
                x.file.to_string_lossy().into_owned(),
                x.start_line,
                x.end_line,
                x.kind.as_str(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("src/A.cs".to_string(), 9, 9, "definition"),
            ("src/B.cs".to_string(), 192, 192, "reference"),
            ("src/A.cs".to_string(), 40, 44, "reference"),
            ("src/A.cs".to_string(), 9, 9, "reference"),
        ],
        "identical entries collapse to the first, distinct ones survive, order is stable: {summary:?}"
    );

    // Empty input stays empty (no panic on the with_capacity path).
    assert!(super::dedupe_references(Vec::new()).is_empty());
}

#[tokio::test]
#[serial_test::serial]
async fn ambiguous_name_answers_with_the_typed_candidates_envelope() {
    let helper_root = tempfile::tempdir().unwrap();
    let _guard = make_helper_available(&helper_root);
    let (service, project) = build_service();
    populate_overload_fixture(&project.path().join(".codesearch.db"));

    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_name: Some("Validate".to_string()),
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["ambiguous"],
        serde_json::Value::Bool(true),
        "an overloaded name must answer the ambiguity envelope, got: {out}"
    );
    let candidates = v["candidates"].as_array().expect("candidates array");
    assert_eq!(candidates.len(), 2, "both overloads listed: {v}");
    assert!(
        candidates[0].as_str().unwrap() <= candidates[1].as_str().unwrap(),
        "candidates must be sorted: {v}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn an_explicit_key_that_misses_is_a_loud_failure_not_an_empty_answer() {
    let helper_root = tempfile::tempdir().unwrap();
    let _guard = make_helper_available(&helper_root);
    let (service, project) = build_service();
    populate_overload_fixture(&project.path().join(".codesearch.db"));

    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_key: Some("csharp Ns . V#Gone().".to_string()),
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["class"], "failed",
        "an explicit-key miss must be a typed failure, got: {out}"
    );
    assert!(
        v["error"].as_str().unwrap().contains("not in the"),
        "the error must name the miss: {out}"
    );
    assert!(
        v.get("ambiguous").is_none(),
        "a key miss is not ambiguity: {out}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn fuzzy_not_found_keeps_the_empty_references_contract() {
    let helper_root = tempfile::tempdir().unwrap();
    let _guard = make_helper_available(&helper_root);
    let (service, project) = build_service();
    populate_overload_fixture(&project.path().join(".codesearch.db"));

    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_name: Some("Nope".to_string()),
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["references"],
        serde_json::json!([]),
        "the historical empty-references contract: {out}"
    );
    assert_eq!(v["symbol"], "Nope", "the echo stays the query");
    assert!(
        v.get("resolved_symbol").is_none(),
        "an unresolved answer names no identity: {out}"
    );
}

#[tokio::test]
async fn symbol_key_combined_with_a_fuzzy_query_is_rejected() {
    // Validation precedes every lookup, so no helper env is needed.
    let (service, _project) = build_service();
    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_key: Some("csharp Ns . V#Validate().".to_string()),
            symbol_name: Some("Validate".to_string()),
            ..find_impact_request()
        },
    )
    .await;
    assert!(
        out.contains("mutually exclusive"),
        "the combination must be rejected with the documented usage error: {out}"
    );
}

/// The TS indexer passes `is_available` via the env override the same way
/// the C# one does — only `is_file()` is checked, so a dummy suffices.
fn make_ts_helper_available(root: &tempfile::TempDir) -> crate::testing::EnvRestore {
    let helper = root.path().join(if cfg!(windows) {
        "scip-typescript.exe"
    } else {
        "scip-typescript"
    });
    std::fs::write(&helper, b"dummy").expect("dummy helper file");
    crate::testing::EnvRestore::set(&[(
        crate::constants::SCIP_TYPESCRIPT_HELPER_ENV,
        helper.to_string_lossy().as_ref(),
    )])
}

const CS_ONLY_KEY: &str = "csharp Ns . O#CsOnly().";
const TS_ONLY_KEY: &str = "scip-typescript npm mypkg 1.0.0 `src/o.ts`/TsOnly().";
const CS_BOTH_KEY: &str = "csharp Ns . B#Both().";
const CS_BOTH_OVERLOAD_KEY: &str = "csharp Ns . B#Both(System.String).";
const TS_BOTH_KEY: &str = "scip-typescript npm mypkg 1.0.0 `src/b.ts`/Both().";

/// One index holding both languages' keys in the shared tables, the way a
/// polyglot repo does: a name unique to each language, and one name (`Both`)
/// defined in each (C# with two overloads).
fn populate_polyglot_fixture(db_path: &std::path::Path) {
    let env = crate::symbols::get_shared_scip_env(db_path).expect("shared env");
    let symbols = crate::lmdb_registry::open_named_db::<heed::types::Str, heed::types::Bytes>(
        &env,
        crate::constants::SCIP_SYMBOLS_DB_NAME,
    )
    .unwrap()
    .unwrap();
    let names = crate::lmdb_registry::open_named_db::<heed::types::Str, heed::types::Bytes>(
        &env,
        crate::constants::SCIP_SIMPLE_NAMES_DB_NAME,
    )
    .unwrap()
    .unwrap();
    let mut wtxn = env.write_txn().expect("wtxn");
    for key in [
        CS_ONLY_KEY,
        TS_ONLY_KEY,
        CS_BOTH_KEY,
        CS_BOTH_OVERLOAD_KEY,
        TS_BOTH_KEY,
    ] {
        symbols
            .put(&mut wtxn, key, &stored_refs_bytes("src/p.cs", "definition"))
            .unwrap();
    }
    let by_name: [(&str, &[&str]); 3] = [
        ("CsOnly", &[CS_ONLY_KEY]),
        ("TsOnly", &[TS_ONLY_KEY]),
        ("Both", &[CS_BOTH_KEY, CS_BOTH_OVERLOAD_KEY, TS_BOTH_KEY]),
    ];
    for (name, keys) in by_name {
        // The simple-name payload shares the key-list wire format.
        names.put(&mut wtxn, name, &warnings_bytes(keys)).unwrap();
    }
    wtxn.commit().unwrap();
}

#[test]
fn a_language_pick_keeps_only_that_languages_keys_of_a_mixed_csharp_and_typescript_name() {
    use crate::symbols::{KeyMatch, SymbolIndexerRegistry};

    let cs_overload = CS_BOTH_OVERLOAD_KEY;
    let ambiguous =
        |keys: &[&str]| KeyMatch::Ambiguous(keys.iter().map(|k| k.to_string()).collect());
    let resolved = |key: &str| KeyMatch::Resolved(key.to_string());
    // (language picked, what the shared tables returned, what that language may answer)
    let cases: Vec<(&str, KeyMatch, KeyMatch)> = vec![
        // C# overloads plus a TypeScript key under the same name: only the C# keys stay candidates.
        (
            "csharp",
            ambiguous(&[cs_overload, CS_BOTH_KEY, TS_BOTH_KEY]),
            ambiguous(&[cs_overload, CS_BOTH_KEY]),
        ),
        // One key per language: the other language's key drops, leaving a single resolved symbol.
        (
            "csharp",
            ambiguous(&[CS_BOTH_KEY, TS_BOTH_KEY]),
            resolved(CS_BOTH_KEY),
        ),
        (
            "typescript",
            ambiguous(&[CS_BOTH_KEY, TS_BOTH_KEY]),
            resolved(TS_BOTH_KEY),
        ),
        // Only the other language's keys: nothing is left to answer from.
        (
            "csharp",
            ambiguous(&[TS_ONLY_KEY, TS_BOTH_KEY]),
            KeyMatch::NotFound,
        ),
        ("csharp", resolved(TS_ONLY_KEY), KeyMatch::NotFound),
        // Owned keys and a miss pass through untouched.
        (
            "csharp",
            ambiguous(&[cs_overload, CS_BOTH_KEY]),
            ambiguous(&[cs_overload, CS_BOTH_KEY]),
        ),
        ("csharp", resolved(CS_ONLY_KEY), resolved(CS_ONLY_KEY)),
        ("csharp", KeyMatch::NotFound, KeyMatch::NotFound),
    ];
    let registry = SymbolIndexerRegistry::new();
    for (language, found, expected) in cases {
        let indexer = registry.get(language).expect("registered indexer");
        assert_eq!(
            retain_owned_keys(indexer, found.clone()),
            expected,
            "{language} must answer only from its own keys, given {found:?}"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn without_language_a_symbol_key_routes_to_its_owning_indexer() {
    // Both helpers installed: a key names its own language by scheme, so no
    // language and no ambiguity answer are needed. The last row is an
    // explicit language that agrees with the key (any casing).
    let csharp_root = tempfile::tempdir().unwrap();
    let ts_root = tempfile::tempdir().unwrap();
    let _csharp = make_helper_available(&csharp_root);
    let _ts = make_ts_helper_available(&ts_root);
    let (service, project) = build_service();
    populate_polyglot_fixture(&project.path().join(".codesearch.db"));

    let cases: [(Option<&str>, &str, &str); 3] = [
        (None, CS_ONLY_KEY, "csharp"),
        (None, TS_ONLY_KEY, "typescript"),
        (Some("TypeScript"), TS_ONLY_KEY, "typescript"),
    ];
    for (language, key, expected) in cases {
        let out = tool_text(
            &service,
            FindImpactRequest {
                symbol_key: Some(key.to_string()),
                language: language.map(str::to_string),
                ..find_impact_request()
            },
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["resolved_symbol"], key,
            "a {expected} key must resolve without a language, got: {out}"
        );
        assert_eq!(
            v["language"], expected,
            "the key's own index must answer: {out}"
        );
        assert!(
            v.get("ambiguous_language").is_none() && v.get("retry_with").is_none(),
            "a resolved language never carries the ambiguity answer: {out}"
        );
    }
}

#[tokio::test]
async fn a_language_that_contradicts_the_symbol_key_is_refused() {
    // Refusal precedes every lookup, so no helper env is needed.
    let (service, _project) = build_service();
    let cases = [
        ("typescript", CS_ONLY_KEY, "csharp"),
        ("csharp", TS_ONLY_KEY, "typescript"),
    ];
    for (language, key, key_language) in cases {
        let out = tool_text(
            &service,
            FindImpactRequest {
                symbol_key: Some(key.to_string()),
                language: Some(language.to_string()),
                ..find_impact_request()
            },
        )
        .await;
        assert!(
            out.contains(&format!("`language` is '{language}'"))
                && out.contains(&format!("is a {key_language} key")),
            "the refusal must name both the language and the key's language, got: {out}"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn without_language_one_applicable_language_answers_without_asking() {
    // A solution file makes csharp the only applicable language. `Both` is
    // defined in each language, so only the applicability filter can keep
    // this from becoming a language question.
    let csharp_root = tempfile::tempdir().unwrap();
    let ts_root = tempfile::tempdir().unwrap();
    let _csharp = make_helper_available(&csharp_root);
    let _ts = make_ts_helper_available(&ts_root);
    let (service, project) = build_applicable_service();
    populate_polyglot_fixture(&project.path().join(".codesearch.db"));

    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_name: Some("Both".to_string()),
            language: None,
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        v.get("ambiguous_language").is_none(),
        "one applicable language must answer, not ask: {out}"
    );
    assert_eq!(
        v["ambiguous"],
        serde_json::Value::Bool(true),
        "csharp answered (here: with its own symbol-level candidates): {out}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn a_picked_language_lists_only_its_own_keys_when_the_name_exists_in_csharp_and_typescript() {
    // `Both` holds two C# overloads and one TypeScript key in the shared
    // tables; the explicit csharp pick must not offer the TypeScript key.
    let csharp_root = tempfile::tempdir().unwrap();
    let ts_root = tempfile::tempdir().unwrap();
    let _csharp = make_helper_available(&csharp_root);
    let _ts = make_ts_helper_available(&ts_root);
    let (service, project) = build_service();
    populate_polyglot_fixture(&project.path().join(".codesearch.db"));

    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_name: Some("Both".to_string()),
            language: Some("csharp".to_string()),
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["candidates"],
        serde_json::json!([CS_BOTH_KEY, CS_BOTH_OVERLOAD_KEY]),
        "only csharp keys may be candidates after a csharp pick: {out}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn without_language_a_name_found_in_one_language_answers_from_it() {
    // Nothing applies (no solution, no tsconfig), so both installed
    // languages are candidates and the index decides.
    let csharp_root = tempfile::tempdir().unwrap();
    let ts_root = tempfile::tempdir().unwrap();
    let _csharp = make_helper_available(&csharp_root);
    let _ts = make_ts_helper_available(&ts_root);
    let (service, project) = build_service();
    populate_polyglot_fixture(&project.path().join(".codesearch.db"));

    let cases = [
        ("CsOnly", "csharp", CS_ONLY_KEY),
        ("TsOnly", "typescript", TS_ONLY_KEY),
    ];
    for (name, expected, key) in cases {
        let out = tool_text(
            &service,
            FindImpactRequest {
                symbol_name: Some(name.to_string()),
                language: None,
                ..find_impact_request()
            },
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["resolved_symbol"], key,
            "'{name}' lives only in the {expected} index and must resolve there: {out}"
        );
        assert_eq!(v["language"], expected, "wrong index answered: {out}");
        assert!(
            v.get("retry_with").is_none(),
            "a resolved language carries no retry entries: {out}"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn without_language_a_name_not_decided_by_the_indexes_returns_retry_entries() {
    // `Both` is defined in each language and `Nope` in neither: the server
    // cannot decide, so the answer hands over one verbatim retry per language
    // carrying the caller's original arguments.
    let csharp_root = tempfile::tempdir().unwrap();
    let ts_root = tempfile::tempdir().unwrap();
    let _csharp = make_helper_available(&csharp_root);
    let _ts = make_ts_helper_available(&ts_root);
    let (service, project) = build_service();
    populate_polyglot_fixture(&project.path().join(".codesearch.db"));

    for name in ["Both", "Nope"] {
        let out = tool_text(
            &service,
            FindImpactRequest {
                symbol_name: Some(name.to_string()),
                file: Some("notes.txt".to_string()),
                line: Some(7),
                language: None,
                ..find_impact_request()
            },
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["ambiguous_language"],
            serde_json::Value::Bool(true),
            "'{name}' must ask for a language: {out}"
        );
        assert!(
            v.get("ambiguous").is_none(),
            "a language question is not a symbol ambiguity: {out}"
        );
        assert_eq!(v["languages"], serde_json::json!(["csharp", "typescript"]));
        assert_eq!(
            v["retry_with"],
            serde_json::json!([
                {"symbol_name": name, "file": "notes.txt", "line": 7, "language": "csharp"},
                {"symbol_name": name, "file": "notes.txt", "line": 7, "language": "typescript"},
            ]),
            "each entry is the original arguments plus one language: {out}"
        );
        let hint = v["hint_for_agent"].as_str().unwrap();
        assert!(
            hint.contains("csharp, typescript")
                && hint.contains("copy one `retry_with` entry verbatim"),
            "the message must name the candidates and say how to retry: {hint}"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn without_language_a_single_installed_helper_answers_deterministically() {
    // Exactly one installed: the pick is deterministic, so the query
    // proceeds (here: to the ambiguity envelope from the fixture) instead
    // of asking which language to use.
    //
    // The premise only holds where `scip-typescript` is NOT resolvable:
    // the adapter falls back to `npx` on PATH, so on a Node machine TS is
    // always installed and the single-helper scenario does not exist.
    let lookup = if cfg!(windows) { "where" } else { "which" };
    let npx_on_path = std::process::Command::new(lookup)
        .arg("npx")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if npx_on_path {
        eprintln!("skipped: npx on PATH makes TS installed, so several helpers are installed");
        return;
    }
    let helper_root = tempfile::tempdir().unwrap();
    let _guard = make_helper_available(&helper_root);
    let (service, project) = build_service();
    populate_overload_fixture(&project.path().join(".codesearch.db"));

    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_name: Some("Validate".to_string()),
            language: None,
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["ambiguous"],
        serde_json::Value::Bool(true),
        "one installed helper must answer, not ask: {out}"
    );
}

// ── Warnings contract: a partial answer must say so ─────────────────
//
// B3 honesty: resolution warnings persisted in LMDB surface on the resolved
// answer; a clean answer must OMIT the field entirely (the additive-JSON
// contract pins `skip_serializing_if`).

/// Version byte + bincode of `Vec<(PathBuf, u32, u32, String)>` — the same
/// bytes `serialize_refs` writes for a `StoredReference` (bincode encodes a
/// struct as its fields in declaration order, identical to the tuple).
fn stored_refs_bytes(file: &str, kind: &str) -> Vec<u8> {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(
        &bincode::serde::encode_to_vec(
            vec![(std::path::PathBuf::from(file), 7u32, 9u32, kind.to_string())],
            bincode::config::legacy(),
        )
        .unwrap(),
    );
    bytes
}

/// Version byte + bincode of `Vec<String>` — the same bytes the lazy and
/// batch write paths store in `scip_ref_warnings` (the key-list format).
fn warnings_bytes(warnings: &[&str]) -> Vec<u8> {
    let owned: Vec<String> = warnings.iter().map(|w| w.to_string()).collect();
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(
        &bincode::serde::encode_to_vec(&owned, bincode::config::legacy()).unwrap(),
    );
    bytes
}

/// Persist index-level rebuild warnings the way a C# rebuild does (meta
/// key, JSON array) — the fixture for answers served from an index built
/// out of a partially loaded solution.
fn write_index_warnings_meta(db_path: &std::path::Path, warnings: &[String]) {
    let env = crate::symbols::get_shared_scip_env(db_path).expect("shared env");
    let meta = crate::lmdb_registry::create_named_db::<heed::types::Str, heed::types::Str>(
        &env,
        crate::constants::SCIP_META_DB_NAME,
    )
    .unwrap();
    let mut wtxn = env.write_txn().expect("wtxn");
    meta.put(
        &mut wtxn,
        crate::constants::SCIP_INDEX_WARNINGS_KEY,
        serde_json::to_string(warnings).unwrap().as_str(),
    )
    .unwrap();
    wtxn.commit().unwrap();
}

/// One resolved symbol with persisted warnings, one without.
fn populate_warnings_fixture(db_path: &std::path::Path) -> (String, String) {
    let warned_key = "csharp Ns . W#Warned().".to_string();
    let clean_key = "csharp Ns . C#Clean().".to_string();
    let env = crate::symbols::get_shared_scip_env(db_path).expect("shared env");
    let symbols = crate::lmdb_registry::open_named_db::<heed::types::Str, heed::types::Bytes>(
        &env,
        crate::constants::SCIP_SYMBOLS_DB_NAME,
    )
    .unwrap()
    .unwrap();
    let warnings_db =
        crate::lmdb_registry::create_named_db::<heed::types::Str, heed::types::Bytes>(
            &env,
            crate::constants::SCIP_REF_WARNINGS_DB_NAME,
        )
        .unwrap();
    let mut wtxn = env.write_txn().expect("wtxn");
    for key in [&warned_key, &clean_key] {
        symbols
            .put(
                &mut wtxn,
                key.as_str(),
                &stored_refs_bytes("src/w.cs", "definition"),
            )
            .unwrap();
    }
    warnings_db
        .put(
            &mut wtxn,
            warned_key.as_str(),
            &warnings_bytes(&[
                "FindReferencesAsync failed for Warned: InvalidOperationException: boom",
            ]),
        )
        .unwrap();
    wtxn.commit().unwrap();
    (warned_key, clean_key)
}

#[tokio::test]
#[serial_test::serial]
async fn resolved_answer_surfaces_persisted_warnings() {
    let helper_root = tempfile::tempdir().unwrap();
    let _guard = make_helper_available(&helper_root);
    let (service, project) = build_service();
    let (warned_key, _clean_key) =
        populate_warnings_fixture(&project.path().join(".codesearch.db"));

    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_key: Some(warned_key.clone()),
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["resolved_symbol"], warned_key,
        "the fixture query must actually resolve, got: {out}"
    );
    let warnings = v["warnings"].as_array().expect("warnings array present");
    assert_eq!(
        warnings.len(),
        1,
        "the persisted warning must surface: {out}"
    );
    assert!(
        warnings[0]
            .as_str()
            .unwrap()
            .contains("FindReferencesAsync failed for Warned"),
        "warning text must round-trip verbatim: {out}"
    );
}

/// Every EMPTY answer carries the per-language symbol-index status of the
/// repo it came from — the check an agent needs before reading
/// `references: []` as "verified no callers". A name that matches nothing
/// is the canonical empty answer: LMDB-only, no helper spawn. Applicable
/// languages only: a repo without a tsconfig must not list typescript as
/// absent-with-blame.
#[tokio::test]
#[serial_test::serial]
async fn answers_carry_the_symbol_index_status() {
    let helper_root = tempfile::tempdir().unwrap();
    let _guard = make_helper_available(&helper_root);
    let (service, root) = build_applicable_service();
    populate_warnings_fixture(&root.path().join(".codesearch.db"));

    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_name: Some("NoSuchSymbol".to_string()),
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["references"],
        serde_json::json!([]),
        "the name must not match the fixture's symbols: {out}"
    );

    let statuses = v["symbol_index"]
        .as_array()
        .expect("symbol_index present on an empty answer");
    let csharp = statuses
        .iter()
        .find(|s| s["language"] == "csharp")
        .expect("csharp status entry present");
    assert_eq!(
        csharp["state"], "absent",
        "the fixture has symbol tables but no key-format stamp, so has_index refuses it: {out}"
    );
    assert!(
        statuses.iter().all(|s| s["language"] != "typescript"),
        "non-applicable languages must be filtered out of find_impact answers: {out}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn clean_resolved_answer_omits_the_warnings_field() {
    let helper_root = tempfile::tempdir().unwrap();
    let _guard = make_helper_available(&helper_root);
    let (service, project) = build_service();
    let (_warned_key, clean_key) =
        populate_warnings_fixture(&project.path().join(".codesearch.db"));

    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_key: Some(clean_key.clone()),
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["resolved_symbol"], clean_key,
        "the fixture query must actually resolve, got: {out}"
    );
    assert!(
        v.get("warnings").is_none(),
        "a clean answer must OMIT warnings (skip_serializing_if), got: {out}"
    );
}

/// The missing-index warning is what keeps a never-built symbol index's
/// empty reference list from passing for "no callers": whatever the heal
/// status, it must name the language and state UNKNOWN explicitly. The
/// wording per status must also stay honest — "a background rebuild was
/// started" is only allowed when one actually was.
#[test]
fn missing_index_warning_names_language_and_marks_answer_unknown() {
    let warning = missing_index_warning("csharp", SymbolIndexHeal::BackgroundStarted);
    assert!(
        warning.contains("csharp"),
        "must name the language, got: {warning}"
    );
    assert!(
        warning.contains("UNKNOWN"),
        "must mark the empty list as UNKNOWN, got: {warning}"
    );
    assert!(
        warning.contains("retry"),
        "must tell the caller to retry after the rebuild, got: {warning}"
    );
}

#[test]
fn missing_index_warning_manual_names_the_build_command() {
    let warning = missing_index_warning("typescript", SymbolIndexHeal::Manual);
    assert!(warning.contains("UNKNOWN"), "got: {warning}");
    assert!(
        warning.contains("codesearch index symbol"),
        "must tell the caller how to build the index, got: {warning}"
    );
    assert!(
        !warning.contains("rebuild was started"),
        "nothing was started — must not claim it, got: {warning}"
    );
}

#[test]
fn missing_index_warning_not_applicable_explains_why_and_promises_nothing() {
    let reason = "the scip-typescript adapter requires a top-level tsconfig.json (monorepo layouts are not resolved yet)";
    let warning = missing_index_warning("typescript", SymbolIndexHeal::NotApplicable(reason));
    assert!(warning.contains("UNKNOWN"), "got: {warning}");
    assert!(
        warning.contains("tsconfig.json"),
        "must surface the indexer's own reason, got: {warning}"
    );
    assert!(
        !warning.contains("rebuild was started"),
        "a non-applicable repo can never be rebuilt here — must not claim a rebuild, got: {warning}"
    );
}

/// A never-built index must not answer "no references" silently: the empty
/// NotFound answer carries the self-heal warning. Fails without the fix —
/// the pre-fix answer is byte-identical to a genuine "no callers", the exact
/// trap that reads as "safe to remove" (observed live on a runtime-registered
/// worktree whose SCIP index was never built).
#[tokio::test]
#[serial_test::serial]
async fn never_built_index_warns_on_the_empty_answer() {
    let helper_root = tempfile::tempdir().unwrap();
    let _guard = make_helper_available(&helper_root);
    // No fixture population: a valid-but-empty store — neither symbols nor a
    // rebuild timestamp. This is the never-built state.
    let (service, _root) = build_service();

    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_name: Some("ResolveLinkedActivityAsync".to_string()),
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        v["references"].as_array().is_some_and(|a| a.is_empty()),
        "the never-built index answers with empty references, got: {out}"
    );
    let warnings = v["warnings"].as_array().expect("self-heal warning present");
    assert_eq!(
        warnings.len(),
        1,
        "exactly the missing-index warning, got: {out}"
    );
    assert!(
        warnings[0].as_str().is_some_and(|w| w.contains("UNKNOWN")),
        "the warning must mark the answer UNKNOWN, got: {out}"
    );
}

/// A POSITION query that misses the index must answer a loud typed failure,
/// never the empty-references shape: an agent copying (file, line) from a
/// `find` result on a stale index read the silent empty answer as "zero
/// callers" (observed live: a method with 8 call sites "had" 0 because the
/// position query missed by a line convention drift). Name queries keep the
/// historical empty answer.
#[tokio::test]
#[serial_test::serial]
async fn position_miss_answers_loud_failure_not_empty_references() {
    let helper_root = tempfile::tempdir().unwrap();
    let _guard = make_helper_available(&helper_root);
    let (service, project) = build_service();
    let db = project.path().join(".codesearch.db");
    let (_warned_key, clean_key) = populate_warnings_fixture(&db);
    // Index-level warnings must ride on the FAILURE shape too — a position
    // miss from a partially loaded index must not hide that the whole
    // index's answers may be incomplete.
    write_index_warnings_meta(
        &db,
        &["The symbol index was built while the C# workspace reported 2 distinct failure(s) — cross-project references may be missing from every answer."
            .to_string()],
    );

    // A real position index with one entry: a miss below is genuinely
    // "nothing at that position", not a missing DB.
    {
        let env = crate::symbols::get_shared_scip_env(&db).expect("shared env");
        let positions =
            crate::lmdb_registry::create_named_db::<heed::types::Str, heed::types::Bytes>(
                &env,
                crate::constants::SCIP_POSITION_DB_NAME,
            )
            .unwrap();
        let mut wtxn = env.write_txn().expect("wtxn");
        let mut entry = vec![1u8];
        entry.extend_from_slice(
            &bincode::serde::encode_to_vec(vec![clean_key], bincode::config::legacy()).unwrap(),
        );
        positions.put(&mut wtxn, "src/w.cs:5", &entry).unwrap();
        wtxn.commit().unwrap();
    }

    let out = tool_text(
        &service,
        FindImpactRequest {
            file: Some("src/w.cs".to_string()),
            line: Some(999),
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        v.get("references").is_none(),
        "a position miss must not answer the empty-references shape, got: {out}"
    );
    assert_eq!(v["class"], "failed", "typed failure, got: {out}");
    assert!(
        v["hint_for_agent"]
            .as_str()
            .is_some_and(|h| h.contains("1-based")),
        "the hint must explain the line convention, got: {out}"
    );
    let failure_warnings = v["warnings"]
        .as_array()
        .expect("index-level warnings ride the failure");
    assert!(
        failure_warnings.iter().any(|w| w
            .as_str()
            .is_some_and(|w| w.contains("cross-project references"))),
        "the partial-index warning must not be hidden by the failure, got: {out}"
    );
}

/// Index-level rebuild warnings must ride on EVERY answer from an index
/// built out of a partially loaded solution — resolved answers AND empty
/// NotFound answers. This pins the live defect reported on a customer
/// worktree: MSBuild project-load failures (broken NuGet restore) produced
/// an index with definitions but no cross-project references, and every
/// answer passed as clean while an orphan-check read the empty lists as
/// "no callers".
#[tokio::test]
#[serial_test::serial]
async fn index_level_rebuild_warnings_ride_on_resolved_and_empty_answers() {
    let helper_root = tempfile::tempdir().unwrap();
    let _guard = make_helper_available(&helper_root);
    let (service, project) = build_service();
    let db = project.path().join(".codesearch.db");
    let (_warned_key, clean_key) = populate_warnings_fixture(&db);
    let index_warnings = vec![
        "The symbol index was built while the C# workspace reported 2 distinct failure(s) — cross-project references may be missing from every answer. Failures `dotnet build` also reports are real and fixable in the repo (often a missing dotnet restore); failures only scip-csharp reports are tool-side — please report them and reindex."
            .to_string(),
        "App.Dam.csproj: depends on Analyzer.X (>= 1.2.3) but it was not found".to_string(),
    ];
    write_index_warnings_meta(&db, &index_warnings);

    // A RESOLVED answer from the partial index carries the index warnings.
    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_key: Some(clean_key.clone()),
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["resolved_symbol"], clean_key,
        "the fixture query must resolve, got: {out}"
    );
    let warnings = v["warnings"].as_array().expect("warnings present");
    assert_eq!(
        serde_json::to_value(warnings).unwrap(),
        serde_json::to_value(&index_warnings).unwrap(),
        "a resolved answer from a partially built index carries exactly the index warnings: {out}"
    );

    // The EMPTY NotFound answer carries them too — an empty reference list
    // out of a partial index must never pass for "no callers".
    let out = tool_text(
        &service,
        FindImpactRequest {
            symbol_name: Some("NotARealSymbolAnywhere".to_string()),
            ..find_impact_request()
        },
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        v["references"].as_array().is_some_and(|a| a.is_empty()),
        "unknown symbol answers empty, got: {out}"
    );
    assert!(
        v["warnings"].as_array().is_some_and(
            |w| !w.is_empty() && w[0].as_str().is_some_and(|s| s.contains("cross-project"))
        ),
        "the empty answer must carry the index-level warning, got: {out}"
    );
}
