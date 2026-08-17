use super::*;
use std::io::Write;

#[test]
fn test_api_key_matches() {
    assert!(api_key_matches("secret-key", "secret-key"));
    assert!(!api_key_matches("secret-key", "secret-keX"));
    assert!(!api_key_matches("secret", "secret-key")); // different length
    assert!(!api_key_matches("", "secret-key"));
    assert!(api_key_matches("", "")); // both empty digests are equal
                                      // Case-sensitive and exact.
    assert!(!api_key_matches("Secret-Key", "secret-key"));
}

#[test]
fn rest_service_drop_does_not_touch_active_sessions() {
    // Per-request REST services (built via make_service for /search /find
    // /explore /chunk, NOT the serve MCP session factory) must never touch
    // active_sessions: their Drop must NOT decrement the counter, or it
    // underflows to u64::MAX. Regression guard for the tracks_session fix.
    let state = std::sync::Arc::new(ServeState::new(ReposConfig::default(), None));
    {
        let _svc = crate::mcp::CodesearchService::new_for_serve(state.clone()).unwrap();
    }
    assert_eq!(
        state.active_session_count(),
        0,
        "REST service drop underflowed active_sessions"
    );
}

#[test]
fn tracked_session_drop_balances_active_sessions() {
    // A genuine MCP session increments on connect and the serve factory
    // marks it tracked, so Drop decrements and the counter returns to 0.
    let state = std::sync::Arc::new(ServeState::new(ReposConfig::default(), None));
    let _id = state.session_connected();
    {
        let mut svc = crate::mcp::CodesearchService::new_for_serve(state.clone()).unwrap();
        svc.mark_session_tracked();
    }
    assert_eq!(
        state.active_session_count(),
        0,
        "tracked session did not balance"
    );
}

#[tokio::test]
async fn await_fsw_shutdown_joins_exited_task_and_removes_entry() {
    // `await_fsw_shutdown` must (a) remove the alias from `fsw_tasks` and
    // (b) actually await (join) the task to completion — not just drop the
    // handle. We prove the join happened by observing a side-effect the
    // task sets on exit. Regression guard for the Windows DB-delete fix:
    // if someone removes the join, the LMDB env stays open and the task's
    // Arc<SharedStores> clone keeps the mmap handle locked on Windows.
    let state = ServeState::new(ReposConfig::default(), None);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_clone = done.clone();
    let handle = tokio::spawn(async move {
        // Yield once so the task isn't already-finished at insert time.
        tokio::task::yield_now().await;
        done_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    state.fsw_tasks.insert("repo-x".to_string(), handle);
    state.await_fsw_shutdown("repo-x").await;
    assert!(
        !state.fsw_tasks.contains_key("repo-x"),
        "fsw_tasks entry not removed"
    );
    assert!(
        done.load(std::sync::atomic::Ordering::SeqCst),
        "FSW task was not joined to completion"
    );
}

#[tokio::test]
async fn await_fsw_shutdown_noop_on_missing_alias() {
    // A repo that never had an FSW task (Warm/Readonly/Conflicted) must
    // not panic — the map lookup is the no-op guard.
    let state = ServeState::new(ReposConfig::default(), None);
    state.await_fsw_shutdown("never-spawned").await;
    assert!(state.fsw_tasks.is_empty());
}

#[tokio::test]
async fn await_index_task_cancels_and_joins_indexing_task() {
    // FINDINGS #1: `remove_repo` stops an in-flight indexing pass via
    // `await_index_task`, which must (a) remove the alias from `index_tasks`,
    // (b) cancel the task's OWN token, and (c) actually await (join) the task
    // to completion — so the task's `Arc<SharedStores>` clone drops and the
    // LMDB mmap closes BEFORE the DB directory delete. Before BUG1, a
    // freshly-added repo's embed pass ran in a detached, untracked task that
    // ignored its token; this locks the tracking + cancellation + join.
    let state = ServeState::new(ReposConfig::default(), None);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_clone = done.clone();
    let token = CancellationToken::new();
    let token_clone = token.clone();
    let handle = tokio::spawn(async move {
        // Spin until cancelled — proving `await_index_task`'s `token.cancel()`
        // actually propagates to the task, not just that the task happened to
        // finish on its own.
        while !token_clone.is_cancelled() {
            tokio::task::yield_now().await;
        }
        done_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    state
        .index_tasks
        .insert("repo-x".to_string(), (handle, token));
    state.await_index_task("repo-x").await;
    assert!(
        !state.index_tasks.contains_key("repo-x"),
        "index_tasks entry not removed"
    );
    assert!(
        done.load(std::sync::atomic::Ordering::SeqCst),
        "indexing task was not cancelled + joined to completion"
    );
}

#[tokio::test]
async fn remove_repo_reports_db_deleted_when_delete_succeeds() {
    // FINDINGS #2: `remove_repo` must report the REAL DB-delete outcome, not
    // always "DB deleted". On the success path `RepoRemovalOutcome.db_deleted`
    // must be `true` and the directory gone from disk. Uses a config-path
    // override so the real `~/.codesearch/repos.json` is never touched.
    let (_tmp, repo_path, state) = state_with_repo("somerepo");
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();
    // Put a file in the DB dir so delete has real work.
    std::fs::write(db_path.join("data.mdb"), "fake").unwrap();

    let outcome = state
        .remove_repo("somerepo")
        .await
        .expect("remove_repo should succeed on the happy path");

    assert!(outcome.db_deleted, "db_deleted must be true on success");
    assert!(
        outcome.db_delete_error.is_none(),
        "no delete error on success, got: {:?}",
        outcome.db_delete_error
    );
    assert!(!db_path.exists(), "DB directory must be removed from disk");
}

#[tokio::test]
async fn remove_repo_reports_db_locked_when_delete_fails() {
    // FINDINGS #2: when the DB path CANNOT be removed, `RepoRemovalOutcome`
    // must honestly report `db_deleted == false` plus a reason — NOT claim
    // success (the BUG2 "always Ok" swallow). We force a deterministic,
    // cross-platform delete failure by making `db_path` a regular file
    // (`remove_dir_all` errors on a non-directory), exercising the retry
    // loop's failure branch without depending on OS file-locking quirks.
    let (_tmp, repo_path, state) = state_with_repo("somerepo");
    // db_path is a FILE, not a directory -> remove_dir_all fails every retry.
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::write(&db_path, "not a directory").unwrap();

    let outcome = state
        .remove_repo("somerepo")
        .await
        .expect("remove_repo returns Ok(outcome); delete failure is non-fatal");

    assert!(
        !outcome.db_deleted,
        "db_deleted must be false when the delete fails"
    );
    assert!(
        outcome.db_delete_error.is_some(),
        "a delete error reason must be present on failure"
    );
}

#[test]
fn remove_orphaned_db_dir_deletes_a_present_directory() {
    // Regression guard for the self-cleanup backstop: when a background
    // indexing task finishes an uninterruptible `build_index` for an alias
    // that was removed mid-build, its post-build guard drops its stores
    // handle and calls `remove_orphaned_db_dir` to delete the now-orphaned
    // `.codesearch.db` directory. Without this mechanism the dir would stay
    // locked (and on disk) until a serve restart.
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();
    std::fs::write(db_path.join("data.mdb"), "fake").unwrap();

    ServeState::remove_orphaned_db_dir("orphan", &db_path);

    assert!(
        !db_path.exists(),
        "self-cleanup must delete the orphaned DB directory"
    );
}

#[test]
fn remove_orphaned_db_dir_handles_already_gone() {
    // The self-cleanup runs concurrently with `remove_repo`'s own delete
    // loop; the loop may win the race and delete the dir first, so by the
    // time the detached task's guard calls `remove_orphaned_db_dir` the
    // path is already gone. That must not panic or surface a spurious
    // error — it is a no-op debug-log path.
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join(DB_DIR_NAME).join("never-existed");
    assert!(!db_path.exists());

    // Must not panic; the already-gone path stays gone.
    ServeState::remove_orphaned_db_dir("orphan", &db_path);
    assert!(!db_path.exists());
}

/// End-to-end regression for PR #179: when `remove_repo` lands while a
/// `build_index` is still inside its uninterruptible `spawn_blocking` phase,
/// the orphaned `.codesearch.db` dir must still end up deleted — by the build
/// task's post-build self-cleanup guard (`remove_orphaned_db_dir`), which runs
/// after the blocking work returns. Unlike
/// `await_index_task_cancels_and_joins_indexing_task` (which plants a
/// cooperatively-cancellable async yield-loop) and unlike the
/// `remove_orphaned_db_dir_*` unit tests (which call the guard directly), this
/// plants a `spawn_blocking`-based task and drives the full `remove_repo` path
/// while that blocking work is still in flight.
#[tokio::test]
async fn remove_repo_during_active_build_self_cleans_db_dir() {
    let (_tmp, repo_path, state) = state_with_repo("buildrepo");
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();
    // Seed a file so the dir is non-empty and delete is real work.
    std::fs::write(db_path.join("data.mdb"), "fake").unwrap();

    // Plant an indexing task that mimics a real build_index: an uninterruptible
    // `spawn_blocking` phase (tokio cannot cancel it mid-sleep via the token),
    // followed by the PR #179 post-build self-cleanup guard.
    let token = CancellationToken::new();
    let db_path_for_cleanup = db_path.clone();
    let handle = tokio::spawn(async move {
        // build_index's synchronous arroy HNSW build runs on the blocking pool
        // and has no cancellation point tokio can interrupt.
        let _ = tokio::task::spawn_blocking(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
        })
        .await;
        // Post-build guard: the alias was removed mid-build, so the dir is
        // orphaned — self-clean it now that the build's handles are released.
        ServeState::remove_orphaned_db_dir("buildrepo", &db_path_for_cleanup);
    });
    state
        .index_tasks
        .insert("buildrepo".to_string(), (handle, token));

    // remove_repo lands WHILE the spawn_blocking build is still sleeping.
    let outcome = state
        .remove_repo("buildrepo")
        .await
        .expect("remove_repo should succeed");
    // Safety margin in case await_index_task's bounded join detached the task.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        outcome.db_deleted,
        "remove_repo must report db_deleted=true; got error: {:?}",
        outcome.db_delete_error
    );
    assert!(
        !db_path.exists(),
        "orphaned .codesearch.db dir must be self-cleaned after a mid-build remove"
    );
}

#[test]
#[allow(clippy::io_other_error)] // synthetic errors with literal messages
fn is_db_locked_error_classifies_lock_and_non_lock_errors() {
    // `remove_repo`'s deadline-bounded delete retry only retries lock-class
    // errors (Windows sharing/lock violation / access-denied, or a message
    // hinting the dir is in use). A permanent failure — e.g. a NotFound, or
    // the dir actually being a regular file — must NOT be retried, so it
    // surfaces immediately instead of burning the retry budget.
    use std::io;

    // Permanent: NotFound (dir already gone) -> not a lock.
    assert!(!ServeState::is_db_locked_error(&io::Error::from(
        io::ErrorKind::NotFound
    )));
    // Lock-class: Windows ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION
    // (33) raw codes -> retried (raw_os_error is platform-independent here).
    assert!(ServeState::is_db_locked_error(
        &io::Error::from_raw_os_error(32)
    ));
    assert!(ServeState::is_db_locked_error(
        &io::Error::from_raw_os_error(33)
    ));
    // Lock-class by message hint (cross-platform fallback).
    assert!(ServeState::is_db_locked_error(&io::Error::new(
        io::ErrorKind::Other,
        "The process cannot access the file because it is being used by another process"
    )));
    // Permanent: a non-lock message -> not retried.
    assert!(!ServeState::is_db_locked_error(&io::Error::new(
        io::ErrorKind::Other,
        "not a directory"
    )));
}

/// Open a real registered LMDB env at `path` — the same holder shape
/// `remove_repo`'s lock-class retry waits on (a live `TrackedEnv` keeps the
/// mmap file handles on Windows and its registry slot everywhere).
fn open_test_lmdb_env(
    path: &std::path::Path,
    description: &str,
) -> crate::lmdb_registry::TrackedEnv {
    let mut opts = heed::EnvOpenOptions::new();
    opts.map_size(1024 * 1024).max_dbs(1);
    unsafe { opts.flags(crate::lmdb_registry::BASE_ENV_FLAGS) };
    unsafe { crate::lmdb_registry::TrackedEnv::open(&opts, path, description).unwrap() }
}

/// `await_lmdb_release` returns empty once the last in-process holder drops,
/// and does NOT return early while the holder is alive. A spawned dropper
/// releases the env after 200 ms; the helper (deadline 5 s) must observe the
/// drain. The elapsed lower bound only rules out an instant-return bug —
/// it cannot flake, since the env provably lived that long.
#[tokio::test]
async fn await_lmdb_release_drains_after_holder_drops() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();

    let env = open_test_lmdb_env(&db_path, "transient-search-holder");
    let held_from = std::time::Instant::now();
    // Drop the env from a spawned task after a short delay — mimics an
    // in-flight search finishing and dropping its Arc<SharedStores>.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        drop(env);
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let remaining = ServeState::await_lmdb_release(&db_path, deadline).await;

    assert!(
        remaining.is_empty(),
        "helper must report a full drain, still sees: {remaining:?}"
    );
    assert!(
        held_from.elapsed() >= std::time::Duration::from_millis(200),
        "helper returned before the holder actually dropped — early-return bug"
    );
}

/// The budget-expiry arm: a holder that never releases must not hang the
/// helper past its deadline — it returns the surviving holder descriptions so
/// `remove_repo` can log exactly who outlived the budget.
#[tokio::test]
async fn await_lmdb_release_returns_holders_at_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();

    let _env = open_test_lmdb_env(&db_path, "stuck-embed-pass");

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(150);
    // Outer bound so a regression to an unbounded loop fails the test fast
    // instead of hanging the suite.
    let remaining = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        ServeState::await_lmdb_release(&db_path, deadline),
    )
    .await
    .expect("helper must return at its deadline, not hang");

    assert_eq!(
        remaining,
        vec!["stuck-embed-pass".to_string()],
        "deadline expiry must carry the surviving holder's description"
    );
}

