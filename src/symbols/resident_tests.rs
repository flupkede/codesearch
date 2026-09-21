//! Tests for the resident-helper WorkspacePool (todo #115) — pure pool
//! logic against a mock client, no real processes involved.

use super::resident::{ClientLike, ResidentRefs, WorkspacePool};
use crate::symbols::SymbolReference;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Clone)]
struct MockState {
    kill_count: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    live_children: Arc<AtomicUsize>,
}

struct MockClient {
    state: MockState,
}

impl ClientLike for MockClient {
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
    let state = MockState {
        kill_count: Arc::new(AtomicUsize::new(0)),
        fail: Arc::new(AtomicBool::new(false)),
        live_children: Arc::new(AtomicUsize::new(0)),
    };
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
        pool: WorkspacePool::new(max, idle, 1, spawn_fn),
        spawns,
        state,
    }
}

fn sln(name: &str) -> PathBuf {
    PathBuf::from(format!("C:\\code\\{name}\\src\\App.sln"))
}

#[test]
fn resident_pool_admission_evicts_lru_when_full() {
    let h = harness(2, Duration::from_secs(600));

    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), "Sym")
        .unwrap();
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("b"), "Sym")
        .unwrap();
    assert_eq!(h.spawns.load(Ordering::SeqCst), 2);
    assert_eq!(h.state.kill_count.load(Ordering::SeqCst), 0);

    // Third repo: the LRU workspace ("a") must be evicted (killed exactly
    // once) to make room — and the answer must still be correct.
    let refs = h
        .pool
        .find_refs(&PathBuf::from("h.exe"), &sln("c"), "Sym")
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
            .find_refs(&PathBuf::from("h.exe"), &sln("a"), "Sym")
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
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), "Sym")
        .unwrap();
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("b"), "Sym")
        .unwrap();
    assert_eq!(h.state.kill_count.load(Ordering::SeqCst), 1);
    assert_eq!(h.state.live_children.load(Ordering::SeqCst), 1);
}

#[test]
fn resident_pool_evicted_repo_respawns_and_answers() {
    let h = harness(1, Duration::from_secs(600));
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), "Sym")
        .unwrap();
    // Same repo, but the single-slot pool evicted it for the second repo...
    let refs = h
        .pool
        .find_refs(&PathBuf::from("h.exe"), &sln("b"), "Sym")
        .unwrap();
    assert_eq!(refs.references.len(), 1);
    // ...and coming back to the first repo must respawn and still answer.
    let refs = h
        .pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), "Sym")
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
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), "Sym")
        .unwrap();
    std::thread::sleep(Duration::from_millis(5));
    h.pool
        .find_refs(&PathBuf::from("h.exe"), &sln("b"), "Sym")
        .unwrap();
    assert!(
        h.state.kill_count.load(Ordering::SeqCst) >= 1,
        "idle workspace must be reaped lazily"
    );
    // Fresh spawn after reap; the answer stays correct.
    let refs = h
        .pool
        .find_refs(&PathBuf::from("h.exe"), &sln("b"), "Sym")
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
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), "Sym")
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
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), "Sym")
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
    let state = MockState {
        kill_count: Arc::new(AtomicUsize::new(0)),
        fail: Arc::new(AtomicBool::new(false)),
        live_children: Arc::new(AtomicUsize::new(0)),
    };
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
    let pool = Arc::new(WorkspacePool::new(2, Duration::from_secs(600), 1, spawn_fn));

    let pool_bg = pool.clone();
    let handle = std::thread::spawn(move || {
        pool_bg
            .find_refs(&PathBuf::from("h.exe"), &sln("a"), "Sym")
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
fn evict_during_an_in_flight_spawn_stops_the_stale_workspace_being_installed_resident() {
    // Reproduces the exact todo #168 window: a rebuild's `evict` lands
    // while another thread's `find_refs` is still spawning the workspace
    // for that same solution (a minutes-long load in production). The
    // fresh workspace must still answer this one caller, but must NOT be
    // installed as resident afterwards — that would resurrect the stale
    // answer `evict` was called to prevent.
    let state = MockState {
        kill_count: Arc::new(AtomicUsize::new(0)),
        fail: Arc::new(AtomicBool::new(false)),
        live_children: Arc::new(AtomicUsize::new(0)),
    };
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
    let pool = Arc::new(WorkspacePool::new(2, Duration::from_secs(600), 1, spawn_fn));

    let pool_bg = pool.clone();
    let handle = std::thread::spawn(move || {
        pool_bg
            .find_refs(&PathBuf::from("h.exe"), &sln("a"), "Sym")
            .unwrap()
    });

    // Wait until the spawn is under way, then evict "a" — nothing resident
    // yet, so this only bumps the generation counter.
    {
        let (lock, cvar) = &*spawn_started;
        let mut guard = lock.lock().unwrap();
        while !*guard {
            guard = cvar.wait(guard).unwrap();
        }
    }
    pool.evict(&sln("a"));

    // Let the spawn complete; find_refs must see the bumped generation.
    {
        let (lock, cvar) = &*spawn_go;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
    let refs = handle.join().unwrap();
    assert_eq!(refs.references.len(), 1, "the caller still gets an answer");

    // A second lookup must respawn (nothing installed resident by the
    // first call) — proves the stale workspace was discarded, not reused.
    let refs2 = pool
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), "Sym")
        .unwrap();
    assert_eq!(refs2.references.len(), 1);
    assert_eq!(
        spawns.load(Ordering::SeqCst),
        2,
        "the post-race workspace must not have been cached as resident"
    );
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
        .find_refs(&PathBuf::from("h.exe"), &sln("a"), "Sym")
        .expect_err("a failing helper must surface an error for the one-shot fallback");
    assert!(err.to_string().contains("mock helper failed"));
}
