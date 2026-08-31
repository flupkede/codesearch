//! Tests for the `find_impact` wall-clock budget.
//!
//! `find_impact_with_budget` is generic over the lookup future, so these
//! tests plant a *sleeping handler* (a future that sleeps past the budget,
//! the same busy-serve simulation the proxy tests use) instead of a real
//! SCIP helper — the budget race is exercised in milliseconds. The env
//! resolution is tested separately against `resolve_find_impact_budget_secs`
//! with `#[serial]` + `EnvRestore` per the repo rule for env mutation.

use super::{find_impact_with_budget, resolve_find_impact_budget_secs, ImpactLookupOutcome};
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
fn busy_envelope_serializes_the_four_documented_fields() {
    let busy = SymbolLookupBusy {
        busy: true,
        state: "resolving 'Ns.I.M' via the csharp SCIP helper".to_string(),
        waited_ms: 60_012,
        advice: "retry the same call in ~60s".to_string(),
    };
    let json: serde_json::Value = serde_json::to_value(&busy).unwrap();
    assert_eq!(json["busy"], serde_json::Value::Bool(true));
    assert!(json["state"].is_string());
    assert_eq!(json["waited_ms"], serde_json::Value::from(60_012));
    let advice = json["advice"].as_str().unwrap();
    assert!(
        advice.contains("retry the same call in ~"),
        "advice must carry the retry hint: {advice}"
    );
    // Exactly the documented envelope shape — the four fields, no extras.
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
    assert_eq!(keys, vec!["advice", "busy", "state", "waited_ms"]);
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
    let base = |index_head_sha: Option<String>, current_head_sha: Option<String>| {
        crate::symbols::FindImpactResult {
            symbol: "csharp Ns.T.M()".to_string(),
            references: vec![SymbolReference {
                file: PathBuf::from("a.cs"),
                start_line: 1,
                end_line: 1,
                kind: "definition".to_string(),
            }],
            index_age_seconds: 12,
            language: "csharp".to_string(),
            scope: "project:p".to_string(),
            index_head_sha,
            current_head_sha,
        }
    };
    let both: serde_json::Value =
        serde_json::to_value(base(Some("a".repeat(40)), Some("b".repeat(40)))).unwrap();
    assert_eq!(both["index_head_sha"], "a".repeat(40));
    assert_eq!(both["current_head_sha"], "b".repeat(40));

    // None must OMIT the key, not serialize null — old consumers see an
    // unchanged shape when the fingerprint is unknown.
    let neither: serde_json::Value = serde_json::to_value(base(None, None)).unwrap();
    let keys: Vec<&str> = neither
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(!keys.contains(&"index_head_sha"), "keys: {keys:?}");
    assert!(!keys.contains(&"current_head_sha"), "keys: {keys:?}");
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