#[test]
fn is_alias_live_reflects_config_and_cancellation() {
    // FINDINGS #4: the resurrection guard. A detached indexing task must
    // NOT restart the FSW / rebuild the index for an alias that has been
    // removed. `is_alias_live` is the conjunction of "not cancelled" and
    // "alias still resolves in config"; the indexing tasks gate
    // build_index/restart_fsw on it. Here we lock all three states.
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("repo");
    std::fs::create_dir(&repo_path).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("repo".to_string()))
        .unwrap();
    let state = ServeState::new(config, None);

    let live = CancellationToken::new();
    let dead = CancellationToken::new();
    dead.cancel();

    // (a) registered + live token -> live
    assert!(
        state.is_alias_live("repo", &live),
        "registered alias with a live token must be live"
    );
    // (b) registered but token cancelled -> NOT live (cancellation wins)
    assert!(
        !state.is_alias_live("repo", &dead),
        "a cancelled token must make the alias not-live (resurrection guard)"
    );
    // (c) not registered + live token -> NOT live
    assert!(
        !state.is_alias_live("ghost", &live),
        "an unregistered alias must never be live"
    );
}

/// Regression guard: `GET /remotes` must NEVER expose a peer's `api_key`.
///
/// `RemotePeerInfo` is a dedicated projection struct with no `api_key`
/// field — serde cannot serialize a field that doesn't exist, so the
/// shared secret cannot leak even by accident. This test locks that
/// defense-in-depth: if a future change adds an `api_key` field to
/// `RemotePeerInfo` (or otherwise lets the key into the response shape),
/// this assertion fails.
#[test]
fn remote_peer_info_never_serializes_api_key() {
    use crate::db_discovery::repos::RemotePeer;

    // Build a peer carrying a real-looking secret, exactly as it lives in
    // repos.json, then project it the same way `remotes_handler` does.
    let peer = RemotePeer {
        url: "https://codesearch-serve.example.internal".to_string(),
        api_key: "supersecret-LEAK-MARKER-do-not-serialize".to_string(),
        group: Some("all".to_string()),
        timeout_secs: Some(90),
    };
    let info = RemotePeerInfo {
        alias: "cloud".to_string(),
        url: peer.url.clone(),
        group: peer.group.clone(),
        timeout_secs: peer.timeout_secs,
    };

    let json = serde_json::to_string(&info).expect("RemotePeerInfo must serialize");

    // The four whitelisted fields are present:
    assert!(json.contains("cloud"), "alias missing: {json}");
    assert!(
        json.contains("codesearch-serve.example.internal"),
        "url missing: {json}"
    );
    assert!(json.contains("all"), "group missing: {json}");
    assert!(json.contains("90"), "timeout_secs missing: {json}");

    // The secret is NOT present — neither the field name nor the value:
    assert!(
        !json.contains("api_key"),
        "api_key FIELD leaked into /remotes response shape: {json}"
    );
    assert!(
        !json.contains("supersecret-LEAK-MARKER"),
        "api_key VALUE leaked into /remotes response: {json}"
    );
}

fn state_with_config(config: ReposConfig) -> ServeState {
    // Use a temp file override so reload_if_changed doesn't see the real repos.json
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();
    ServeState::new(config, Some(config_file))
}

