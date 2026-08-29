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
