//! Tests for the process-wide scip-csharp helper gate (`helper_gate.rs`).
//! Tests whose acquire may block are `#[serial]`: a blocked acquire consults
//! the global shutdown flag, which other serial tests toggle.

use super::helper_gate::{HelperGate, HelperPurpose};
use super::resident::{ClientLike, ResidentRefs, UpdateOutcome, WorkspacePool};
use crate::constants::{
    csharp_scip_concurrency, parse_csharp_scip_concurrency, CSHARP_SCIP_CONCURRENCY_ENV,
    SHUTDOWN_REQUESTED,
};
use anyhow::Result;
use serial_test::serial;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::time::Duration;

const BLOCKED_FOR: Duration = Duration::from_millis(300);
const COMPLETES_WITHIN: Duration = Duration::from_secs(10);
const JOB: Duration = Duration::from_millis(50);

fn wt() -> &'static Path {
    Path::new("C:/code/wt")
}

/// Concurrently running heavy jobs: the peak and the total count.
#[derive(Default)]
struct Overlap {
    running: AtomicUsize,
    peak: AtomicUsize,
    total: AtomicUsize,
}

impl Overlap {
    fn run(&self) {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(JOB);
        self.running.fetch_sub(1, Ordering::SeqCst);
        self.total.fetch_add(1, Ordering::SeqCst);
    }
}

/// Start one gated job per root at once; returns the peak overlap.
fn peak_overlap(gate: &Arc<HelperGate>, roots: &[PathBuf]) -> usize {
    let overlap = Arc::new(Overlap::default());
    let start = Arc::new(Barrier::new(roots.len()));
    let jobs: Vec<_> = roots
        .iter()
        .cloned()
        .map(|root| {
            let (gate, overlap, start) =
                (Arc::clone(gate), Arc::clone(&overlap), Arc::clone(&start));
            std::thread::spawn(move || {
                start.wait();
                let _permit = gate.acquire(HelperPurpose::Rebuild, &root).unwrap();
                overlap.run();
            })
        })
        .collect();
    for job in jobs {
        job.join().unwrap();
    }
    assert_eq!(overlap.total.load(Ordering::SeqCst), roots.len());
    overlap.peak.load(Ordering::SeqCst)
}

struct OkClient;