/// Common single-repo test scaffolding: a temp dir (kept alive for the test
/// lifetime — unlike `state_with_config`, which drops its `TempDir` on return),
/// a `repos.json` inside it, an empty repo dir at `<tmp>/<alias>`, a
/// `ReposConfig` with that repo registered under `alias`, and a `ServeState`
/// wired to the config file. Returns `(tmp, repo_path, state)`.
///
/// Callers that need to seed a `.codesearch.db` inside the repo do so from the
/// returned `repo_path` after this call.
fn state_with_repo(alias: &str) -> (tempfile::TempDir, std::path::PathBuf, ServeState) {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");
    let repo_path = tmp.path().join(alias);
    std::fs::create_dir(&repo_path).unwrap();
    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some(alias.to_string()))
        .unwrap();
    config.save_to(&config_file).unwrap();
    let state = ServeState::new(config, Some(config_file));
    (tmp, repo_path, state)
}

#[tokio::test]
async fn missing_db_not_cached_as_conflicted() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("myrepo");
    std::fs::create_dir(&repo_path).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
        .unwrap();

    let state = state_with_config(config);

    // First call: DB missing → error, NOT cached as Conflicted
    let err = match state.get_or_open_stores("testalias", true).await {
        Err(e) => e,
        Ok(_) => panic!("expected error for missing DB"),
    };
    assert!(
        err.contains("Database not found"),
        "expected 'not found', got: {}",
        err
    );
    assert!(!state.repos.contains_key("testalias"));

    // Recreate the DB directory + metadata so the next call succeeds.
    // Deliberately do NOT open SharedStores directly here: the reopen below
    // (get_or_open_stores → try_open_stores) creates the LMDB env itself
    // (proven by `try_open_stores_creates_db_for_brand_new_repo`). Opening
    // it directly first would open the same LMDB env twice in one process,
    // which the AGENTS.md LMDB rule forbids; on Linux the first env is not
    // always released before the reopen, making this test flaky. One open =
    // deterministic.
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir(&db_path).unwrap();
    let meta = db_path.join("metadata.json");
    let mut f = std::fs::File::create(&meta).unwrap();
    write!(f, "{{\"dimensions\":384}}").unwrap();
    drop(f);

    // Second call: should succeed without restart
    let res = state.get_or_open_stores("testalias", true).await;
    assert!(res.is_ok(), "expected ok after recreating DB, got: Err");
}

#[tokio::test]
async fn not_found_error_mentions_fix_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("myrepo");
    std::fs::create_dir(&repo_path).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
        .unwrap();

    let state = state_with_config(config);
    let err = match state.get_or_open_stores("testalias", true).await {
        Err(e) => e,
        Ok(_) => panic!("expected error for missing DB"),
    };
    assert!(
        err.contains("codesearch index add"),
        "error should mention 'index add': {}",
        err
    );
    assert!(
        err.contains("codesearch index rm"),
        "error should mention 'index rm': {}",
        err
    );
}

#[tokio::test]
async fn conflicted_error_mentions_stop_and_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("myrepo");
    std::fs::create_dir(&repo_path).unwrap();
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir(&db_path).unwrap();
    let meta = db_path.join("metadata.json");
    let mut f = std::fs::File::create(&meta).unwrap();
    write!(f, "{{\"dimensions\":384}}").unwrap();
    drop(f);

    // Open a write lock externally
    let _lock = SharedStores::new(&db_path, 384).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
        .unwrap();

    let state = state_with_config(config);
    let err = match state.get_or_open_stores("testalias", true).await {
        Err(e) => e,
        Ok(_) => panic!("expected conflict error"),
    };
    assert!(err.contains("Stop"), "error should mention 'Stop': {}", err);
    assert!(
        err.contains("retry"),
        "error should mention 'retry': {}",
        err
    );
}

/// A repo that failed to open because the DB was write-locked must recover on a
/// later query once that lock is gone — WITHOUT restarting serve.
///
/// Regression guard: `Conflicted` was cached in `self.repos` and the fast path in
/// `get_or_open_stores` replayed it forever. Its only documented exit was idle
/// eviction, which was unreachable — the reaper iterates `last_access`, but the
/// paths that mark a repo Conflicted return via `?` before ever calling
/// `touch_access`, so such a repo has no `last_access` entry and is never
/// considered for eviction however long it sits idle. Observed in the wild: a
/// repo left untouched for days was still returning the cached error, curable
/// only by restarting serve — while `conflicted_msg` claimed "the next query will
/// retry automatically".
#[tokio::test]
async fn conflicted_repo_recovers_after_lock_released() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("myrepo");
    std::fs::create_dir(&repo_path).unwrap();
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir(&db_path).unwrap();
    let meta = db_path.join("metadata.json");
    let mut f = std::fs::File::create(&meta).unwrap();
    write!(f, "{{\"dimensions\":384}}").unwrap();
    drop(f);

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
        .unwrap();
    let state = state_with_config(config);

    // Hold the write lock so the first open genuinely conflicts.
    let lock = SharedStores::new(&db_path, 384).unwrap();

    // Control: without a real conflict here the recovery assertion below would
    // pass vacuously, so failure of the FIRST call is what gives the test teeth.
    assert!(
        state.get_or_open_stores("testalias", true).await.is_err(),
        "precondition: holding the write lock must make the first open fail"
    );

    // Second control: the failure must actually have been CACHED as Conflicted.
    // Without this the retry path under test is never exercised, and the test
    // would go green even if the fix were reverted.
    assert!(
        state
            .repos
            .get("testalias")
            .is_some_and(|e| matches!(e.value(), RepoState::Conflicted)),
        "precondition: the failed open must be cached as Conflicted"
    );

    // Release the lock — the underlying cause is now gone.
    drop(lock);

    // The next query must recover on its own. No restart, no idle timeout, and
    // notably no waiting: recovery must not depend on the repo going untouched.
    let res = state.get_or_open_stores("testalias", true).await;
    assert!(
        res.is_ok(),
        "conflicted repo must reopen once the lock is released, got: {:?}",
        res.err()
    );
}

// ------------------------------------------------------------------
// Central store-creation / register path — regression guards.
//
// This is the point that has silently broken multiple times: opening or
// creating a repo's database for a BRAND-NEW repo whose `.codesearch.db`
// directory does not exist yet. The failure mode was a misleading
// "Database is locked by another process" error -> HTTP 500 on POST /repos
// -> repos.json registration rolled back -> CLI fell back to a local
// duplicate index (control never handed to serve).
//
// RULE FOR THESE TESTS: never pre-create the `.codesearch.db` directory.
// Earlier tests masked this exact bug by creating it first. The create /
// register path must be exercised with the directory genuinely absent.
// ------------------------------------------------------------------

/// Core invariant: `try_open_stores(allow_create = true)` on a repo whose
/// database directory does not exist yet MUST create it and return a
/// writable handle — never a "locked"/open error. This is the single
/// assertion that directly catches the regression class.
#[tokio::test]
async fn try_open_stores_creates_db_for_brand_new_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("brandnew");
    std::fs::create_dir(&repo_path).unwrap();
    let db_path = repo_path.join(DB_DIR_NAME);
    assert!(
        !db_path.exists(),
        "test precondition violated: db dir must NOT be pre-created"
    );

    let state = state_with_config(ReposConfig::default());

    match state.try_open_stores("brandnew", &db_path, true, false) {
        Ok(OpenedStores::Write(_)) => {}
        Ok(OpenedStores::Readonly(_)) => {
            panic!("brand-new repo opened Readonly; expected Write")
        }
        Err(e) => {
            panic!("opening stores for a brand-new repo (allow_create=true) must succeed, got: {e}")
        }
    }

    assert!(
        db_path.exists(),
        "the .codesearch.db directory should have been created"
    );
}

/// End-to-end guard for the exact symptom pair: `POST /repos` for a repo
/// whose database does not exist yet must return 202 Accepted, persist the
/// alias to repos.json, and register the repo in WRITE mode — it must NOT
/// return 500 and roll back the registration.
///
/// Determinism: `#[tokio::test]` uses a current-thread runtime, so the
/// background reindex task spawned by the handler cannot preempt this test
/// (no `.await` follows the handler call). All assertions observe the
/// handler's synchronous pre-spawn state — no embedding model required, no
/// race. `persist_config` honors the temp config override, so the real
/// `~/.codesearch/repos.json` is never touched.
#[tokio::test]
async fn add_repo_handler_registers_brand_new_repo_without_rollback() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("brandnew");
    std::fs::create_dir(&repo_path).unwrap();
    let db_path = repo_path.join(DB_DIR_NAME);
    assert!(!db_path.exists(), "precondition: db dir must not exist yet");

    let state = Arc::new(state_with_config(ReposConfig::default()));

    let (status, body) = add_repo_handler(
        axum::extract::State(state.clone()),
        axum::extract::Json(AddRepoRequest {
            path: repo_path.clone(),
            alias: Some("brandnew".to_string()),
            model: None,
        }),
    )
    .await;

    assert_eq!(
        status,
        axum::http::StatusCode::ACCEPTED,
        "brand-new repo register must be accepted (not 500), got {}: {}",
        status,
        body.0
    );

    // Registration persisted, NOT rolled back.
    assert!(
        state.config_snapshot().repos.contains_key("brandnew"),
        "alias must remain in repos.json after register (no rollback)"
    );

    // Registered in memory as Write so the fast-path avoids a second open.
    assert_eq!(
        state.repo_lock_status("brandnew"),
        Some("write"),
        "repo should be registered as Write immediately after add"
    );

    assert!(
        db_path.exists(),
        "the .codesearch.db directory should have been created"
    );
}

