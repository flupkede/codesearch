//! Tests for the process-wide scip-csharp helper gate (`helper_gate.rs`).
//! Tests whose acquire may block are `#[serial]`: a blocked acquire consults
//! the global shutdown flag, which other serial tests toggle.

use super::helper_gate::{HelperGate, HelperPurpose};
use super::resident::{ClientLike, ResidentRefs, WorkspacePool};
use crate::constants::{
    csharp_scip_concurrency, parse_csharp_scip_concurrency, CSHARP_SCIP_CONCURRENCY_ENV,
};
use anyhow::Result;
use serial_test::serial;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

const BLOCKED_FOR: Duration = Duration::from_millis(300);
const COMPLETES_WITHIN: Duration = Duration::from_secs(10);

fn wt() -> &'static Path {
    Path::new("C:/code/wt")
}

struct OkClient;

impl ClientLike for OkClient {
    fn find_refs(&self, _symbol: &str) -> Result<ResidentRefs> {
        Ok(ResidentRefs::default())
    }

    fn kill(&self) {}
}

fn pool_with(
    gate: &Arc<HelperGate>,
    spawn: impl Fn() -> Result<Arc<dyn ClientLike>> + Send + Sync + 'static,
) -> Arc<WorkspacePool> {
    Arc::new(WorkspacePool::new(
        2,
        Duration::from_secs(600),
        1,
        Box::new(move |_: &Path, _: &Path, _: u64| spawn()),
        Arc::clone(gate),
    ))
}

fn lookup(pool: &WorkspacePool) -> Result<ResidentRefs> {
    pool.find_refs(
        &PathBuf::from("h.exe"),
        &PathBuf::from("C:/code/wt/App.sln"),
        wt(),
        "Sym",
    )
}

#[test]
fn concurrency_parse_defaults_and_clamps() {
    let cases: &[(Option<&str>, usize)] = &[
        (None, 1),
        (Some("0"), 1),
        (Some("1"), 1),
        (Some("2"), 2),
        (Some("4"), 4),
        (Some("9"), 4),
        (Some("garbage"), 1),
        (Some(""), 1),
        (Some("-1"), 1),
    ];
    for (raw, expected) in cases {
        assert_eq!(
            parse_csharp_scip_concurrency(*raw),
            *expected,
            "raw = {raw:?}"
        );
    }
}

#[test]
fn gate_capacity_is_at_least_one() {
    for (requested, expected) in [(0, 1), (1, 1), (3, 3)] {
        assert_eq!(HelperGate::new(requested).capacity(), expected);
    }
}

#[test]
#[serial]
fn concurrency_reads_the_env_var() {
    for (raw, expected) in [("3", 3), ("9", 4), ("garbage", 1)] {
        let _env = crate::testing::EnvRestore::set(&[(CSHARP_SCIP_CONCURRENCY_ENV, raw)]);
        assert_eq!(csharp_scip_concurrency(), expected, "raw = {raw:?}");
    }
    let _env = crate::testing::EnvRestore::remove(&[CSHARP_SCIP_CONCURRENCY_ENV]);
    assert_eq!(csharp_scip_concurrency(), 1);
}

#[test]
#[serial]
fn capacity_one_blocks_a_second_acquirer_until_the_first_permit_drops() {
    let gate = Arc::new(HelperGate::new(1));
    let first = gate.acquire(HelperPurpose::Rebuild, wt()).unwrap();

    let (tx, rx) = mpsc::channel();
    let gate_bg = Arc::clone(&gate);
    std::thread::spawn(move || {
        let acquired = gate_bg.acquire(HelperPurpose::FindRefs, wt()).map(drop);
        let _ = tx.send(acquired.map_err(|e| e.to_string()));
    });

    assert_eq!(
        rx.recv_timeout(BLOCKED_FOR),
        Err(RecvTimeoutError::Timeout),
        "second acquirer got a slot while the only one was held"
    );
    drop(first);
    assert_eq!(rx.recv_timeout(COMPLETES_WITHIN), Ok(Ok(())));
}

#[test]
#[serial]
fn resident_spawn_holds_the_gate_until_the_workspace_is_loaded() {
    let gate = Arc::new(HelperGate::new(1));
    let (started_tx, started_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let started_tx = std::sync::Mutex::new(started_tx);
    let go_rx = std::sync::Mutex::new(go_rx);
    let pool = pool_with(&gate, move || {
        let _ = started_tx.lock().unwrap().send(());
        let _ = go_rx.lock().unwrap().recv();
        Ok(Arc::new(OkClient))
    });

    let pool_bg = Arc::clone(&pool);
    let lookup_done = std::thread::spawn(move || lookup(&pool_bg).map(drop));
    started_rx.recv_timeout(COMPLETES_WITHIN).unwrap();

    let (tx, rx) = mpsc::channel();
    let gate_bg = Arc::clone(&gate);
    std::thread::spawn(move || {
        let acquired = gate_bg.acquire(HelperPurpose::Rebuild, wt()).map(drop);
        let _ = tx.send(acquired.map_err(|e| e.to_string()));
    });
    assert_eq!(
        rx.recv_timeout(BLOCKED_FOR),
        Err(RecvTimeoutError::Timeout),
        "a rebuild got a slot while a resident workspace was loading"
    );

    go_tx.send(()).unwrap();
    assert_eq!(rx.recv_timeout(COMPLETES_WITHIN), Ok(Ok(())));
    lookup_done.join().unwrap().unwrap();
}

#[test]
fn a_request_to_a_resident_helper_does_not_take_the_gate() {
    let gate = Arc::new(HelperGate::new(1));
    let pool = pool_with(&gate, || Ok(Arc::new(OkClient)));
    lookup(&pool).unwrap();

    let _held = gate.acquire(HelperPurpose::Rebuild, wt()).unwrap();
    let (tx, rx) = mpsc::channel();
    let pool_bg = Arc::clone(&pool);
    std::thread::spawn(move || {
        let _ = tx.send(lookup(&pool_bg).map(drop).map_err(|e| e.to_string()));
    });
    assert_eq!(rx.recv_timeout(COMPLETES_WITHIN), Ok(Ok(())));
}

#[test]
fn resident_spawn_failure_then_one_shot_fallback_completes_at_capacity_one() {
    let gate = Arc::new(HelperGate::new(1));
    let pool = pool_with(&gate, || anyhow::bail!("resident spawn failed"));

    let (tx, rx) = mpsc::channel();
    let gate_bg = Arc::clone(&gate);
    std::thread::spawn(move || {
        let resident = lookup(&pool);
        let fallback = gate_bg.acquire(HelperPurpose::FindRefs, wt()).map(drop);
        let _ = tx.send((
            resident.map(drop).map_err(|e| e.to_string()),
            fallback.map_err(|e| e.to_string()),
        ));
    });

    let (resident, fallback) = rx
        .recv_timeout(COMPLETES_WITHIN)
        .expect("fallback blocked: the failed resident spawn kept its slot");
    assert_eq!(resident, Err("resident spawn failed".to_string()));
    assert_eq!(fallback, Ok(()));
}