impl ClientLike for OkClient {
    fn update(&self, changed: &[PathBuf], deleted: &[PathBuf]) -> Result<UpdateOutcome> {
        Ok(UpdateOutcome::Applied(changed.len() + deleted.len()))
    }

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
    use crate::constants::{
        CSHARP_SCIP_CONCURRENCY_DEFAULT as D, CSHARP_SCIP_CONCURRENCY_MAX as M,
    };
    // A parsed 0 clamps to 1; unparseable input falls back to the default.
    let cases: &[(Option<&str>, usize)] = &[
        (None, D),
        (Some("0"), 1),
        (Some("1"), 1),
        (Some("2"), 2),
        (Some("4"), 4),
        (Some("9"), M),
        (Some("garbage"), D),
        (Some(""), D),
        (Some("-1"), D),
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
    for (raw, expected) in [
        ("3", 3),
        ("9", crate::constants::CSHARP_SCIP_CONCURRENCY_MAX),
        ("garbage", crate::constants::CSHARP_SCIP_CONCURRENCY_DEFAULT),
    ] {
        let _env = crate::testing::EnvRestore::set(&[(CSHARP_SCIP_CONCURRENCY_ENV, raw)]);
        assert_eq!(csharp_scip_concurrency(), expected, "raw = {raw:?}");
    }
    let _env = crate::testing::EnvRestore::remove(&[CSHARP_SCIP_CONCURRENCY_ENV]);
    assert_eq!(
        csharp_scip_concurrency(),
        crate::constants::CSHARP_SCIP_CONCURRENCY_DEFAULT
    );
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

/// Clears the global shutdown flag on drop, so a failed assertion cannot
/// leave it set for the next serial test.
struct ShutdownFlagReset;

impl Drop for ShutdownFlagReset {
    fn drop(&mut self) {
        SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    }
}

fn acquire_on_thread(
    gate: &Arc<HelperGate>,
    root: &'static Path,
) -> mpsc::Receiver<Result<(), String>> {
    let (tx, rx) = mpsc::channel();
    let gate_bg = Arc::clone(gate);
    std::thread::spawn(move || {
        let acquired = gate_bg.acquire(HelperPurpose::FindRefs, root).map(drop);
        let _ = tx.send(acquired.map_err(|e| e.to_string()));
    });
    rx
}

#[test]
#[serial]
fn shutdown_cancels_a_blocked_acquire_and_leaks_no_slot() {
    let cases: [(&str, &'static Path); 2] = [
        ("blocked on the worktree lock", wt()),
        ("blocked on the global slot", Path::new("C:/code/other-wt")),
    ];
    for (case, blocked_root) in cases {
        let _reset = ShutdownFlagReset;
        let gate = Arc::new(HelperGate::new(1));
        let holder = gate.acquire(HelperPurpose::Rebuild, wt()).unwrap();

        let blocked = acquire_on_thread(&gate, blocked_root);
        assert_eq!(
            blocked.recv_timeout(BLOCKED_FOR),
            Err(RecvTimeoutError::Timeout),
            "{case}: the second acquirer did not block"
        );

        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
        drop(holder);
        let outcome = blocked.recv_timeout(COMPLETES_WITHIN);
        SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
        assert!(
            matches!(outcome, Ok(Err(_))),
            "{case}: a blocked acquire must fail on shutdown, got {outcome:?}"
        );

        let fresh = acquire_on_thread(&gate, wt());
        assert_eq!(
            fresh.recv_timeout(COMPLETES_WITHIN),
            Ok(Ok(())),
            "{case}: the cancelled acquire leaked its slot or worktree lock"
        );
    }
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

fn update(pool: &WorkspacePool) -> Result<()> {
    match pool.update(
        &PathBuf::from("C:/code/wt/App.sln"),
        &[PathBuf::from("C:/code/wt/A.cs")],
        &[],
        COMPLETES_WITHIN,
    )? {
        UpdateOutcome::Applied(_) => Ok(()),
        other => anyhow::bail!("update not applied: {other:?}"),
    }
}

#[test]
fn a_request_or_update_to_a_resident_helper_does_not_take_the_gate() {
    type GatedOp = fn(&WorkspacePool) -> Result<()>;
    let cases: [(&str, GatedOp); 2] = [
        ("find-refs", |pool| lookup(pool).map(drop)),
        ("update", update),
    ];
    for (case, op) in cases {
        let gate = Arc::new(HelperGate::new(1));
        let pool = pool_with(&gate, || Ok(Arc::new(OkClient)));
        lookup(&pool).unwrap();

        let _held = gate.acquire(HelperPurpose::Rebuild, wt()).unwrap();
        let (tx, rx) = mpsc::channel();
        let pool_bg = Arc::clone(&pool);
        std::thread::spawn(move || {
            let _ = tx.send(op(&pool_bg).map_err(|e| e.to_string()));
        });
        assert_eq!(rx.recv_timeout(COMPLETES_WITHIN), Ok(Ok(())), "{case}");
    }
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

#[test]
#[serial]
fn capacity_four_serializes_heavy_work_on_one_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let cases: [(&str, Vec<PathBuf>); 2] = [
        ("one spelling", vec![root.clone(); 4]),
        (
            "equivalent spellings",
            vec![
                root.clone(),
                root.join("."),
                root.join("src").join(".."),
                root.join("src/.."),
            ],
        ),
    ];
    for (case, roots) in cases {
        let gate = Arc::new(HelperGate::new(4));
        assert_eq!(peak_overlap(&gate, &roots), 1, "{case}");
    }
}

#[test]
#[serial]
fn capacity_four_runs_two_worktrees_of_one_repo_in_parallel() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("repo");
    let linked = tmp.path().join("repo-feature");
    std::fs::create_dir_all(main.join(".git").join("worktrees").join("feature")).unwrap();
    std::fs::create_dir_all(&linked).unwrap();
    std::fs::write(
        linked.join(".git"),
        format!(
            "gitdir: {}",
            main.join(".git")
                .join("worktrees")
                .join("feature")
                .display()
        ),
    )
    .unwrap();

    let gate = Arc::new(HelperGate::new(4));
    let inside = Arc::new((Mutex::new(0usize), Condvar::new()));
    let jobs: Vec<_> = [main, linked]
        .into_iter()
        .map(|root| {
            let (gate, inside) = (Arc::clone(&gate), Arc::clone(&inside));
            std::thread::spawn(move || {
                let _permit = gate.acquire(HelperPurpose::Rebuild, &root).unwrap();
                let (count, joined) = &*inside;
                let mut count = count.lock().unwrap();
                *count += 1;
                joined.notify_all();
                let (_count, wait) = joined
                    .wait_timeout_while(count, COMPLETES_WITHIN, |n| *n < 2)
                    .unwrap();
                !wait.timed_out()
            })
        })
        .collect();
    for job in jobs {
        assert!(
            job.join().unwrap(),
            "the two worktrees never held the gate at the same time"
        );
    }
}

#[test]
#[serial]
fn a_job_queued_behind_its_worktree_holds_no_global_slot() {
    let gate = Arc::new(HelperGate::new(2));
    let first = gate.acquire(HelperPurpose::Rebuild, wt()).unwrap();

    let (queued_tx, queued_rx) = mpsc::channel();
    let gate_bg = Arc::clone(&gate);
    std::thread::spawn(move || {
        let acquired = gate_bg.acquire(HelperPurpose::FindRefs, wt()).map(drop);
        let _ = queued_tx.send(acquired.map_err(|e| e.to_string()));
    });
    assert_eq!(
        queued_rx.recv_timeout(BLOCKED_FOR),
        Err(RecvTimeoutError::Timeout),
        "a second job on a busy worktree started"
    );

    let (other_tx, other_rx) = mpsc::channel();
    let gate_bg = Arc::clone(&gate);
    std::thread::spawn(move || {
        let acquired = gate_bg
            .acquire(HelperPurpose::Rebuild, Path::new("C:/code/other-wt"))
            .map(drop);
        let _ = other_tx.send(acquired.map_err(|e| e.to_string()));
    });
    assert_eq!(
        other_rx.recv_timeout(COMPLETES_WITHIN),
        Ok(Ok(())),
        "the queued job took the slot another worktree needed"
    );

    drop(first);
    assert_eq!(queued_rx.recv_timeout(COMPLETES_WITHIN), Ok(Ok(())));
}

#[test]
#[serial]
fn rebuilds_and_a_resident_spawn_on_one_worktree_never_overlap() {
    let gate = Arc::new(HelperGate::new(4));
    let overlap = Arc::new(Overlap::default());
    let spawn_overlap = Arc::clone(&overlap);
    let pool = pool_with(&gate, move || {
        spawn_overlap.run();
        Ok(Arc::new(OkClient))
    });

    let start = Arc::new(Barrier::new(4));
    let mut jobs: Vec<std::thread::JoinHandle<Result<()>>> = (0..3)
        .map(|_| {
            let (gate, overlap, start) =
                (Arc::clone(&gate), Arc::clone(&overlap), Arc::clone(&start));
            std::thread::spawn(move || {
                start.wait();
                let _permit = gate.acquire(HelperPurpose::Rebuild, wt())?;
                overlap.run();
                Ok(())
            })
        })
        .collect();
    let start_lookup = Arc::clone(&start);
    jobs.push(std::thread::spawn(move || {
        start_lookup.wait();
        lookup(&pool).map(drop)
    }));
    for job in jobs {
        job.join().unwrap().unwrap();
    }

    assert_eq!(overlap.total.load(Ordering::SeqCst), 4);
    assert_eq!(overlap.peak.load(Ordering::SeqCst), 1);
}