/// `persist_config` must write to the override path (and therefore be
/// observable by `reload_if_changed`/`config_snapshot`) rather than the real
/// `~/.codesearch/repos.json`. Guards the wiring that makes the register
/// path hermetically testable.
#[test]
fn persist_config_honors_override_path() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");
    let repo_path = tmp.path().join("somerepo");
    std::fs::create_dir(&repo_path).unwrap();

    ReposConfig::default().save_to(&config_file).unwrap();
    let state = ServeState::new(ReposConfig::default(), Some(config_file.clone()));

    {
        let mut cfg = state.config.write().unwrap();
        cfg.register_with_alias(repo_path.clone(), Some("somerepo".to_string()))
            .unwrap();
        state.persist_config(&cfg).unwrap();
    }

    // The override file on disk must contain the alias.
    let on_disk = ReposConfig::load_from(&config_file).unwrap();
    assert!(
        on_disk.repos.contains_key("somerepo"),
        "persist_config must write to the override path"
    );
}

#[test]
fn config_reload_picks_up_new_alias() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");

    let repo_a = tmp.path().join("repo-a");
    std::fs::create_dir(&repo_a).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_a.clone(), Some("a".to_string()))
        .unwrap();
    config.save_to(&config_file).unwrap();

    let state = ServeState::new(config, Some(config_file.clone()));
    assert_eq!(state.aliases(), vec!["a"]);

    // Add a new alias directly to the file
    let repo_b = tmp.path().join("repo-b");
    std::fs::create_dir(&repo_b).unwrap();
    let mut config2 = ReposConfig::load_from(&config_file).unwrap();
    config2
        .register_with_alias(repo_b, Some("b".to_string()))
        .unwrap();

    // Small sleep to ensure mtime changes on Windows
    std::thread::sleep(std::time::Duration::from_millis(150));
    config2.save_to(&config_file).unwrap();

    // Next query should pick it up
    let aliases = state.aliases();
    assert!(aliases.contains(&"a".to_string()));
    assert!(aliases.contains(&"b".to_string()));
}

#[tokio::test]
async fn config_reload_drops_removed_alias() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");

    let repo_path = tmp.path().join("myrepo");
    std::fs::create_dir(&repo_path).unwrap();
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir(&db_path).unwrap();
    let meta = db_path.join("metadata.json");
    let mut f = std::fs::File::create(&meta).unwrap();
    write!(f, "{{\"dimensions\":384}}").unwrap();
    drop(f);
    let _stores = SharedStores::new(&db_path, 384).unwrap();
    drop(_stores);

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("x".to_string()))
        .unwrap();
    config.save_to(&config_file).unwrap();

    let state = ServeState::new(config, Some(config_file.clone()));
    // Open alias x so it lands in DashMap
    let _ = state.get_or_open_stores("x", true).await.unwrap();
    assert!(state.repos.contains_key("x"));

    // Rewrite config without x
    let config2 = ReposConfig::default();

    // Small sleep to ensure mtime changes on Windows
    std::thread::sleep(std::time::Duration::from_millis(150));
    config2.save_to(&config_file).unwrap();

    // Next query for x should fail as unknown
    let err = match state.get_or_open_stores("x", true).await {
        Err(e) => e,
        Ok(_) => panic!("expected unknown alias after removal"),
    };
    assert!(
        err.contains("Unknown alias"),
        "expected unknown alias, got: {}",
        err
    );
    assert!(!state.repos.contains_key("x"));
}

#[test]
fn config_reload_no_spurious_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");

    let repo_path = tmp.path().join("myrepo");
    std::fs::create_dir(&repo_path).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path, Some("a".to_string()))
        .unwrap();
    config.save_to(&config_file).unwrap();

    let state = ServeState::new(config, Some(config_file.clone()));
    let initial = state.reload_count.load(std::sync::atomic::Ordering::SeqCst);

    // First call triggers reload (mtime was None)
    let _ = state.aliases();
    let after_first = state.reload_count.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(after_first, initial + 1);

    // Second call without file change should NOT reload
    let _ = state.aliases();
    let after_second = state.reload_count.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(after_second, after_first);
}

/// Verify that the /repos/:alias/reindex route is registered and reachable.
/// This test starts a real axum server on a random port and sends a POST request.
#[tokio::test]
async fn reindex_route_is_registered() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("myrepo");
    std::fs::create_dir(&repo_path).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
        .unwrap();

    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();

    let state = Arc::new(ServeState::new(config, Some(config_file)));

    let app = axum::Router::new()
        .route(
            crate::constants::HEALTH_PATH,
            axum::routing::get(health_handler),
        )
        .route(
            "/repos/:alias/reindex",
            axum::routing::post(reindex_handler),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give the server a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = reqwest::Client::new();

    // POST to unknown alias → 404 from our handler (not axum's built-in 404)
    let resp = client
        .post(format!("http://{}/repos/unknown/reindex", addr))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "expected 404 from our handler"
    );
    let body: serde_json::Value = resp
        .json()
        .await
        .expect("handler should return JSON body for 404");
    assert!(
        body.get("error").is_some(),
        "expected JSON error body, got: {}",
        body
    );

    // POST to known alias → 202 Accepted or 500 (DB missing), but NOT axum's built-in 404
    // The key assertion is that the route IS registered (we get our handler's response, not axum's empty 404)
    let resp = client
        .post(format!("http://{}/repos/testalias/reindex", addr))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.expect("handler should return JSON body");
    assert!(
        status == reqwest::StatusCode::ACCEPTED
            || status == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "expected 202 or 500 from our handler (not axum's 404), got {}: {}",
        status,
        body
    );
    assert!(
        body.get("status").is_some(),
        "expected JSON with 'status' field, got: {}",
        body
    );
}

/// `repo_read_only=true` must refuse a reindex on the one route that can undo
/// it — even with `?force=true`. This is the cloud-peer OOM-avoidance
/// invariant: the lightweight serve replica must never rebuild the heavy DOCS
/// corpus index it only holds read-only. The handler returns 409 CONFLICT with
/// `status: "read_only"` (see the read-only guard in `reindex_handler`,
/// src/serve/mod.rs).
#[tokio::test]
async fn reindex_refused_for_read_only_repo_even_with_force() {
    let (_tmp, _repo_path, state) = state_with_repo("readonlyrepo");
    // Mark the repo read-only in the live config (how a snapshot-restore sets it).
    state
        .config
        .write()
        .unwrap()
        .repo_read_only
        .insert("readonlyrepo".to_string(), true);

    let state = Arc::new(state);
    let app = axum::Router::new()
        .route(
            crate::constants::HEALTH_PATH,
            axum::routing::get(health_handler),
        )
        .route(
            "/repos/:alias/reindex",
            axum::routing::post(reindex_handler),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    // Even force=true must be refused for a read-only repo.
    let resp = client
        .post(format!(
            "http://{}/repos/readonlyrepo/reindex?force=true",
            addr
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::CONFLICT,
        "force-reindex on a read-only repo must be refused with 409"
    );
    let body: serde_json::Value = resp.json().await.expect("handler returns JSON");
    assert_eq!(
        body.get("status").and_then(|v| v.as_str()),
        Some("read_only"),
        "expected status=read_only, got: {body}"
    );
}

/// `/healthz` is exempt from `require_auth_for_network`: reachable without a
/// key even on a (simulated) network bind, while `/health` stays protected.
#[tokio::test]
async fn healthz_is_unauthenticated_on_network_bind() {
    let network_auth = NetworkAuthConfig {
        is_network_bind: true,
        api_key: Some("secret-key".to_string()),
    };

    let app = axum::Router::new()
        .route(HEALTH_PATH, axum::routing::get(health_handler))
        .route(HEALTHZ_PATH, axum::routing::get(healthz_handler))
        .layer(axum::middleware::from_fn(require_auth_for_network))
        .layer(axum::Extension(network_auth));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = reqwest::Client::new();

    // /healthz reachable WITHOUT a key on a network bind.
    let resp = client
        .get(format!("http://{}/healthz", addr))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "/healthz must be public on a network bind"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("status").and_then(|v| v.as_str()),
        Some("ok"),
        "/healthz body must be {{\"status\":\"ok\"}}"
    );

    // /health stays protected on a network bind (401 without a key).
    let resp = client
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "/health must still require auth on a network bind"
    );
}

