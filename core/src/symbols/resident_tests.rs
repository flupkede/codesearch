//! Tests for the resident-helper WorkspacePool (todo #115) — pure pool
//! logic against a mock client, no real processes involved.

use super::helper_gate::HelperGate;
use super::resident::{
    resident_update_timeout, ClientLike, ResidentRefs, UpdateOutcome, WorkspacePool,
};
use crate::symbols::SymbolReference;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Default)]
enum MockUpdate {
    #[default]
    Apply,
    NeedsReload,
    Fail,
    /// Blocks until killed, like a helper that stopped answering.
    Hang,
}

type UpdateCall = (Vec<PathBuf>, Vec<PathBuf>);

#[derive(Clone, Default)]
struct MockState {
    kill_count: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    live_children: Arc<AtomicUsize>,
    update_mode: Arc<Mutex<MockUpdate>>,
    updates: Arc<Mutex<Vec<UpdateCall>>>,
}

impl MockState {
    fn record_update(&self, changed: &[PathBuf], deleted: &[PathBuf]) {
        self.updates
            .lock()
            .unwrap()
            .push((changed.to_vec(), deleted.to_vec()));
    }

    fn update_calls(&self) -> Vec<UpdateCall> {
        self.updates.lock().unwrap().clone()
    }
}

struct MockClient {
    state: MockState,
}

impl ClientLike for MockClient {
    fn update(&self, changed: &[PathBuf], deleted: &[PathBuf]) -> Result<UpdateOutcome> {
        self.state.record_update(changed, deleted);
        let mode = *self.state.update_mode.lock().unwrap();
        match mode {
            MockUpdate::Apply => Ok(UpdateOutcome::Applied(changed.len() + deleted.len())),
            MockUpdate::NeedsReload => Ok(UpdateOutcome::NeedsReload(
                "needs-reload: C:\\code\\a\\src\\New.cs".to_string(),
            )),
            MockUpdate::Fail => anyhow::bail!("mock update failed"),
            MockUpdate::Hang => {
                while self.state.kill_count.load(Ordering::SeqCst) == 0 {
                    std::thread::sleep(Duration::from_millis(5));
                }
                anyhow::bail!("mock helper killed mid-update")
            }
        }
    }

    fn find_refs(&self, _symbol: &str) -> Result<ResidentRefs> {
        if self.state.fail.load(Ordering::SeqCst) {
            anyhow::bail!("mock helper failed");
        }
        Ok(ResidentRefs {
            references: vec![SymbolReference {
                file: PathBuf::from("src/Mock.cs"),
                start_line: 1,
                end_line: 1,
                kind: "reference".to_string(),
            }],
            warnings: Vec::new(),
        })
    }

