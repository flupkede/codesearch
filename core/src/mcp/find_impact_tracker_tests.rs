//! Tests for the `find_impact` background-continuation tracker.
//!
//! Behavioural, per-case tests against a locally constructed
//! `ImpactLookupTracker` (the process global is only a sharing wrapper —
//! the tracker logic carries no global state of its own). The one
//! cross-task test plants the exact production shape: a detached task that
//! finishes the entry after the registering caller stopped polling.

use super::{ImpactLookupTracker, TrackedStatus};
use crate::symbols::SymbolReference;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn key(name: &str) -> (PathBuf, String) {
    (PathBuf::from("C:/proj/.codesearch.db"), name.to_string())
}

fn sample_refs() -> Vec<SymbolReference> {
    vec![SymbolReference {
        file: PathBuf::from("src/a.cs"),
        start_line: 10,
        end_line: 12,
        kind: "call".to_string(),
    }]
}

#[test]
fn running_entry_reports_progress_then_warm_result_then_is_consumed() {
    let tracker = ImpactLookupTracker::new(Duration::from_secs(60));
    let k = key("Ns.I.M");

    let entry = tracker.register(k.clone());
    match tracker.check(&k) {
        Some(TrackedStatus::Running { elapsed_ms }) => {
            assert!(elapsed_ms <= 1_000, "just registered: {elapsed_ms}ms");
        }
        other => panic!("a running entry must report Running, got {other:?}"),
    }

    entry.finish(Ok(sample_refs()));

    match tracker.check(&k) {
        Some(TrackedStatus::Done(Ok(refs))) => {
            assert_eq!(refs.len(), 1);
            assert_eq!(refs[0].file, PathBuf::from("src/a.cs"));
            assert_eq!(refs[0].kind, "call");
        }
        other => panic!("a finished entry must serve the warm result, got {other:?}"),
    }
    // Consumed on read: the NEXT lookup must start fresh (real cache path).
    assert!(
        tracker.check(&k).is_none(),
        "a Done entry must be consumed by its first reader"
    );
}

#[test]
fn recorded_failure_is_served_once_with_rendered_chain() {
    let tracker = ImpactLookupTracker::new(Duration::from_secs(60));
    let k = key("Gone.I.M");

    let entry = tracker.register(k.clone());
    // The error side is recorded as its rendered `{:#}` chain (the render
    // happens at the record site — `anyhow::Error` is not `Clone`).
    let rendered = format!(
        "{:#}",
        anyhow::anyhow!("helper exited 1").context("scip-csharp find-refs failed")
    );
    entry.finish(Err(rendered));

    match tracker.check(&k) {
        Some(TrackedStatus::Done(Err(chain))) => {
            assert!(
                chain.contains("scip-csharp find-refs failed") && chain.contains("helper exited 1"),
                "the rendered chain must survive the tracker: {chain}"
            );
        }
        other => panic!("a recorded failure must surface as Done(Err), got {other:?}"),
    }
    assert!(tracker.check(&k).is_none(), "failure is consumed too");
}

#[test]
fn ttl_expiry_drops_entry_so_the_next_lookup_starts_fresh() {
    // Windows timer granularity is ~15ms; 50ms TTL vs 250ms sleep keeps
    // this deterministic on the slowest runner.
    let tracker = ImpactLookupTracker::new(Duration::from_millis(50));
    let k = key("Stale.I.M");

    tracker.register(k.clone());
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        tracker.check(&k).is_none(),
        "an entry past its TTL must be swept, not reported Running forever"
    );
}

#[tokio::test]
async fn detached_task_finish_is_observed_by_a_later_retry() {
    // The production shape: the handler's awaiting future is dropped at
    // budget overrun, but the detached blocking task keeps running and
    // records the outcome via the shared entry. A retry issued afterwards
    // must observe it.
    let tracker = Arc::new(ImpactLookupTracker::new(Duration::from_secs(60)));
    let k = key("Detached.I.M");
    let entry = tracker.register(k.clone());
    let entry_for_task = Arc::clone(&entry);

    let finisher = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        entry_for_task.finish(Ok(sample_refs()));
    });
    drop(entry); // the registering side lets go; the task survives

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match tracker.check(&k) {
            Some(TrackedStatus::Done(Ok(refs))) => {
                assert_eq!(refs.len(), 1);
                break;
            }
            Some(TrackedStatus::Done(Err(chain))) => {
                panic!("detached finish recorded a failure it must not have: {chain}");
            }
            Some(TrackedStatus::Running { .. }) => {}
            None => panic!("entry vanished before the detached finish landed"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "detached finish never observed within 3s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    finisher.await.expect("finisher task must not panic");
}

#[test]
fn racing_first_callers_share_one_entry() {
    let tracker = ImpactLookupTracker::new(Duration::from_secs(60));
    let k = key("Dup.I.M");

    let a = tracker.register(k.clone());
    let b = tracker.register(k.clone());
    assert!(
        Arc::ptr_eq(&a, &b),
        "register must get-or-create: racing callers dedupe onto one entry"
    );
    // One finish serves both callers; the first reader consumes it.
    a.finish(Ok(sample_refs()));
    assert!(matches!(
        tracker.check(&k),
        Some(TrackedStatus::Done(Ok(_)))
    ));
    assert!(tracker.check(&k).is_none());
}

#[test]
fn remove_after_in_budget_completion_forgets_the_entry() {
    let tracker = ImpactLookupTracker::new(Duration::from_secs(60));
    let k = key("Fast.I.M");

    let entry = tracker.register(k.clone());
    entry.finish(Ok(sample_refs()));
    tracker.remove(&k);
    assert!(
        tracker.check(&k).is_none(),
        "a within-budget completion must leave nothing tracked"
    );
}