/// Verify that the /repos/:alias/info and /repos/:alias/doctor routes are
/// registered and reachable. Starts a real axum server on a random port and
/// asserts that an unknown alias yields our handler's 404 (not axum's 404).
#[tokio::test]
async fn info_doctor_routes_registered() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("myrepo");
    std::fs::create_dir(&repo_path).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
        .unwrap();

    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();

    let state = Arc::new(ServeState::new(config, Some(config_file)));

    let app = axum::Router::new()
        .route(
            crate::constants::HEALTH_PATH,
            axum::routing::get(health_handler),
        )
        .route("/repos/:alias/info", axum::routing::get(info_handler))
        .route("/repos/:alias/doctor", axum::routing::post(doctor_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give the server a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = reqwest::Client::new();

    // GET unknown alias info → 404 from our handler (not axum's built-in 404)
    let resp = client
        .get(format!("http://{}/repos/unknown/info", addr))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "expected 404 from info handler"
    );
    let body: serde_json::Value = resp
        .json()
        .await
        .expect("info handler should return JSON body for 404");
    assert!(
        body.get("error").is_some(),
        "expected JSON error body from info handler, got: {}",
        body
    );

    // GET a registered alias's info → 200, and the body must carry "path"
    // (the peer's on-disk index directory) so a TUI client's
    // `#[serde(default)] path: String` field has something to deserialize
    // rather than silently falling back to an empty string forever.
    let resp = client
        .get(format!("http://{}/repos/testalias/info", addr))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "expected 200 from info handler for a registered alias"
    );
    let body: serde_json::Value = resp
        .json()
        .await
        .expect("info handler should return JSON body for a registered alias");
    let path = body
        .get("path")
        .and_then(|v| v.as_str())
        .expect("info handler response must carry a \"path\" key");
    assert!(
        path.ends_with(crate::constants::DB_DIR_NAME),
        "expected path to end with {}, got: {}",
        crate::constants::DB_DIR_NAME,
        path
    );

    // POST unknown alias doctor → 404 from our handler (not axum's built-in 404)
    let resp = client
        .post(format!("http://{}/repos/unknown/doctor", addr))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "expected 404 from doctor handler"
    );
    let body: serde_json::Value = resp
        .json()
        .await
        .expect("doctor handler should return JSON body for 404");
    assert!(
        body.get("error").is_some(),
        "expected JSON error body from doctor handler, got: {}",
        body
    );
}

/// Verify that the federation REST endpoints (/search, /find, /explore,
/// /chunk/:id) are registered and reachable. Each must dispatch to OUR
/// handler (returning a JSON body) rather than axum's built-in empty 404.
/// Starts a real axum server on a random port.
#[tokio::test]
async fn rest_routes_are_registered() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("myrepo");
    std::fs::create_dir(&repo_path).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
        .unwrap();

    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();

    let state = Arc::new(ServeState::new(config, Some(config_file)));

    let app = axum::Router::new()
        .route(
            crate::constants::HEALTH_PATH,
            axum::routing::get(health_handler),
        )
        .route(
            crate::constants::SEARCH_PATH,
            axum::routing::post(crate::mcp::rest_search_handler),
        )
        .route(
            crate::constants::FIND_PATH,
            axum::routing::post(crate::mcp::rest_find_handler),
        )
        .route(
            crate::constants::EXPLORE_PATH,
            axum::routing::post(crate::mcp::rest_explore_handler),
        )
        .route(
            crate::constants::CHUNK_PATH,
            axum::routing::get(crate::mcp::rest_get_chunk_handler),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = reqwest::Client::new();

    // Helper: a response from OUR handler is either 200 (success) or 500
    // (McpError mapped), but ALWAYS a parseable JSON body — never axum's
    // built-in empty 404. The repo has no index, so the tools return
    // error/scope JSON; we only assert the route + handler are wired.
    async fn assert_our_handler(client: &reqwest::Client, url: String) -> serde_json::Value {
        let resp = client.get(&url).send().await.unwrap();
        // GET endpoints: must reach our handler (JSON body), status 200/500.
        assert!(
            resp.status() == reqwest::StatusCode::OK
                || resp.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "GET {} -> unexpected status {} (route not registered?)",
            url,
            resp.status()
        );
        resp.json().await.unwrap_or_else(|e| {
            panic!(
                "GET {} did not return a JSON body from our handler: {}",
                url, e
            )
        })
    }

    // POST /search — dispatches to rest_search_handler.
    let resp = client
        .post(format!("http://{}/search", addr))
        .json(&serde_json::json!({"query": "foo", "project": "testalias"}))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == reqwest::StatusCode::OK
            || resp.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "POST /search -> unexpected status {} (route not registered?)",
        resp.status()
    );
    let _body: serde_json::Value = resp
        .json()
        .await
        .expect("POST /search should return JSON from our handler, not axum's 404");

    // POST /find — dispatches to rest_find_handler.
    let resp = client
        .post(format!("http://{}/find", addr))
        .json(&serde_json::json!({"kind": "definition", "symbol": "foo", "project": "testalias"}))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == reqwest::StatusCode::OK
            || resp.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "POST /find -> unexpected status {} (route not registered?)",
        resp.status()
    );
    let _: serde_json::Value = resp
        .json()
        .await
        .expect("POST /find should return JSON from our handler");

    // POST /explore — dispatches to rest_explore_handler.
    let resp = client
        .post(format!("http://{}/explore", addr))
        .json(&serde_json::json!({"kind": "outline", "target": "somefile", "project": "testalias"}))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == reqwest::StatusCode::OK
            || resp.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "POST /explore -> unexpected status {} (route not registered?)",
        resp.status()
    );
    let _: serde_json::Value = resp
        .json()
        .await
        .expect("POST /explore should return JSON from our handler");

    // GET /chunk/1 — dispatches to rest_get_chunk_handler.
    let _ = assert_our_handler(
        &client,
        format!("http://{}/chunk/1?project=testalias", addr),
    )
    .await;
}

#[test]
fn config_reload_tolerates_parse_error() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");

    let repo_path = tmp.path().join("myrepo");
    std::fs::create_dir(&repo_path).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("a".to_string()))
        .unwrap();
    config.save_to(&config_file).unwrap();

    let state = ServeState::new(config, Some(config_file.clone()));
    assert!(state.aliases().contains(&"a".to_string()));

    // Overwrite with garbage
    std::fs::write(&config_file, "not-json-at-all").unwrap();

    // Should not panic; old config still usable
    let aliases = state.aliases();
    assert!(aliases.contains(&"a".to_string()));
}