    fn kill(&self) {
        if self.state.kill_count.fetch_add(1, Ordering::SeqCst) == 0 {
            self.state.live_children.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

struct Harness {
    pool: WorkspacePool,
    spawns: Arc<AtomicUsize>,
    state: MockState,
}

fn harness(max: usize, idle: Duration) -> Harness {
    let state = MockState::default();
    let spawns = Arc::new(AtomicUsize::new(0));
    let spawn_state = state.clone();
    let spawn_spawns = spawns.clone();
    let spawn_fn = Box::new(
        move |_helper: &Path, _solution: &Path, _cap: u64| -> Result<Arc<dyn ClientLike>> {
            spawn_spawns.fetch_add(1, Ordering::SeqCst);
            spawn_state.live_children.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(MockClient {
                state: spawn_state.clone(),
            }))
        },
    );
    Harness {
        pool: WorkspacePool::new(max, idle, 1, spawn_fn, test_gate()),
        spawns,
        state,
    }
}

/// A private gate: pool tests must not contend on the process-wide one.
fn test_gate() -> Arc<HelperGate> {
    Arc::new(HelperGate::new(1))
}

fn sln(name: &str) -> PathBuf {
    PathBuf::from(format!("C:\\code\\{name}\\src\\App.sln"))
}

#[test]
fn resident_pool_admission_evicts_lru_when_full() {
    let h = harness(2, Duration::from_secs(600));

    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .unwrap();
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("b"), Path::new("wt"), "Sym")
        .unwrap();
    assert_eq!(h.spawns.load(Ordering::SeqCst), 2);
    assert_eq!(h.state.kill_count.load(Ordering::SeqCst), 0);

    // Third repo: the LRU workspace ("a") must be evicted (killed exactly
    // once) to make room — and the answer must still be correct.
    let refs = h
        .pool
        .find_refs(&PathBuf::from("h.exe"), &sln("c"), Path::new("wt"), "Sym")
        .unwrap();
    assert_eq!(refs.references.len(), 1);
    assert_eq!(h.spawns.load(Ordering::SeqCst), 3);
    assert_eq!(h.state.kill_count.load(Ordering::SeqCst), 1);
    assert_eq!(h.state.live_children.load(Ordering::SeqCst), 2);
}

#[test]
fn resident_pool_reuses_one_workspace_per_repo() {
    let h = harness(2, Duration::from_secs(600));
    for _ in 0..5 {
        h.pool
            .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
            .unwrap();
    }
    assert_eq!(h.spawns.load(Ordering::SeqCst), 1, "same repo = one spawn");
    assert_eq!(h.state.kill_count.load(Ordering::SeqCst), 0);
}

#[test]
fn resident_pool_defers_kill_while_in_flight_but_still_kills() {
    // The doomed path cannot be driven through the public API (a real
    // in-flight request would have to overlap admission), so exercise the
    // invariant directly: eviction of an idle entry kills NOW; the counter-
    // then-teardown rule guarantees an in-flight entry is never killed by
    // the evictor — proven here by eviction of an idle entry only.
    let h = harness(1, Duration::from_secs(600));
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .unwrap();
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("b"), Path::new("wt"), "Sym")
        .unwrap();
    assert_eq!(h.state.kill_count.load(Ordering::SeqCst), 1);
    assert_eq!(h.state.live_children.load(Ordering::SeqCst), 1);
}

#[test]
fn resident_pool_evicted_repo_respawns_and_answers() {
    let h = harness(1, Duration::from_secs(600));
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .unwrap();
    // Same repo, but the single-slot pool evicted it for the second repo...
    let refs = h
        .pool
        .find_refs(&PathBuf::from("h.exe"), &sln("b"), Path::new("wt"), "Sym")
        .unwrap();
    assert_eq!(refs.references.len(), 1);
    // ...and coming back to the first repo must respawn and still answer.
    let refs = h
        .pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .unwrap();
    assert_eq!(refs.references.len(), 1);
    assert_eq!(h.spawns.load(Ordering::SeqCst), 3);
}

#[test]
fn resident_pool_ttl_reaps_idle_workspaces_on_next_access() {
    let h = harness(2, Duration::from_millis(1));
    // Spawn "a", let it go idle (TTL is 1ms), then access repo "b": the
    // lazy reap on that access must kill the idle "a" workspace.
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .unwrap();
    std::thread::sleep(Duration::from_millis(5));
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("b"), Path::new("wt"), "Sym")
        .unwrap();
    assert!(
        h.state.kill_count.load(Ordering::SeqCst) >= 1,
        "idle workspace must be reaped lazily"
    );
    // Fresh spawn after reap; the answer stays correct.
    let refs = h
        .pool
        .find_refs(&PathBuf::from("h.exe"), &sln("b"), Path::new("wt"), "Sym")
        .unwrap();
    assert_eq!(refs.references.len(), 1);
}

