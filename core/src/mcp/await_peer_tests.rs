use super::McpProxyService;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A `McpProxyService` whose peer slot never fills on its own — no reconnect
/// plumbing behind it, matching the "single-shot" spirit of the existing
/// `McpProxyService::new` test constructor but with an *empty* peer slot,
/// which is the case `await_peer_bounded` actually has to wait through.
fn empty_peer_service() -> McpProxyService {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let (connect_tx, _connect_rx) = tokio::sync::mpsc::channel(1);
    McpProxyService {
        peer: Arc::new(tokio::sync::RwLock::new(None)),
        disconnect_tx: tx,
        connect_request_tx: connect_tx,
        last_activity: Arc::new(Mutex::new(Instant::now())),
        in_flight: Arc::new(AtomicUsize::new(0)),
        connect_failed: Arc::new(tokio::sync::Notify::new()),
    }
}

#[tokio::test]
async fn times_out_when_the_peer_slot_never_fills_and_nothing_is_notified() {
    let svc = empty_peer_service();
    let start = Instant::now();
    let ok = svc.await_peer_bounded(150).await;
    assert!(!ok);
    // Baseline: with no signal at all this genuinely waits out the budget,
    // rather than returning early for some unrelated reason — which is what
    // makes the next test's early return meaningful.
    assert!(
        start.elapsed() >= Duration::from_millis(150),
        "expected the full wait budget to elapse, took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn a_connect_failure_notification_clamps_the_wait_to_the_refusal_grace() {
    // Uses `await_peer_bounded_with_grace` directly (not the production
    // `await_peer_bounded`, which hardcodes `CONNECT_REFUSAL_GRACE` at
    // ~4s) so the clamp itself is exercised with a millisecond-scale
    // grace instead of actually waiting out `reconnect::INTERVAL_SECS`.
    let svc = empty_peer_service();
    let connect_failed = svc.connect_failed.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        connect_failed.notify_waiters();
    });

    let start = Instant::now();
    // Budget is 5s, grace is 100ms — if the clamp did not fire, this call
    // would take the full 5s instead of ~20ms (notification) + ~100ms
    // (grace) for the peer slot to (not) fill in.
    let ok = svc
        .await_peer_bounded_with_grace(5_000, Duration::from_millis(100))
        .await;
    let elapsed = start.elapsed();

    assert!(
        !ok,
        "peer slot stayed empty — this was a refusal, not a success"
    );
    assert!(
        elapsed < Duration::from_millis(1_000),
        "expected the failure notification to clamp the 5s wait down near the \
             grace window, took {:?}",
        elapsed
    );
    assert!(
        elapsed >= Duration::from_millis(100),
        "expected the clamp to still honor the refusal-grace window rather than \
             returning immediately, took {:?}",
        elapsed
    );
}

#[tokio::test]
async fn note_connect_failure_wakes_a_parked_waiter_and_schedules_a_disconnect() {
    // Pins the exact production call site (`run_mcp_client`'s
    // `connect_request_rx` Err arm) rather than re-testing
    // `await_peer_bounded`'s reaction to a hand-fired notification: this
    // is the one line that makes that short-circuit real, and nothing
    // previously covered it — deleting `note_connect_failure`'s body left
    // the whole suite green.
    let connect_failed = Arc::new(tokio::sync::Notify::new());
    let (disconnect_tx, mut disconnect_rx) = tokio::sync::mpsc::channel::<()>(1);

    let waiter_failed = connect_failed.clone();
    let waiter = tokio::spawn(async move {
        waiter_failed.notified().await;
    });
    // Give the spawned task a moment to actually park in `.notified()`
    // before firing, so this proves a live waiter is woken — not merely
    // that a notification lands somewhere.
    tokio::time::sleep(Duration::from_millis(20)).await;

    super::note_connect_failure(&connect_failed, &disconnect_tx);

    tokio::time::timeout(Duration::from_millis(500), waiter)
        .await
        .expect("note_connect_failure did not wake the parked waiter in time")
        .expect("waiter task panicked");

    let got = tokio::time::timeout(Duration::from_millis(500), disconnect_rx.recv())
        .await
        .expect("note_connect_failure did not schedule the synthetic disconnect in time");
    assert!(
        got.is_some(),
        "expected the disconnect channel to receive a message"
    );
}

#[tokio::test]
async fn a_stale_notification_before_anyone_is_waiting_does_not_leak_forward() {
    // Notify::notify_waiters() only wakes tasks already parked in
    // .notified() — it stores no permit for a future waiter (unlike
    // notify_one()). Pinned explicitly because `await_peer_bounded`'s
    // correctness depends on this: a failure from an unrelated, already-
    // finished wait must not falsely short-circuit the next one.
    let svc = empty_peer_service();
    svc.connect_failed.notify_waiters(); // no one is waiting yet

    let start = Instant::now();
    let ok = svc.await_peer_bounded(150).await;
    assert!(!ok);
    assert!(
        start.elapsed() >= Duration::from_millis(150),
        "a pre-existing notification must not shorten a later, unrelated wait, took {:?}",
        start.elapsed()
    );
}