/// Verify that concurrent reindex requests for the same alias return 409 Conflict.
#[tokio::test]
async fn concurrent_reindex_returns_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("myrepo");
    std::fs::create_dir(&repo_path).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
        .unwrap();

    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();

    let state = Arc::new(ServeState::new(config, Some(config_file)));

    let app = axum::Router::new()
        .route(
            "/repos/:alias/reindex",
            axum::routing::post(reindex_handler),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = reqwest::Client::new();

    // First request: 202 Accepted (or 500 if DB missing) — but NOT 409
    let resp1 = client
        .post(format!("http://{}/repos/testalias/reindex", addr))
        .send()
        .await
        .unwrap();
    let status1 = resp1.status();
    assert!(
        status1 == reqwest::StatusCode::ACCEPTED
            || status1 == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "first request should be 202 or 500, got {}",
        status1
    );

    // If the first request was accepted (202), the reindex is running in background.
    // Send a second request immediately — should get 409 Conflict.
    if status1 == reqwest::StatusCode::ACCEPTED {
        let resp2 = client
            .post(format!("http://{}/repos/testalias/reindex", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp2.status(),
            reqwest::StatusCode::CONFLICT,
            "second concurrent request should be 409 Conflict"
        );
        let body: serde_json::Value = resp2.json().await.unwrap();
        assert_eq!(body["status"], "conflict");
    }
}

/// Unit tests for `validate_path_within_allowed_roots`.
///
/// These tests temporarily set/remove the `CODESEARCH_ALLOWED_ROOTS` env var.
/// A static Mutex serializes env mutation to prevent races under parallel test execution.
#[cfg(test)]
mod allowed_roots_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Global lock to serialize env var mutations across parallel test threads.
    static ENV_LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    /// Helper: create a unique temp dir per test, return its canonical path.
    fn temp_root(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("codesearch_test_roots_{}", suffix));
        let _ = std::fs::create_dir_all(&dir);
        safe_canonicalize(&dir).unwrap()
    }

    fn clear_env() {
        std::env::remove_var(ALLOWED_ROOTS_ENV);
    }

    fn set_env(val: &str) {
        std::env::set_var(ALLOWED_ROOTS_ENV, val);
    }

    #[test]
    fn env_unset_allows_all() {
        let _guard = lock();
        clear_env();
        let path = PathBuf::from("/some/random/path");
        assert!(validate_path_within_allowed_roots(&path).is_ok());
    }

    #[test]
    fn env_empty_allows_all() {
        let _guard = lock();
        set_env("");
        let path = PathBuf::from("/some/random/path");
        assert!(validate_path_within_allowed_roots(&path).is_ok());
        clear_env();
    }

    #[test]
    fn path_within_root_is_allowed() {
        let _guard = lock();
        let root = temp_root("within");
        set_env(&root.display().to_string());
        let child = root.join("my-project");
        let _ = std::fs::create_dir_all(&child);
        let canonical_child = safe_canonicalize(&child).unwrap();
        assert!(validate_path_within_allowed_roots(&canonical_child).is_ok());
        clear_env();
    }

    #[test]
    fn exact_root_match_is_allowed() {
        let _guard = lock();
        let root = temp_root("exact");
        set_env(&root.display().to_string());
        assert!(validate_path_within_allowed_roots(&root).is_ok());
        clear_env();
    }

    #[test]
    fn path_outside_root_is_rejected() {
        let _guard = lock();
        let root = temp_root("outside");
        set_env(&root.display().to_string());
        // Construct a path guaranteed outside the temp root
        let outside = if cfg!(windows) {
            PathBuf::from("C:\\Windows\\System32")
        } else {
            PathBuf::from("/etc")
        };
        assert!(
            !outside.starts_with(&root),
            "Test setup error: outside path '{}' must not overlap root '{}'",
            outside.display(),
            root.display()
        );
        let result = validate_path_within_allowed_roots(&outside);
        assert!(result.is_err(), "Expected rejection for path outside root");
        assert!(result.unwrap_err().contains("outside allowed roots"));
        clear_env();
    }

    #[test]
    fn all_nonexistent_roots_rejects() {
        let _guard = lock();
        set_env("/nonexistent/path/abc;/also/nonexistent/xyz");
        let some_path = std::env::temp_dir();
        let canonical = safe_canonicalize(&some_path).unwrap();
        let result = validate_path_within_allowed_roots(&canonical);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No valid roots found"));
        clear_env();
    }

    #[test]
    fn semicolons_with_empty_segments_works() {
        let _guard = lock();
        let root = temp_root("semicolons");
        set_env(&format!(";{};;", root.display()));
        let child = root.join("project");
        let _ = std::fs::create_dir_all(&child);
        let canonical_child = safe_canonicalize(&child).unwrap();
        assert!(validate_path_within_allowed_roots(&canonical_child).is_ok());
        clear_env();
    }

    #[test]
    fn multiple_roots_any_match() {
        let _guard = lock();
        let root1 = temp_root("multi1");
        let root2 = temp_root("multi2");

        set_env(&format!("{};{}", root1.display(), root2.display()));

        // Path under root1
        let child1 = root1.join("project");
        let _ = std::fs::create_dir_all(&child1);
        let canonical1 = safe_canonicalize(&child1).unwrap();
        assert!(validate_path_within_allowed_roots(&canonical1).is_ok());

        // Path under root2
        let child2 = root2.join("project");
        let _ = std::fs::create_dir_all(&child2);
        let canonical2 = safe_canonicalize(&child2).unwrap();
        assert!(validate_path_within_allowed_roots(&canonical2).is_ok());

        clear_env();
    }
}

/// The reserved virtual "all" group must resolve to every registered alias
/// via the serve-layer entry point used by MCP tools (issue #131).
#[test]
fn resolve_group_aliases_all_returns_every_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_a = tmp.path().join("repo-a");
    let repo_b = tmp.path().join("repo-b");
    std::fs::create_dir(&repo_a).unwrap();
    std::fs::create_dir(&repo_b).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_a, Some("alpha".to_string()))
        .unwrap();
    config
        .register_with_alias(repo_b, Some("beta".to_string()))
        .unwrap();

    let state = state_with_config(config);

    let aliases = state
        .resolve_group_aliases(crate::constants::ALL_GROUP_NAME)
        .expect("'all' should resolve");
    assert_eq!(aliases, vec!["alpha".to_string(), "beta".to_string()]);

    // "all" is never stored — an unknown real group still errors.
    assert!(state.resolve_group_aliases("does-not-exist").is_err());
}

/// Tests for `build_streamable_http_config` — DNS rebinding defence env vars
/// (`CODESEARCH_ALLOWED_HOSTS`, `CODESEARCH_DISABLE_HOST_VALIDATION`) added
/// for issue #149 / GHSA-89vp-x53w-74fx.
mod allowed_hosts_tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize env var mutations across parallel test threads (same pattern
    /// as `allowed_roots_tests`). Different env vars from `allowed_roots_tests`
    /// so cross-module parallelism is safe.
    static ENV_LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn clear_env() {
        std::env::remove_var(ALLOWED_HOSTS_ENV);
        std::env::remove_var(DISABLE_HOST_VALIDATION_ENV);
    }

    #[test]
    fn default_is_loopback_only() {
        let _guard = lock();
        clear_env();
        let config = build_streamable_http_config();
        assert_eq!(
            config.allowed_hosts,
            vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "::1".to_string(),
            ]
        );
    }

    #[test]
    fn custom_allowed_hosts_replaces_default() {
        let _guard = lock();
        clear_env();
        std::env::set_var(ALLOWED_HOSTS_ENV, "codesearch.internal, codesearch:39725");
        let config = build_streamable_http_config();
        assert_eq!(
            config.allowed_hosts,
            vec![
                "codesearch.internal".to_string(),
                "codesearch:39725".to_string(),
            ]
        );
    }

    #[test]
    fn disable_validation_clears_allowlist() {
        let _guard = lock();
        clear_env();
        std::env::set_var(DISABLE_HOST_VALIDATION_ENV, "1");
        let config = build_streamable_http_config();
        assert!(
            config.allowed_hosts.is_empty(),
            "disable_allowed_hosts() should produce an empty allowlist"
        );
    }

    #[test]
    fn disable_validation_accepts_true_case_insensitive() {
        let _guard = lock();
        clear_env();
        std::env::set_var(DISABLE_HOST_VALIDATION_ENV, "TRUE");
        let config = build_streamable_http_config();
        assert!(config.allowed_hosts.is_empty());
    }

    #[test]
    fn disable_validation_ignores_other_values() {
        let _guard = lock();
        clear_env();
        std::env::set_var(DISABLE_HOST_VALIDATION_ENV, "yes");
        let config = build_streamable_http_config();
        // Not "1" or "true" → rmcp default applies.
        assert_eq!(config.allowed_hosts.len(), 3);
    }

    #[test]
    fn empty_allowed_hosts_falls_back_to_default() {
        let _guard = lock();
        clear_env();
        std::env::set_var(ALLOWED_HOSTS_ENV, "  ,  ,  ");
        let config = build_streamable_http_config();
        assert_eq!(
            config.allowed_hosts,
            vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "::1".to_string(),
            ],
            "all-empty entries should leave the rmcp default intact"
        );
    }

    #[test]
    fn disable_overrides_allowed_hosts() {
        let _guard = lock();
        clear_env();
        std::env::set_var(ALLOWED_HOSTS_ENV, "codesearch.internal");
        std::env::set_var(DISABLE_HOST_VALIDATION_ENV, "true");
        let config = build_streamable_http_config();
        assert!(
            config.allowed_hosts.is_empty(),
            "DISABLE_HOST_VALIDATION takes precedence over ALLOWED_HOSTS"
        );
    }
}

/// Tests for `extract_host_from_url` — used solely by the keep-warm
/// misconfiguration sanity check (a keep-warm target host that doesn't look
/// like "self" gets a loud warning; see the diagnosis this shipped with in
/// docs/diagnose-federated-keep-warm.md).
mod keep_warm_host_extraction_tests {
    use super::*;

    #[test]
    fn extracts_host_from_plain_http_url() {
        assert_eq!(
            extract_host_from_url("http://127.0.0.1:8080/healthz"),
            Some("127.0.0.1".to_string())
        );
    }

    #[test]
    fn extracts_host_from_https_url_without_port() {
        assert_eq!(
            extract_host_from_url("https://happywave-063747be.azurecontainerapps.io/healthz"),
            Some("happywave-063747be.azurecontainerapps.io".to_string())
        );
    }

    #[test]
    fn extracts_host_with_no_scheme() {
        // The keep-warm URL is user-supplied (CLI flag or env var) and never
        // validated to include a scheme — must not panic or silently return
        // the whole string including a path.
        assert_eq!(
            extract_host_from_url("localhost:39725/healthz"),
            Some("localhost".to_string())
        );
    }

    #[test]
    fn extracts_ipv6_host_preserving_brackets() {
        // A bare rsplit_once(':') would wrongly split inside the IPv6
        // literal itself (e.g. on the last `:` in `::1`) if not guarded.
        assert_eq!(
            extract_host_from_url("http://[::1]:8080/healthz"),
            Some("[::1]".to_string())
        );
    }