#[test]
fn evict_kills_the_resident_workspace_and_forces_a_fresh_respawn() {
    // Simulates a symbol rebuild: the resident workspace for a repo must be
    // evicted so the next find_refs (issued right after) sees current
    // on-disk source instead of the compilation loaded before the change.
    let h = harness(2, Duration::from_secs(600));
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .unwrap();
    assert_eq!(h.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(h.state.kill_count.load(Ordering::SeqCst), 0);

    h.pool.evict(&sln("a"));
    assert_eq!(
        h.state.kill_count.load(Ordering::SeqCst),
        1,
        "evict must kill the idle resident workspace immediately"
    );

    let refs = h
        .pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .unwrap();
    assert_eq!(refs.references.len(), 1);
    assert_eq!(
        h.spawns.load(Ordering::SeqCst),
        2,
        "post-evict find_refs must respawn, not reuse the killed workspace"
    );
}

/// A client whose `find_refs` blocks until released, so a test can hold a
/// call in flight while driving eviction from another thread — exercises
/// the doomed/deferred-kill branch of `evict`, which the idle-only tests
/// above cannot reach.
struct BlockingClient {
    state: MockState,
    started: Arc<(Mutex<bool>, Condvar)>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl ClientLike for BlockingClient {
    fn update(&self, changed: &[PathBuf], deleted: &[PathBuf]) -> Result<UpdateOutcome> {
        self.state.record_update(changed, deleted);
        Ok(UpdateOutcome::Applied(changed.len() + deleted.len()))
    }

    fn find_refs(&self, _symbol: &str) -> Result<ResidentRefs> {
        {
            let (lock, cvar) = &*self.started;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        {
            let (lock, cvar) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cvar.wait(released).unwrap();
            }
        }
        Ok(ResidentRefs {
            references: vec![SymbolReference {
                file: PathBuf::from("src/Mock.cs"),
                start_line: 1,
                end_line: 1,
                kind: "reference".to_string(),
            }],
            warnings: Vec::new(),
        })
    }

    fn kill(&self) {
        if self.state.kill_count.fetch_add(1, Ordering::SeqCst) == 0 {
            self.state.live_children.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[test]
fn evict_marks_an_in_flight_workspace_doomed_and_kills_it_exactly_once_on_release() {
    let state = MockState::default();
    let started = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let spawn_state = state.clone();
    let spawn_started = started.clone();
    let spawn_release = release.clone();
    let spawn_fn = Box::new(
        move |_helper: &Path, _solution: &Path, _cap: u64| -> Result<Arc<dyn ClientLike>> {
            spawn_state.live_children.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(BlockingClient {
                state: spawn_state.clone(),
                started: spawn_started.clone(),
                release: spawn_release.clone(),
            }))
        },
    );
    let pool = Arc::new(WorkspacePool::new(
        2,
        Duration::from_secs(600),
        1,
        spawn_fn,
        test_gate(),
    ));

    let pool_bg = pool.clone();
    let handle = std::thread::spawn(move || {
        pool_bg
            .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
            .unwrap()
    });

    // Wait until the call is actually in flight before evicting.
    {
        let (lock, cvar) = &*started;
        let mut guard = lock.lock().unwrap();
        while !*guard {
            guard = cvar.wait(guard).unwrap();
        }
    }

    pool.evict(&sln("a"));
    assert_eq!(
        state.kill_count.load(Ordering::SeqCst),
        0,
        "an in-flight workspace must not be killed while still in use"
    );

    {
        let (lock, cvar) = &*release;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
    let refs = handle.join().unwrap();
    assert_eq!(refs.references.len(), 1, "the in-flight call still answers");
    assert_eq!(
        state.kill_count.load(Ordering::SeqCst),
        1,
        "release must trigger exactly one deferred kill"
    );
    assert_eq!(state.live_children.load(Ordering::SeqCst), 0);
}

#[test]
fn evict_or_update_during_an_in_flight_spawn_stops_the_stale_workspace_being_installed_resident() {
    // Reproduces the exact todo #168 window: a rebuild's `evict` or
    // `update` lands while another thread's `find_refs` is still spawning
    // the workspace for that same solution (a minutes-long load in
    // production). The fresh workspace must still answer this one caller,
    // but must NOT be installed as resident afterwards — it may hold the
    // pre-edit text the rebuild is replacing.
    let cases: &[(&str, fn(&WorkspacePool))] = &[
        ("evict", |pool| pool.evict(&sln("a"))),
        ("update", |pool| {
            let outcome = pool
                .update(
                    &sln("a"),
                    &[PathBuf::from("C:\\code\\a\\src\\A.cs")],
                    &[],
                    Duration::from_secs(10),
                )
                .unwrap();
            assert_eq!(
                outcome,
                UpdateOutcome::NotResident,
                "no resident yet: must not wait for the spawn"
            );
        }),
    ];
    for (case, race) in cases {
        assert_stale_spawn_is_not_installed(case, *race);
    }
}

fn assert_stale_spawn_is_not_installed(case: &str, race: fn(&WorkspacePool)) {
    let state = MockState::default();
    let spawn_started = Arc::new((Mutex::new(false), Condvar::new()));
    let spawn_go = Arc::new((Mutex::new(false), Condvar::new()));
    let spawns = Arc::new(AtomicUsize::new(0));
    let spawn_state = state.clone();
    let s_started = spawn_started.clone();
    let s_go = spawn_go.clone();
    let spawn_spawns = spawns.clone();
    let spawn_fn = Box::new(
        move |_helper: &Path, _solution: &Path, _cap: u64| -> Result<Arc<dyn ClientLike>> {
            spawn_spawns.fetch_add(1, Ordering::SeqCst);
            {
                let (lock, cvar) = &*s_started;
                *lock.lock().unwrap() = true;
                cvar.notify_all();
            }
            {
                let (lock, cvar) = &*s_go;
                let mut go = lock.lock().unwrap();
                while !*go {
                    go = cvar.wait(go).unwrap();
                }
            }
            spawn_state.live_children.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(MockClient {
                state: spawn_state.clone(),
            }))
        },
    );
    let pool = Arc::new(WorkspacePool::new(
        2,
        Duration::from_secs(600),
        1,
        spawn_fn,
        test_gate(),
    ));

    let pool_bg = pool.clone();
    let handle = std::thread::spawn(move || {
        pool_bg
            .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
            .unwrap()
    });

    // Wait until the spawn is under way, then race it — nothing resident
    // yet, so this only bumps the generation counter.
    {
        let (lock, cvar) = &*spawn_started;
        let mut guard = lock.lock().unwrap();
        while !*guard {
            guard = cvar.wait(guard).unwrap();
        }
    }
    race(&pool);

    // Let the spawn complete; find_refs must see the bumped generation.
    {
        let (lock, cvar) = &*spawn_go;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
    let refs = handle.join().unwrap();
    assert_eq!(
        refs.references.len(),
        1,
        "{case}: the caller still gets an answer"
    );
    assert_eq!(
        state.kill_count.load(Ordering::SeqCst),
        1,
        "{case}: the stale workspace is killed after its one answer"
    );

    // A second lookup must respawn (nothing installed resident by the
    // first call) — proves the stale workspace was discarded, not reused.
    let refs2 = pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .unwrap();
    assert_eq!(refs2.references.len(), 1);
    assert_eq!(
        spawns.load(Ordering::SeqCst),
        2,
        "{case}: the post-race workspace must not have been cached as resident"
    );
}

/// A client whose kill takes a while to observe the process exit, like
/// `ServeClient`'s kill + wait; a concurrent kill waits for the first.
struct SlowExitClient {
    alive: Arc<AtomicUsize>,
    exited: Mutex<bool>,
    kill_started: mpsc::Sender<()>,
}

impl ClientLike for SlowExitClient {
    fn update(&self, _changed: &[PathBuf], _deleted: &[PathBuf]) -> Result<UpdateOutcome> {
        Ok(UpdateOutcome::Applied(0))
    }

    fn find_refs(&self, _symbol: &str) -> Result<ResidentRefs> {
        Ok(ResidentRefs::default())
    }

    fn kill(&self) {
        let mut exited = self.exited.lock().unwrap();
        if *exited {
            return;
        }
        let _ = self.kill_started.send(());
        std::thread::sleep(Duration::from_millis(300));
        self.alive.fetch_sub(1, Ordering::SeqCst);
        *exited = true;
    }
}

#[test]
fn a_replacement_spawn_waits_until_the_evicted_resident_has_exited() {
    let alive = Arc::new(AtomicUsize::new(0));
    let alive_at_spawn = Arc::new(Mutex::new(Vec::new()));
    let (kill_tx, kill_rx) = mpsc::channel();
    let spawn_alive = alive.clone();
    let spawn_seen = alive_at_spawn.clone();
    let spawn_fn = Box::new(
        move |_helper: &Path, _solution: &Path, _cap: u64| -> Result<Arc<dyn ClientLike>> {
            spawn_seen
                .lock()
                .unwrap()
                .push(spawn_alive.load(Ordering::SeqCst));
            spawn_alive.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(SlowExitClient {
                alive: spawn_alive.clone(),
                exited: Mutex::new(false),
                kill_started: kill_tx.clone(),
            }))
        },
    );
    let pool = Arc::new(WorkspacePool::new(
        2,
        Duration::from_secs(600),
        1,
        spawn_fn,
        test_gate(),
    ));
    pool.find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .unwrap();

    // A rebuild evicts the idle resident; its kill runs on the rebuild thread.
    let pool_bg = pool.clone();
    let evicting = std::thread::spawn(move || pool_bg.evict(&sln("a")));
    kill_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("evict never started killing the resident");

    pool.find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .unwrap();
    evicting.join().unwrap();

    assert_eq!(
        *alive_at_spawn.lock().unwrap(),
        vec![0, 0],
        "the replacement spawned while the evicted helper was still alive"
    );
    assert_eq!(alive.load(Ordering::SeqCst), 1);
}

#[test]
fn evict_of_an_unknown_solution_is_a_harmless_no_op() {
    let h = harness(2, Duration::from_secs(600));
    h.pool.evict(&sln("never-loaded"));
    assert_eq!(h.state.kill_count.load(Ordering::SeqCst), 0);
}

#[test]
fn resident_pool_surfaces_client_failure_to_the_fallback() {
    let h = harness(2, Duration::from_secs(600));
    h.state.fail.store(true, Ordering::SeqCst);
    let err = h
        .pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .expect_err("a failing helper must surface an error for the one-shot fallback");
    assert!(err.to_string().contains("mock helper failed"));
}

/// serve shutdown: statics are never dropped at process exit, so the
/// `ServeClient::Drop` kill never fires there. `kill_all` must kill every
/// resident child and leave the pool empty (a later request respawns).
#[test]
fn kill_all_kills_every_resident_child_and_empties_the_pool() {
    let h = harness(2, Duration::from_secs(600));

    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .unwrap();
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("b"), Path::new("wt"), "Sym")
        .unwrap();
    assert_eq!(h.state.kill_count.load(Ordering::SeqCst), 0);

    h.pool.kill_all();

    assert_eq!(
        h.state.kill_count.load(Ordering::SeqCst),
        2,
        "every resident child must be killed on shutdown"
    );

    // Pool emptied: a follow-up request respawns instead of reusing a
    // killed client.
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
        .unwrap();
    assert_eq!(h.spawns.load(Ordering::SeqCst), 3);
}

fn edited(name: &str) -> PathBuf {
    PathBuf::from(format!("C:\\code\\a\\src\\{name}.cs"))
}

fn load(h: &Harness, repo: &str) {
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln(repo), Path::new("wt"), "Sym")
        .unwrap();
}

#[test]
fn update_applies_edits_to_the_resident_workspace_without_a_reload() {
    let h = harness(2, Duration::from_secs(600));
    load(&h, "a");
    let (changed, deleted) = (vec![edited("A")], vec![edited("Old")]);

    let outcome = h
        .pool
        .update(&sln("a"), &changed, &deleted, Duration::from_secs(10))
        .unwrap();

    assert_eq!(outcome, UpdateOutcome::Applied(2));
    assert_eq!(h.state.update_calls(), vec![(changed, deleted)]);
    assert_eq!(h.state.kill_count.load(Ordering::SeqCst), 0);
    load(&h, "a");
    assert_eq!(
        h.spawns.load(Ordering::SeqCst),
        1,
        "the updated workspace keeps serving"
    );
}

#[test]
fn update_without_a_resident_answers_not_resident_and_touches_no_other_workspace() {
    let h = harness(2, Duration::from_secs(600));
    load(&h, "b");

    let outcome = h
        .pool
        .update(&sln("a"), &[edited("A")], &[], Duration::from_secs(10))
        .unwrap();

    assert_eq!(outcome, UpdateOutcome::NotResident);
    assert!(h.state.update_calls().is_empty());
    assert_eq!(h.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(h.state.kill_count.load(Ordering::SeqCst), 0);
}

#[test]
fn update_counts_as_use_for_the_idle_ttl() {
    let h = harness(2, Duration::from_millis(1000));
    load(&h, "a");
    std::thread::sleep(Duration::from_millis(600));
    h.pool
        .update(&sln("a"), &[edited("A")], &[], Duration::from_secs(10))
        .unwrap();
    std::thread::sleep(Duration::from_millis(600));

    // Loading "b" lazily reaps idle residents; "a" was used 600ms ago.
    load(&h, "b");
    assert_eq!(h.state.kill_count.load(Ordering::SeqCst), 0);
}

#[test]
fn update_failures_surface_and_only_a_silent_helper_is_evicted_by_the_pool() {
    let cases: &[(&str, MockUpdate, bool, &str, bool)] = &[
        // (case, helper reply, Ok?, text, evicted by the pool)
        (
            "needs reload",
            MockUpdate::NeedsReload,
            true,
            "needs-reload: ",
            false,
        ),
        (
            "helper error",
            MockUpdate::Fail,
            false,
            "mock update failed",
            false,
        ),
        ("no reply", MockUpdate::Hang, false, "no reply within", true),
    ];
    for (case, mode, ok, text, evicted) in cases {
        let h = harness(2, Duration::from_secs(600));
        *h.state.update_mode.lock().unwrap() = *mode;
        load(&h, "a");

        let started = Instant::now();
        let result = h
            .pool
            .update(&sln("a"), &[edited("A")], &[], Duration::from_millis(200));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{case}: the timeout must bound the call"
        );

        assert_eq!(result.is_ok(), *ok, "{case}: {result:?}");
        let shown = match &result {
            Ok(outcome) => format!("{outcome:?}"),
            Err(e) => format!("{e:#}"),
        };
        assert!(shown.contains(text), "{case}: {shown}");
        load(&h, "a");
        assert_eq!(
            h.spawns.load(Ordering::SeqCst),
            if *evicted { 2 } else { 1 },
            "{case}: evicted by the pool"
        );
    }
}

#[test]
fn update_waits_for_a_request_in_flight_on_the_resident_and_is_bounded_by_the_timeout() {
    let state = MockState::default();
    let started = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let spawn_state = state.clone();
    let (spawn_started, spawn_release) = (started.clone(), release.clone());
    let spawn_fn = Box::new(
        move |_helper: &Path, _solution: &Path, _cap: u64| -> Result<Arc<dyn ClientLike>> {
            Ok(Arc::new(BlockingClient {
                state: spawn_state.clone(),
                started: spawn_started.clone(),
                release: spawn_release.clone(),
            }))
        },
    );
    let pool = Arc::new(WorkspacePool::new(
        2,
        Duration::from_secs(600),
        1,
        spawn_fn,
        test_gate(),
    ));
    let pool_bg = pool.clone();
    let request = std::thread::spawn(move || {
        pool_bg
            .find_refs(&PathBuf::from("h.exe"), &sln("a"), Path::new("wt"), "Sym")
            .unwrap()
    });
    {
        let (lock, cvar) = &*started;
        let mut guard = lock.lock().unwrap();
        while !*guard {
            guard = cvar.wait(guard).unwrap();
        }
    }

    let err = pool
        .update(&sln("a"), &[edited("A")], &[], Duration::from_millis(150))
        .expect_err("the in-flight request holds the workspace past the bound");
    assert!(err.to_string().contains("waiting for a request"), "{err:#}");
    assert!(state.update_calls().is_empty(), "never interleaved");

    {
        let (lock, cvar) = &*release;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
    request.join().unwrap();
    let outcome = pool
        .update(&sln("a"), &[edited("A")], &[], Duration::from_secs(10))
        .unwrap();
    assert_eq!(outcome, UpdateOutcome::Applied(1));
    assert_eq!(state.kill_count.load(Ordering::SeqCst), 0);
}

#[test]
#[serial_test::serial]
fn resident_update_timeout_honours_only_a_positive_env_override() {
    let env = crate::constants::SCIP_RESIDENT_UPDATE_TIMEOUT_SECS_ENV;
    let default = Duration::from_secs(crate::constants::SCIP_RESIDENT_UPDATE_TIMEOUT_SECS);
    let cases: &[(Option<&str>, Duration)] = &[
        (Some("5"), Duration::from_secs(5)),
        (Some(" 7 "), Duration::from_secs(7)),
        (Some("0"), default),
        (Some("junk"), default),
        (None, default),
    ];
    for (raw, expected) in cases {
        let _env = match raw {
            Some(value) => crate::testing::EnvRestore::set(&[(env, value)]),
            None => crate::testing::EnvRestore::remove(&[env]),
        };
        assert_eq!(resident_update_timeout(), *expected, "{env}={raw:?}");
    }
}