    #[test]
    fn strips_query_and_fragment_before_host_ends() {
        assert_eq!(
            extract_host_from_url("http://example.com/healthz?x=1#frag"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn returns_none_for_empty_host() {
        assert_eq!(extract_host_from_url("http:///healthz"), None);
    }
}

/// The keep-warm "target isn't self" warning must fire on a genuine
/// misconfiguration and stay silent on the cloud deployment where keep-warm is
/// actually supposed to run. Getting the latter wrong is worse than having no
/// check at all: a warning that fires on every correct cold start trains
/// operators to ignore it.
#[cfg(test)]
mod keep_warm_foreign_target_tests {
    use super::*;

    /// The regression this rule exists for: on Azure Container Apps the process
    /// binds `0.0.0.0` while the keep-warm target is correctly the ingress
    /// FQDN. A naive host comparison flags that as "not self" and warns on
    /// every cold start of the only correct deployment.
    #[test]
    fn wildcard_bind_never_warns_even_for_a_foreign_looking_fqdn() {
        for wildcard in ["0.0.0.0", "::", "[::]", "0:0:0:0:0:0:0:0", ""] {
            assert_eq!(
                keep_warm_foreign_target(
                    "https://codesearch-serve.azurecontainerapps.io",
                    wildcard
                ),
                None,
                "wildcard bind {wildcard:?} must not warn — our external host is unknown"
            );
        }
    }

    /// The case the check exists to catch: a concretely-bound local serve whose
    /// keep-warm URL points at somebody else's cloud replica.
    #[test]
    fn concrete_bind_warns_for_a_different_host() {
        assert_eq!(
            keep_warm_foreign_target("https://peer.example.com/healthz", "192.168.1.10"),
            Some("peer.example.com".to_string())
        );
    }

    #[test]
    fn matching_host_does_not_warn() {
        assert_eq!(
            keep_warm_foreign_target("http://192.168.1.10:39725/healthz", "192.168.1.10"),
            None
        );
    }

    #[test]
    fn loopback_targets_are_always_treated_as_self() {
        for target in [
            "http://localhost:39725/healthz",
            "http://127.0.0.1:39725/healthz",
            "http://[::1]:39725/healthz",
        ] {
            assert_eq!(
                keep_warm_foreign_target(target, "192.168.1.10"),
                None,
                "{target} is loopback and must not warn"
            );
        }
    }

    /// No extractable host → nothing to compare → no warning.
    #[test]
    fn unparseable_target_does_not_warn() {
        assert_eq!(
            keep_warm_foreign_target("http:///healthz", "192.168.1.10"),
            None
        );
    }
}

// ===========================================================================
// GET /indexing — freshness probe (grep-guard wait-and-retry, todo #55)
// ===========================================================================

/// Helper: a repos map from (alias, root) pairs.
fn repos_map(entries: &[(&str, &str)]) -> std::collections::HashMap<String, std::path::PathBuf> {
    entries
        .iter()
        .map(|(a, p)| (a.to_string(), std::path::PathBuf::from(p)))
        .collect()
}

#[test]
fn containing_repo_alias_matches_subdir_but_not_sibling_prefix() {
    let repos = repos_map(&[("alpha", "/base/alpha"), ("beta", "/base/beta")]);

    // Exact root.
    assert_eq!(
        containing_repo_alias(&repos, Path::new("/base/alpha")),
        Some("alpha".to_string())
    );
    // File inside the repo.
    assert_eq!(
        containing_repo_alias(&repos, Path::new("/base/alpha/src/main.rs")),
        Some("alpha".to_string())
    );
    // Component boundary: /base/alpha-x is NOT inside /base/alpha.
    assert_eq!(
        containing_repo_alias(&repos, Path::new("/base/alpha-x/file.rs")),
        None,
        "string-prefix sibling must not match"
    );
    // Entirely outside.
    assert_eq!(containing_repo_alias(&repos, Path::new("/elsewhere")), None);
}

#[test]
fn containing_repo_alias_prefers_nested_repo() {
    // Two registered repos, one nested inside the other's tree: the inner
    // (longer root) must win so the freshness answer is about the repo the
    // path actually belongs to.
    let repos = repos_map(&[("outer", "/base"), ("inner", "/base/inner")]);
    assert_eq!(
        containing_repo_alias(&repos, Path::new("/base/inner/src/a.rs")),
        Some("inner".to_string())
    );
    assert_eq!(
        containing_repo_alias(&repos, Path::new("/base/other/src/b.rs")),
        Some("outer".to_string())
    );
}

#[test]
fn containing_repo_alias_case_insensitive_only_on_windows() {
    let repos = repos_map(&[("alpha", "/Base/Alpha")]);
    let hit = containing_repo_alias(&repos, Path::new("/base/alpha/x.rs"));
    if cfg!(windows) {
        assert_eq!(hit, Some("alpha".to_string()));
    } else {
        assert_eq!(hit, None, "unix path matching stays case-sensitive");
    }
}

#[test]
fn freshness_for_path_reports_indexing_state() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("repo");

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("fresh".to_string()))
        .unwrap();

    let state = Arc::new(ServeState::new(config, None));
    let target = repo_path.join("src").join("lib.rs");

    // Idle: covered, not indexing.
    let (alias, indexing) = state.freshness_for_path(&target.to_string_lossy());
    assert_eq!(alias.as_deref(), Some("fresh"));
    assert!(!indexing);

    // Mid-reindex (the exact state a branch-switch full refresh puts the
    // repo in — make_indexing_status_callback inserts around it).
    state.begin_indexing("fresh");
    let (_, indexing) = state.freshness_for_path(&target.to_string_lossy());
    assert!(indexing, "begin_indexing must surface as indexing=true");

    state.end_indexing("fresh");
    let (_, indexing) = state.freshness_for_path(&target.to_string_lossy());
    assert!(!indexing);

    // Unknown path: not covered, no crash.
    let (alias, indexing) = state.freshness_for_path("/definitely/not/registered");
    assert_eq!(alias, None);
    assert!(!indexing);
}

#[tokio::test]
async fn indexing_route_answers_json() {
    // Route + handler wiring: a GET with a covered path returns our JSON
    // (never axum's empty 404), with the covered/indexing fields present.
    let tmp = tempfile::tempdir().unwrap();
    let raw_repo = tmp.path().join("hooked");
    std::fs::create_dir(&raw_repo).unwrap();
    // CANONICALIZE (same trap as remove_order_tests' make_proj): on the
    // Windows CI runner the temp root sits under an 8.3 short name
    // (RUNNER~1) that only canonicalize resolves, and register canonicalizes
    // before storing — querying with the raw path made covered=false there
    // (green locally, red on CI).
    let repo_path = crate::cache::safe_canonicalize(&raw_repo).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("hooked".to_string()))
        .unwrap();

    let state = Arc::new(ServeState::new(config, None));

    let app = axum::Router::new()
        .route(
            crate::constants::INDEXING_PATH,
            axum::routing::get(indexing_handler),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    // Temp paths are safe ASCII; normalise backslashes so the query string is
    // legal as-is (component matching treats / and \ identically on Windows).
    let covered = repo_path.to_string_lossy().replace('\\', "/");

    // Covered path.
    let resp = client
        .get(format!(
            "http://{addr}/indexing?path={covered}",
            addr = addr
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["covered"], serde_json::json!(true));
    assert_eq!(body["alias"], serde_json::json!("hooked"));
    assert_eq!(body["indexing"], serde_json::json!(false));

    // Uncovered path.
    let resp = client
        .get(format!(
            "http://{addr}/indexing?path=/nowhere/at/all",
            addr = addr
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["covered"], serde_json::json!(false));
    assert_eq!(body["indexing"], serde_json::json!(false));
}

// ===========================================================================
// `codesearch index rm` → running serve: end-to-end delegation (todo #48 L2)
// ===========================================================================

/// The full Layer-2 acceptance path: a running serve instance holds the
/// repo's registration; `remove_from_index` (the CLI code path) must
/// DELEGATE to it (health probe → DELETE /repos/:alias), serve must stop
/// holders and delete the DB directory WITHOUT being stopped, repos.json
/// must lose the entry, and a later query for the alias must be a clean
/// "Unknown alias" (no zombie stores) — all without stopping serve.
///
/// Hermetic per the repo env-var rule: `#[serial]` + `EnvRestore` pins
/// `CODESEARCH_REPOS_CONFIG` (temp repos.json shared by CLI and serve) and
/// `CODESEARCH_SERVE_PORT` (the spawned test serve), so the delegation
/// probe can never reach a developer's real serve or registry.
#[tokio::test]
#[serial_test::serial]
async fn index_rm_delegates_to_running_serve_end_to_end() {
    use crate::testing::EnvRestore;

    let tmp = tempfile::tempdir().unwrap();
    let raw_proj = tmp.path().join("e2eproj");
    std::fs::create_dir(&raw_proj).unwrap();
    let proj = crate::cache::safe_canonicalize(&raw_proj).unwrap();
    // A real (empty) DB directory — stand-in for the LMDB dir; deleting it
    // exercises the same remove_dir_all path without opening a real env.
    std::fs::create_dir(proj.join(".codesearch.db")).unwrap();

    // Env vars FIRST, then seed repos.json (same ordering trap the
    // remove_order_tests document: saving before the override writes the
    // developer's real registry).
    let cfg_path = tmp.path().join("repos.json");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    // NOTE: local_addr() is a SocketAddr — take .port() or the env var and
    // URLs below silently become "127.0.0.1:127.0.0.1:<port>" and the port
    // parse falls back to the DEFAULT port (39725), pointing the delegation
    // at a developer's REAL serve. Exactly the failure this test hunted down.
    let port = listener.local_addr().unwrap().port();
    // SERVE_HOST_ENV too: `try_delegate_rm_to_serve` resolves the host via
    // `resolve_serve_host()` — without pinning it to loopback, a machine with
    // CODESEARCH_SERVE_HOST set (or a future default change) sends the probe
    // somewhere else entirely.
    let _env = EnvRestore::set(&[
        (
            crate::constants::REPOS_CONFIG_ENV,
            &cfg_path.to_string_lossy(),
        ),
        (crate::constants::SERVE_PORT_ENV, port.to_string().as_str()),
        (crate::constants::SERVE_HOST_ENV, "127.0.0.1"),
    ]);
    let mut cfg = ReposConfig::default();
    cfg.register(proj.clone());
    // Capture the alias the registration derived (directory-name based) so
    // the post-removal probe below addresses exactly what was registered.
    let alias = cfg
        .repos
        .iter()
        .find(|(_, p)| *p == &proj)
        .map(|(a, _)| a.clone())
        .expect("seeded config must contain the proj");
    cfg.save().expect("seed repos.json save must succeed");
    assert!(
        cfg_path.exists(),
        "seed repos.json must land in the temp path, not the global default"
    );

    // A running serve instance sharing the SAME repos.json. Routes are the
    // two the CLI delegation touches: GET /health and the real
    // remove_repo_handler (DELETE /repos/:alias).
    let state = Arc::new(ServeState::new(cfg, Some(cfg_path.clone())));
    let app = axum::Router::new()
        .route(
            crate::constants::HEALTH_PATH,
            axum::routing::get(health_handler),
        )
        .route("/repos/:alias", axum::routing::delete(remove_repo_handler))
        .with_state(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // Bounded readiness wait (same pattern as federation's spawn_test_server).
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // The CLI path — must delegate (serve is reachable) and succeed.
    crate::index::remove_from_index(Some(proj.clone()), false)
        .await
        .expect("delegated index rm must succeed");

    // DB directory gone WITHOUT stopping serve.
    assert!(
        !proj.join(".codesearch.db").exists(),
        "serve must delete the DB dir during delegation — no serve stop needed"
    );
    // repos.json (the shared temp file) lost the entry.
    let after = ReposConfig::load().expect("repos.json reload after rm");
    assert!(
        !after.repos.values().any(|p| p == &proj),
        "entry must be unregistered from repos.json, still has: {:?}",
        after.repos.values().collect::<Vec<_>>()
    );

    // Later queries for the alias: a clean "Unknown alias", not a zombie
    // store resurrecting the repo (todo #48 L2 acceptance criterion 3).
    let err = state
        .remove_repo(&alias)
        .await
        .expect_err("removing an unregistered alias must fail");
    assert!(
        err.to_string().contains("Unknown alias"),
        "expected a clean Unknown-alias error, got: {err:#}"
    );
}

/// The hard variant of the Layer-2 acceptance ("file-delete succeeds without
/// serve-stop"): the running serve does not merely KNOW the repo — it holds a
/// REAL open LMDB environment on the `.codesearch.db` directory (a Warm
/// `RepoState`, exactly what a live query leaves behind). `remove_from_index`
/// must still delete the directory in one shot via delegation, without
/// stopping serve, and the eviction must actually close the env — pinned by
/// `lmdb_registry::open_holders_under` going from non-empty to empty across
/// the removal. That registry drain is the Windows precondition for the
/// file-delete to succeed at all: while the TrackedEnv slot is live the
/// mmap'd data/lock files cannot be removed, so asserting the drain asserts
/// the mechanism the locked-delete path depends on, cross-platform.
///
/// The store is opened via the production `try_open_stores` path (creating
/// the DB dir for real), and the returned `Arc<SharedStores>` is MOVED into
/// `RepoState::Warm` with no clone kept — the test process must not itself
/// be the extra holder that defeats the delete.
#[tokio::test]
#[serial_test::serial]
async fn index_rm_deletes_db_while_serve_holds_real_lmdb_env() {
    use crate::testing::EnvRestore;

    let tmp = tempfile::tempdir().unwrap();
    let raw_proj = tmp.path().join("heldenv");
    std::fs::create_dir(&raw_proj).unwrap();
    let proj = crate::cache::safe_canonicalize(&raw_proj).unwrap();
    let db_path = proj.join(DB_DIR_NAME);
    // No pre-created DB dir: try_open_stores must create it, proving the
    // env we then hold is a real production-shaped store.

    let cfg_path = tmp.path().join("repos.json");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let _env = EnvRestore::set(&[
        (
            crate::constants::REPOS_CONFIG_ENV,
            &cfg_path.to_string_lossy(),
        ),
        (crate::constants::SERVE_PORT_ENV, port.to_string().as_str()),
        (crate::constants::SERVE_HOST_ENV, "127.0.0.1"),
    ]);

    let mut cfg = ReposConfig::default();
    cfg.register_with_alias(proj.clone(), Some("heldenv".to_string()))
        .unwrap();
    cfg.save().expect("seed repos.json save must succeed");
    assert!(
        cfg_path.exists(),
        "seed repos.json must land in the temp path, not the global default"
    );

    let state = Arc::new(ServeState::new(cfg, Some(cfg_path.clone())));

    // Serve opens the repo FOR REAL — a live LMDB env under db_path.
    let opened = state
        .try_open_stores("heldenv", &db_path, true, false)
        .expect("opening a real store for a brand-new repo must succeed");
    let OpenedStores::Write(stores) = opened else {
        panic!("brand-new repo must open Write, not Readonly");
    };
    // Move the Arc in; keep NO clone (a test-side clone would be exactly the
    // transient holder class remove_repo's retry has to out-wait).
    state
        .repos
        .insert("heldenv".to_string(), RepoState::Warm { stores });

    // Precondition: the env is genuinely held — this is what makes the
    // delete impossible on Windows until serve's eviction releases it.
    assert!(
        !crate::lmdb_registry::open_holders_under(&db_path).is_empty(),
        "test precondition: a real LMDB holder must be live under the db dir"
    );

    // The serve side: real handler routes, same as a production bind.
    let app = axum::Router::new()
        .route(
            crate::constants::HEALTH_PATH,
            axum::routing::get(health_handler),
        )
        .route("/repos/:alias", axum::routing::delete(remove_repo_handler))
        .with_state(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // The CLI path — delegated removal against a serve that HOLDS the env.
    crate::index::remove_from_index(Some(proj.clone()), false)
        .await
        .expect("delegated index rm must succeed while serve holds the env");

    // The DB directory is gone — deleted BY SERVE, in-place, serve still up.
    assert!(
        !db_path.exists(),
        "serve must delete the really-held DB dir without being stopped"
    );
    // The eviction genuinely dropped the RepoState (and with it the last
    // Arc<SharedStores>). NOTE: a registry query (`open_holders_under`) is
    // VACUOUS here — the dir is deleted, canonicalize fails, and the helper
    // by design answers "no holders" for a missing path even though a zombie
    // env would still be alive on it (mutation-verified: skipping
    // repos.remove left the test green through that assert on Linux). The
    // repos-map assert is the non-vacuous pin: the Warm entry must be gone.
    // On Windows a skipped eviction additionally fails the dir-gone assert
    // (the mmap'd files refuse deletion while held).
    assert!(
        !state.repos.contains_key("heldenv"),
        "eviction must drop the RepoState — a surviving Warm entry is a zombie holder"
    );
    // repos.json lost the entry.
    let after = ReposConfig::load().expect("repos.json reload after rm");
    assert!(
        !after.repos.values().any(|p| p == &proj),
        "entry must be unregistered from repos.json, still has: {:?}",
        after.repos.values().collect::<Vec<_>>()
    );
    // No zombie: the alias no longer resolves for later queries.
    let err = state
        .remove_repo("heldenv")
        .await
        .expect_err("removing an unregistered alias must fail");
    assert!(
        err.to_string().contains("Unknown alias"),
        "expected a clean Unknown-alias error, got: {err:#}"
    );
}
