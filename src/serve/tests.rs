use super::*;
use serial_test::serial;
use std::io::Write;

#[tokio::test]
async fn try_open_stores_attaches_the_serve_job_gate() {
    // Serve-opened stores must carry the same process-wide gate as the
    // ServeState, so cold opens, warmup and FSW refreshes all draw from one
    // heavy-job budget (CODESEARCH_INDEX_JOBS).
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("repo").join(DB_DIR_NAME);
    let state = std::sync::Arc::new(ServeState::new(ReposConfig::default(), None));

    let stores = match state
        .try_open_stores("repo", &db_path, true, false, None)
        .expect("a fresh repo must open in write mode")
    {
        OpenedStores::Write(s) => s,
        OpenedStores::Readonly(_) => panic!("allow_create=true must open write mode"),
    };

    let store_gate = stores
        .job_gate
        .as_ref()
        .expect("serve-opened stores must carry the job gate");
    assert!(
        std::sync::Arc::ptr_eq(store_gate, &state.job_gate),
        "the store gate must be the ServeState gate"
    );
}

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

#[tokio::test]
#[serial]
async fn get_chunk_routes_mounted_remote_projects_through_federation() {
    // todo #153: `get_chunk(project="<peer>/<alias>", chunk_id=…)` must route
    // through the federated fetch exactly like search's project-level
    // federation, instead of dying in local routing with "Unknown alias".
    // The peer URL here is unreachable, so the correctly routed answer is the
    // federation failure message — "Unknown alias" means the routing did not
    // happen.
    let mut config = ReposConfig::default();
    config.remotes.insert(
        "cloud".to_string(),
        crate::db_discovery::repos::RemotePeer {
            url: "http://127.0.0.1:1".to_string(),
            api_key: "test-key".to_string(),
            group: None,
            timeout_secs: None,
        },
    );
    config.remote_mounts.push("cloud/bynder".to_string());
    // Hermetic config: persist to a temp file and pass the override, so
    // `reload_if_changed` reads THIS config — not the developer's real
    // ~/.codesearch/repos.json (which would leak real peers into the test).
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();
    let state = std::sync::Arc::new(ServeState::new(config, Some(config_file)));
    let service = crate::mcp::CodesearchService::new_for_serve(state).unwrap();

    let _env =
        crate::testing::EnvRestore::set(&[(crate::constants::REMOTE_PEER_RETRY_BACKOFF_ENV, "1")]);
    let req = crate::mcp::types::GetChunkRequest {
        chunk_id: 2058,
        chunk_ref: None,
        context_lines: None,
        project: Some("cloud/bynder".to_string()),
        group: None,
    };
    let res = service
        .get_chunk(rmcp::handler::server::wrapper::Parameters(req))
        .await
        .expect("handler must not error");
    let text = match res.content.first() {
        Some(rmcp::model::ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(
        text.contains("Could not fetch chunk from remote peer 'cloud'"),
        "get_chunk must route mounted remote projects to the peer, got: {text}"
    );
    assert!(
        !text.contains("Unknown alias"),
        "a mounted remote project is not a local alias — routing failed: {text}"
    );
}

#[test]
fn idle_mcp_sessions_are_bounded_to_reap_after_30_minutes() {
    // The idle-reap fix is exactly this configuration: keep_alive=None
    // assumed TCP liveness would collect wedged sessions, but a stuck
    // request keeps its socket and its FIFO-serialized worker alive
    // indefinitely (observed active_sessions=4 with a single client).
    // The bound must stay a plain half hour — far beyond any human pause in
    // local interactive use, yet a guarantee the session eventually goes
    // away without a serve restart.
    let manager = crate::serve::session_manager_with_idle_reap();
    assert_eq!(
        manager.session_config.keep_alive,
        Some(std::time::Duration::from_secs(crate::constants::MCP_IDLE_SESSION_SECS)),
        "the reap bound must be configured on the rmcp session manager"
    );
    assert_eq!(
        crate::constants::MCP_IDLE_SESSION_SECS,
        30 * 60,
        "the reap bound must stay at thirty minutes"
    );
}

#[test]
fn shutdown_drain_notice_bounds_the_pause_and_forbids_the_reflex_ctrl_c() {
    // The notice exists because the ~3 s drain after `q` reads as a hang
    // from the restored terminal, and the reflex second Ctrl-C hard-kills
    // serve mid-drain (raw mode is already off, so it IS delivered). Every
    // element matters: the bounded wait, the session count, and the
    // explicit "exits by itself".
    let quiet = crate::serve::tui::shutdown_drain_notice(0);
    assert_eq!(quiet, "🛑 Shutting down…", "no sessions — no drain to explain");

    let draining = crate::serve::tui::shutdown_drain_notice(4);
    assert!(
        draining.contains("4 open MCP session(s)"),
        "the count of draining sessions must be named, got: {draining}"
    );
    assert!(
        draining.contains("~3 s"),
        "the visibly bounded wait is the whole point, got: {draining}"
    );
    assert!(
        draining.contains("exiting by itself"),
        "the user must be told NOT to intervene, got: {draining}"
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
    state.index_tasks.insert(
        "repo-x".to_string(),
        IndexTask {
            handle,
            token,
            db_path: std::path::PathBuf::from("/nonexistent-db"),
            started_at: Instant::now(),
        },
    );
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

#[tokio::test]
async fn remove_repo_quarantines_db_dir_when_delete_budget_expires() {
    // When the delete budget expires (external holder: another process, an
    // AV scanner), the directory used to stay in place as a "valid" index —
    // the next registration adopted its stale metadata and chunk stores as a
    // false readiness, and re-registration against that legacy data ended in
    // arroy EINVAL (the pilot's bare-stamp repos). The quarantine rename
    // makes a fresh registration build from scratch; the quarantined sibling
    // stays for manual cleanup, same convention as .codesearch.db.bak-*.
    let (_tmp, repo_path, state) = state_with_repo("ghostrepo");
    // db_path is a FILE: remove_dir_all fails every retry with a non-lock
    // error, deterministically exhausting the budget branch on every OS.
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::write(&db_path, "not a directory").unwrap();

    let outcome = state
        .remove_repo("ghostrepo")
        .await
        .expect("remove_repo returns Ok(outcome); delete failure is non-fatal");

    assert!(!outcome.db_deleted);
    let quarantine = outcome
        .db_quarantined
        .expect("a surviving db path must be quarantined, not left in place");
    assert!(!db_path.exists(), "the original db path must be gone");
    assert!(quarantine.exists(), "the quarantined sibling must exist");
    assert!(
        quarantine
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".codesearch.db.removed-"),
        "quarantine follows the <DB_DIR_NAME>.removed-* convention: {}",
        quarantine.display()
    );
}

#[tokio::test]
async fn delete_repo_http_mapping_reports_a_quarantined_db_honestly() {
    // The DELETE /repos/{alias} payload must not read "removed" when the DB
    // dir survived the delete budget: the caller (an operator or an agent
    // driving /repos) decides whether the disk still holds a stale index by
    // `status` and `db_quarantined`. The quarantine outcome itself is pinned
    // by remove_repo_quarantines_db_dir_when_delete_budget_expires; this
    // pins its HTTP mapping.
    let (_tmp, repo_path, state) = state_with_repo("qmap");
    let state = Arc::new(state);
    // A FILE as the db path: remove_dir_all fails every retry with a
    // non-lock error, deterministically exhausting the budget into the
    // quarantine branch on every OS.
    std::fs::write(repo_path.join(DB_DIR_NAME), "not a directory").unwrap();

    let (status, body) =
        crate::serve::remove_repo_handler(axum::extract::Path("qmap".to_string()), axum::extract::State(state))
            .await;

    assert_eq!(status, axum::http::StatusCode::OK, "got body: {body:?}");
    assert_eq!(
        body["status"], "removed_db_quarantined",
        "the status must name the honest outcome, got: {body:?}"
    );
    assert_eq!(
        body["db_deleted"], false,
        "a quarantined dir was NOT deleted, got: {body:?}"
    );
    let quarantine = body["db_quarantined"]
        .as_str()
        .expect("db_quarantined must be a string path")
        .to_string();
    assert!(
        quarantine.contains(&format!("{DB_DIR_NAME}.removed-")),
        "the payload must point at the quarantined sibling, got: {quarantine}"
    );
}

#[test]
fn sweep_removes_quarantine_remnants_and_leaves_everything_else() {
    // The quarantine rename leaves the survivor "for manual cleanup", and
    // manual cleanup never happens — every budget-expired delete leaked a
    // full stale index copy forever. The startup sweep is the cleanup:
    // by serve start the previous session's external holders are gone, so
    // the tombstone has no readers left to protect.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    let remnant = repo.join(format!("{DB_DIR_NAME}.removed-1700000000000"));
    std::fs::create_dir_all(&remnant).unwrap();
    std::fs::write(remnant.join("metadata.json"), "{}").unwrap();
    let live_db = repo.join(DB_DIR_NAME);
    std::fs::create_dir_all(&live_db).unwrap();
    // Similar name, different prefix family — must not be touched.
    let other = repo.join("notes.removed-manual");
    std::fs::create_dir_all(&other).unwrap();

    crate::serve::sweep_quarantined_db_remnants(std::slice::from_ref(&repo));

    assert!(
        !remnant.exists(),
        "the .codesearch.db.removed-* sibling must be swept"
    );
    assert!(live_db.exists(), "the live db dir must stay untouched");
    assert!(other.exists(), "unrelated .removed names must not be touched");

    // A repo path that cannot be listed warns but does not panic.
    let missing = tmp.path().join("gone");
    crate::serve::sweep_quarantined_db_remnants(&[missing]);
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
    state.index_tasks.insert(
        "buildrepo".to_string(),
        IndexTask {
            handle,
            token,
            db_path: db_path.clone(),
            started_at: Instant::now(),
        },
    );

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

/// Pin for the cold-open single-flight (todo #131): a second opener must PARK
/// on the per-alias open lock instead of racing into `try_open_stores` and
/// tripping the LMDB double-open guard. Deterministic: every step before the
/// lock acquire is synchronous, so one yield lets the spawned task reach the
/// lock await; on revert (no single-flight) the whole call completes on that
/// same poll and the `!is_finished()` assertion fails.
#[tokio::test]
async fn cold_open_parks_second_opener_behind_alias_lock() {
    let (_tmp, _repo_path, state) = state_with_repo("testalias");
    let state = std::sync::Arc::new(state);

    // Hold the alias open lock like an in-flight cold open would.
    let alias_lock = state.open_lock("testalias");
    let guard = alias_lock.lock().await;

    let s2 = std::sync::Arc::clone(&state);
    let task = tokio::spawn(async move { s2.get_or_open_stores("testalias", true).await });

    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "second opener must park on the alias open lock, not run (and fail) concurrently"
    );

    drop(guard);
    let res = task.await.unwrap();
    // No DB seeded → the parked opener resumes and fails with the ordinary
    // missing-DB error; what matters is that it ran to completion AFTER the
    // lock was released, never concurrently.
    assert!(
        res.is_err(),
        "expected the ordinary missing-DB error after the lock was released"
    );
}

/// Invariant: N concurrent cold opens of the same repo must all succeed and
/// share ONE stores Arc (single open, everyone else hits the cache re-check).
/// Pre-single-flight this race could produce the LMDB double-open error on
/// the losers; with it, exactly one opener reaches `try_open_stores`.
#[tokio::test]
async fn concurrent_cold_opens_share_one_stores_arc() {
    let (_tmp, repo_path, state) = state_with_repo("testalias");
    // Seed an openable DB (same recipe as missing_db_not_cached_as_conflicted).
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir(&db_path).unwrap();
    let mut f = std::fs::File::create(db_path.join("metadata.json")).unwrap();
    write!(f, "{{\"dimensions\":384}}").unwrap();
    drop(f);

    let state = std::sync::Arc::new(state);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let s = std::sync::Arc::clone(&state);
        handles.push(tokio::spawn(async move {
            s.get_or_open_stores("testalias", true).await
        }));
    }

    let mut first: Option<std::sync::Arc<SharedStores>> = None;
    for h in handles {
        let stores = h
            .await
            .unwrap()
            .expect("concurrent cold open must not fail (double-open guard)");
        match &first {
            None => first = Some(stores),
            Some(fst) => assert!(
                std::sync::Arc::ptr_eq(fst, &stores),
                "concurrent openers must share one stores Arc"
            ),
        }
    }
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

    match state.try_open_stores("brandnew", &db_path, true, false, None) {
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
///
/// `#[serial]` + env reset: the handler reads `CODESEARCH_ALLOWED_ROOTS` via
/// `validate_path_within_allowed_roots`, so this test must not run while the
/// `allowed_roots_tests` below are mutating it (and must not inherit a stale
/// value from ambient state).
#[serial]
#[tokio::test]
async fn add_repo_handler_registers_brand_new_repo_without_rollback() {
    let _env = crate::testing::EnvRestore::remove(&[ALLOWED_ROOTS_ENV]);
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

/// `POST /repos` with no explicit `model` must create a brand-new index at the
/// serve-wide default's dimension (`codesearch serve --model X`), not the
/// built-in 384-dim default. This is the write-side counterpart of the per-repo
/// query-model contract.
#[tokio::test]
async fn add_repo_handler_uses_serve_default_model_for_new_index() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("defaulted");
    std::fs::create_dir(&repo_path).unwrap();

    let state = Arc::new(
        state_with_config(ReposConfig::default())
            .with_default_model(Some(crate::embed::ModelType::EmbeddingGemma300MQ4)),
    );

    let (status, body) = add_repo_handler(
        axum::extract::State(state.clone()),
        axum::extract::Json(AddRepoRequest {
            path: repo_path.clone(),
            alias: Some("defaulted".to_string()),
            model: None,
        }),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::ACCEPTED,
        "add must be accepted, got {}: {}",
        status,
        body.0
    );

    let stores = state
        .get_opened_stores("defaulted")
        .expect("store must be open immediately after add");
    let dims = stores
        .vector_store
        .try_read()
        .unwrap()
        .stats()
        .unwrap()
        .dimensions;
    assert_eq!(
        dims,
        crate::embed::ModelType::EmbeddingGemma300MQ4.dimensions(),
        "a new index must be created at the serve default model's dimension"
    );
}

/// The serve-wide default must NOT override an index that already records its
/// own model: re-adding a repo whose `.codesearch.db` is still on disk keeps the
/// recorded model and dimension.
#[tokio::test]
async fn add_repo_handler_keeps_recorded_model_over_serve_default() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("existing");
    std::fs::create_dir(&repo_path).unwrap();
    let db_path = repo_path.join(DB_DIR_NAME);

    // A pre-existing index recording the 384-dim default model.
    std::fs::create_dir_all(&db_path).unwrap();
    let mut meta = serde_json::Map::new();
    crate::embed::ModelType::AllMiniLML6V2Q.write_metadata_fields(&mut meta);
    std::fs::write(
        db_path.join("metadata.json"),
        serde_json::to_string(&meta).unwrap(),
    )
    .unwrap();

    let state = Arc::new(
        state_with_config(ReposConfig::default())
            .with_default_model(Some(crate::embed::ModelType::EmbeddingGemma300MQ4)),
    );

    let (status, body) = add_repo_handler(
        axum::extract::State(state.clone()),
        axum::extract::Json(AddRepoRequest {
            path: repo_path.clone(),
            alias: Some("existing".to_string()),
            model: None,
        }),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::ACCEPTED,
        "add must be accepted, got {}: {}",
        status,
        body.0
    );

    let stores = state
        .get_opened_stores("existing")
        .expect("store must be open immediately after add");
    let dims = stores
        .vector_store
        .try_read()
        .unwrap()
        .stats()
        .unwrap()
        .dimensions;
    assert_eq!(
        dims,
        crate::embed::ModelType::AllMiniLML6V2Q.dimensions(),
        "an existing index must keep its recorded model, not adopt the serve default"
    );
}

/// Precedence contract for the model a `POST /repos` add indexes with.
#[test]
fn resolve_add_repo_model_precedence() {
    use crate::embed::ModelType;
    let gemma = ModelType::EmbeddingGemma300MQ4;
    let mini = ModelType::AllMiniLML6V2Q;

    // Explicit model always wins, even over a recorded model and a default.
    assert_eq!(
        resolve_add_repo_model(Some(gemma), Some(mini), Some(mini)),
        Some(gemma)
    );
    // No explicit model, no recorded model → serve default applies (new index).
    assert_eq!(resolve_add_repo_model(None, None, Some(gemma)), Some(gemma));
    // No explicit model, recorded model present → serve default is ignored.
    assert_eq!(resolve_add_repo_model(None, Some(mini), Some(gemma)), None);
    // No explicit model, no recorded model, no default → no override.
    assert_eq!(resolve_add_repo_model(None, None, None), None);
    // Explicit model still wins when nothing else is set.
    assert_eq!(resolve_add_repo_model(Some(mini), None, None), Some(mini));
}

/// The serve-wide default is the scope-free fallback model in serve mode (the
/// unpinned status summary, a call with no routed alias). It is deliberately
/// NOT the query fallback for a repo that records no model — see
/// `unrecorded_index_is_queried_with_builtin_default_not_serve_default`.
#[test]
fn serve_default_model_is_service_fallback() {
    use crate::embed::ModelType;
    let state = std::sync::Arc::new(
        ServeState::new(ReposConfig::default(), None)
            .with_default_model(Some(ModelType::EmbeddingGemma300MQ4)),
    );
    assert_eq!(state.default_model(), Some(ModelType::EmbeddingGemma300MQ4));

    let svc = crate::mcp::CodesearchService::new_for_serve(state).unwrap();
    assert_eq!(
        svc.query_model(None),
        ModelType::EmbeddingGemma300MQ4,
        "serve must fall back to its default model, not the built-in default"
    );
}

/// Without `--model`, `ServeState` reports no default and the service falls back
/// to the built-in default.
#[test]
fn no_serve_default_keeps_builtin_fallback() {
    use crate::embed::ModelType;
    let state = std::sync::Arc::new(ServeState::new(ReposConfig::default(), None));
    assert_eq!(state.default_model(), None);

    let svc = crate::mcp::CodesearchService::new_for_serve(state).unwrap();
    assert_eq!(svc.query_model(None), ModelType::default());
}

/// A repo whose `metadata.json` records no model is queried with the BUILT-IN
/// default, never the serve-wide `--model` default.
///
/// Regression guard for `serve --model X` silently overriding a legacy index:
/// with a 768-dim serve default, a 384-dim legacy index failed every search with
/// "Query embedding dimension mismatch: expected 384, got 768", and a
/// same-dimension default would have compared incomparable vector spaces without
/// erroring. The assumption must also reach the caller as a warning naming the
/// repo, the assumed model and the re-index command.
#[test]
fn unrecorded_index_is_queried_with_builtin_default_not_serve_default() {
    use crate::embed::ModelType;

    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");
    let repo_path = tmp.path().join("legacy");
    std::fs::create_dir(&repo_path).unwrap();
    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("legacy".to_string()))
        .unwrap();
    config.save_to(&config_file).unwrap();

    let state = std::sync::Arc::new(
        ServeState::new(config, Some(config_file))
            .with_default_model(Some(ModelType::EmbeddingGemma300MQ4)),
    );
    let svc = crate::mcp::CodesearchService::new_for_serve(state).unwrap();

    // No metadata.json yet: an unrecorded model. Serve default is gemma.
    let resolution = svc.resolve_query_model(Some("legacy"));
    assert_eq!(
        resolution.model,
        ModelType::default(),
        "a repo that records no model must be queried with the built-in default, \
         not the '{}' serve default",
        ModelType::EmbeddingGemma300MQ4.short_name()
    );
    let warning = resolution
        .assumed_warning
        .expect("the assumed model must be surfaced to the caller");
    assert!(
        warning.contains("legacy"),
        "warning must name the repo: {warning}"
    );
    assert!(
        warning.contains(ModelType::default().short_name()),
        "warning must name the assumed model: {warning}"
    );
    assert!(
        warning.contains("--force"),
        "warning must give the re-index command: {warning}"
    );

    // A recorded model is used as-is and must not warn.
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();
    std::fs::write(
        db_path.join("metadata.json"),
        r#"{"model_short_name":"embeddinggemma-q4","dimensions":768}"#,
    )
    .unwrap();
    let resolution = svc.resolve_query_model(Some("legacy"));
    assert_eq!(resolution.model, ModelType::EmbeddingGemma300MQ4);
    assert!(
        resolution.assumed_warning.is_none(),
        "a recorded model must not warn"
    );
}

/// The unrecorded-model log warning fires once per alias, so a busy hub does not
/// repeat the same line on every query. The caller-facing warning is separate
/// and is not deduped.
#[test]
fn legacy_model_warning_is_logged_once_per_alias() {
    let state = ServeState::new(ReposConfig::default(), None);
    assert!(state.mark_legacy_model_warned("a"));
    assert!(!state.mark_legacy_model_warned("a"));
    assert!(state.mark_legacy_model_warned("b"));
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

/// Verify that the /repos/{alias}/reindex route is registered and reachable.
/// This test starts a real axum server on a random port and sends a POST request.
///
/// `#[serial]` + env reset — same allowed-roots race guard as the add_repo
/// handler test above.
#[serial]
#[tokio::test]
async fn reindex_route_is_registered() {
    let _env = crate::testing::EnvRestore::remove(&[ALLOWED_ROOTS_ENV]);
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
            "/repos/{alias}/reindex",
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
#[serial]
#[tokio::test]
async fn reindex_refused_for_read_only_repo_even_with_force() {
    let _env = crate::testing::EnvRestore::remove(&[ALLOWED_ROOTS_ENV]);
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
            "/repos/{alias}/reindex",
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

/// Verify that the /repos/{alias}/info and /repos/{alias}/doctor routes are
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
        .route("/repos/{alias}/info", axum::routing::get(info_handler))
        .route("/repos/{alias}/doctor", axum::routing::post(doctor_handler))
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
/// /chunk/{id}) are registered and reachable. Each must dispatch to OUR
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
        .route(
            crate::constants::FIND_IMPACT_PATH,
            axum::routing::post(crate::mcp::rest_find_impact_handler),
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

    // POST /find-impact — dispatches to rest_find_impact_handler.
    let resp = client
        .post(format!("http://{}/find-impact", addr))
        .json(&serde_json::json!({"symbol_name": "foo", "project": "testalias"}))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == reqwest::StatusCode::OK
            || resp.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "POST /find-impact -> unexpected status {} (route not registered?)",
        resp.status()
    );
    let _: serde_json::Value = resp
        .json()
        .await
        .expect("POST /find-impact should return JSON from our handler");

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
            "/repos/{alias}/reindex",
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
/// These tests mutate the `CODESEARCH_ALLOWED_ROOTS` env var. Per the
/// AGENTS.md rule they are `#[serial]` and restore the var via `EnvRestore`:
/// a private Mutex cannot protect against non-serial tests elsewhere in the
/// process that READ the var through the real handlers (the add_repo
/// handler test below), which is exactly the 403 flake this closed.
#[cfg(test)]
mod allowed_roots_tests {
    use super::*;
    use crate::testing::EnvRestore;
    use serial_test::serial;
    use std::path::PathBuf;

    /// Helper: create a unique temp dir per test, return its canonical path.
    fn temp_root(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("codesearch_test_roots_{}", suffix));
        let _ = std::fs::create_dir_all(&dir);
        safe_canonicalize(&dir).unwrap()
    }

    #[serial]
    #[test]
    fn env_unset_allows_all() {
        let _env = EnvRestore::remove(&[ALLOWED_ROOTS_ENV]);
        let path = PathBuf::from("/some/random/path");
        assert!(validate_path_within_allowed_roots(&path).is_ok());
    }

    #[serial]
    #[test]
    fn env_empty_allows_all() {
        let _env = EnvRestore::set(&[(ALLOWED_ROOTS_ENV, "")]);
        let path = PathBuf::from("/some/random/path");
        assert!(validate_path_within_allowed_roots(&path).is_ok());
    }

    #[serial]
    #[test]
    fn path_within_root_is_allowed() {
        let _env = EnvRestore::remove(&[ALLOWED_ROOTS_ENV]);
        let root = temp_root("within");
        std::env::set_var(ALLOWED_ROOTS_ENV, root.display().to_string());
        let child = root.join("my-project");
        let _ = std::fs::create_dir_all(&child);
        let canonical_child = safe_canonicalize(&child).unwrap();
        assert!(validate_path_within_allowed_roots(&canonical_child).is_ok());
    }

    #[serial]
    #[test]
    fn exact_root_match_is_allowed() {
        let _env = EnvRestore::remove(&[ALLOWED_ROOTS_ENV]);
        let root = temp_root("exact");
        std::env::set_var(ALLOWED_ROOTS_ENV, root.display().to_string());
        assert!(validate_path_within_allowed_roots(&root).is_ok());
    }

    #[serial]
    #[test]
    fn path_outside_root_is_rejected() {
        let _env = EnvRestore::remove(&[ALLOWED_ROOTS_ENV]);
        let root = temp_root("outside");
        std::env::set_var(ALLOWED_ROOTS_ENV, root.display().to_string());
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
    }

    #[serial]
    #[test]
    fn all_nonexistent_roots_rejects() {
        let _env = EnvRestore::set(&[(
            ALLOWED_ROOTS_ENV,
            "/nonexistent/path/abc;/also/nonexistent/xyz",
        )]);
        let some_path = std::env::temp_dir();
        let canonical = safe_canonicalize(&some_path).unwrap();
        let result = validate_path_within_allowed_roots(&canonical);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No valid roots found"));
    }

    #[serial]
    #[test]
    fn semicolons_with_empty_segments_works() {
        let _env = EnvRestore::remove(&[ALLOWED_ROOTS_ENV]);
        let root = temp_root("semicolons");
        std::env::set_var(ALLOWED_ROOTS_ENV, format!(";{};;", root.display()));
        let child = root.join("project");
        let _ = std::fs::create_dir_all(&child);
        let canonical_child = safe_canonicalize(&child).unwrap();
        assert!(validate_path_within_allowed_roots(&canonical_child).is_ok());
    }

    #[serial]
    #[test]
    fn multiple_roots_any_match() {
        let _env = EnvRestore::remove(&[ALLOWED_ROOTS_ENV]);
        let root1 = temp_root("multi1");
        let root2 = temp_root("multi2");

        std::env::set_var(
            ALLOWED_ROOTS_ENV,
            format!("{};{}", root1.display(), root2.display()),
        );

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
/// `.docs/DIAGNOSE_FEDERATED_KEEP_WARM.md`).
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
    state.begin_indexing("fresh", IndexingOwner::Watcher);
    let (_, indexing) = state.freshness_for_path(&target.to_string_lossy());
    assert!(indexing, "begin_indexing must surface as indexing=true");

    state.end_indexing("fresh", IndexingOwner::Watcher);
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

/// Shared envelope for the Layer-2 e2e tests below: pin the delegation env
/// vars to a fresh temp repos.json, seed it via `seed`, spawn a REAL serve
/// (the two routes the CLI `index rm` delegation touches: `GET /health` and
/// the real `remove_repo_handler` at `DELETE /repos/{alias}`) sharing that
/// config, and wait (bounded, panicking on timeout) for it to accept.
///
/// Every step here is trap-sensitive, which is why it is a helper and not
/// copy-paste: env vars must be pinned BEFORE `cfg.save()` or the seed lands
/// in the developer's REAL registry; the port must come from
/// `listener.local_addr().port()` (never the SocketAddr — its Display form
/// makes the env var unparseable, the port silently falls back to the
/// DEFAULT 39725, and the delegation fires a live DELETE at a developer's
/// running serve); `SERVE_HOST_ENV` must be pinned to loopback or a stray
/// machine-level value sends the probe elsewhere entirely.
///
/// Returns `(state, port, env_guard)`. The caller MUST keep the
/// [`crate::testing::EnvRestore`] guard bound for the whole test — dropping
/// it (e.g. letting a helper-internal guard die) unpins the vars before the
/// delegation runs. This is why the guard is returned rather than held here.
/// The helper lives in this module rather than `src/testing.rs` because the
/// handlers it routes are private to `serve`.
async fn spawn_rm_delegation_test_serve<F>(
    tmp: &std::path::Path,
    seed: F,
) -> (Arc<ServeState>, u16, crate::testing::EnvRestore)
where
    F: FnOnce(&mut ReposConfig),
{
    let cfg_path = tmp.join("repos.json");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // Env vars FIRST, then seed repos.json (ordering trap: see doc above).
    let env = crate::testing::EnvRestore::set(&[
        (
            crate::constants::REPOS_CONFIG_ENV,
            &cfg_path.to_string_lossy(),
        ),
        (crate::constants::SERVE_PORT_ENV, port.to_string().as_str()),
        (crate::constants::SERVE_HOST_ENV, "127.0.0.1"),
    ]);
    let mut cfg = ReposConfig::default();
    seed(&mut cfg);
    cfg.save().expect("seed repos.json save must succeed");
    assert!(
        cfg_path.exists(),
        "seed repos.json must land in the temp path, not the global default"
    );

    let state = Arc::new(ServeState::new(cfg, Some(cfg_path.clone())));
    let app = axum::Router::new()
        .route(
            crate::constants::HEALTH_PATH,
            axum::routing::get(health_handler),
        )
        .route("/repos/{alias}", axum::routing::delete(remove_repo_handler))
        .with_state(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // Bounded readiness wait that FAILS LOUDLY on timeout (a silent exit
    // here surfaces downstream as an unrelated delegation failure).
    let mut ready = false;
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(ready, "test serve never became ready on port {port}");
    (state, port, env)
}

/// The full Layer-2 acceptance path: a running serve instance holds the
/// repo's registration; `remove_from_index` (the CLI code path) must
/// DELEGATE to it (health probe → DELETE /repos/{alias}), serve must stop
/// holders and delete the DB directory WITHOUT being stopped, repos.json
/// must lose the entry, and a later query for the alias must be a clean
/// "Unknown alias" (no zombie stores) — all without stopping serve.
#[tokio::test]
#[serial_test::serial]
async fn index_rm_delegates_to_running_serve_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let raw_proj = tmp.path().join("e2eproj");
    std::fs::create_dir(&raw_proj).unwrap();
    let proj = crate::cache::safe_canonicalize(&raw_proj).unwrap();
    // A real (empty) DB directory — stand-in for the LMDB dir; deleting it
    // exercises the same remove_dir_all path without opening a real env.
    std::fs::create_dir(proj.join(".codesearch.db")).unwrap();

    let (state, _port, _env) = spawn_rm_delegation_test_serve(tmp.path(), |cfg| {
        cfg.register(proj.clone());
    })
    .await;
    // Capture the alias the registration derived (directory-name based) so
    // the post-removal probe below addresses exactly what was registered.
    let alias = ReposConfig::load()
        .expect("seeded repos.json reload")
        .repos
        .iter()
        .find(|(_, p)| **p == proj)
        .map(|(a, _)| a.clone())
        .expect("seeded config must contain the proj");

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
/// stopping serve. The eviction is pinned from both sides: the registry
/// provably held a live env BEFORE the removal (precondition assert), and the
/// repos-map entry is gone AFTER it — while a holder is live the mmap'd
/// data/lock files cannot be deleted on Windows, so the dir-gone assert and
/// the eviction assert together prove the mechanism the locked-delete path
/// depends on, cross-platform.
///
/// The store is opened via the production `try_open_stores` path (creating
/// the DB dir for real), and the returned `Arc<SharedStores>` is MOVED into
/// `RepoState::Warm` with no clone kept — the test process must not itself
/// be the extra holder that defeats the delete.
#[tokio::test]
#[serial_test::serial]
async fn index_rm_deletes_db_while_serve_holds_real_lmdb_env() {
    let tmp = tempfile::tempdir().unwrap();
    let raw_proj = tmp.path().join("heldenv");
    std::fs::create_dir(&raw_proj).unwrap();
    let proj = crate::cache::safe_canonicalize(&raw_proj).unwrap();
    let db_path = proj.join(DB_DIR_NAME);
    // No pre-created DB dir: try_open_stores must create it, proving the
    // env we then hold is a real production-shaped store.

    let (state, _port, _env) = spawn_rm_delegation_test_serve(tmp.path(), |cfg| {
        cfg.register_with_alias(proj.clone(), Some("heldenv".to_string()))
            .expect("seed registration must succeed");
    })
    .await;

    // Serve opens the repo FOR REAL — a live LMDB env under db_path.
    let opened = state
        .try_open_stores("heldenv", &db_path, true, false, None)
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

/// A cached C# symbol-index Error must NOT outlive the repo it belongs to.
///
/// `repo_statuses_lightweight()` prefers the cached entry over its on-disk
/// probe, so an Error left behind by idle eviction renders a red `C#!` in the
/// TUI forever — a closed repo has no watcher left to retry a rebuild and
/// flip the state to Ready. Regression guard for the frozen-`!` fix observed
/// on a repo whose rebuild lost a one-shot LMDB double-open race days
/// earlier.
#[serial_test::serial]
#[test]
fn evicting_idle_repo_clears_frozen_csharp_error_state() {
    let _env = crate::testing::EnvRestore::set(&[(crate::constants::REPO_IDLE_TIMEOUT_ENV, "1")]);
    let state = ServeState::new(ReposConfig::default(), None);

    // Simulate the poisoned state: an Error + message cached by a failed
    // watcher rebuild, and a last-access old enough to be evicted.
    state
        .csharp_index_status
        .insert("frozen".to_string(), CSharpIndexStatus::Error);
    state.csharp_index_error.insert(
        "frozen".to_string(),
        "LMDB double-open prevented".to_string(),
    );
    state.last_access.insert(
        "frozen".to_string(),
        std::time::Instant::now() - std::time::Duration::from_secs(5),
    );

    state.evict_idle_repos();

    assert!(
        !state.csharp_index_status.contains_key("frozen"),
        "eviction must clear the cached C# status — a frozen Error renders red forever otherwise"
    );
    assert!(
        !state.csharp_index_error.contains_key("frozen"),
        "eviction must clear the cached C# error message along with the status"
    );
}

/// The model a serve query is embedded with is read from the routed repo's own
/// index metadata — never assumed to be the hub-wide default.
///
/// Regression guard for the serve hub pinning `ModelType::default()` (384-dim
/// MiniLM) for every query: on a hub whose indexes were rebuilt with
/// EmbeddingGemma that failed with "Query embedding dimension mismatch:
/// expected 768, got 384". Reintroducing the default pin makes the gemma cases
/// below fail.
#[test]
fn model_for_alias_reads_the_index_metadata_model() {
    let cases = [
        (
            "embeddinggemma-q4",
            Some(crate::embed::ModelType::EmbeddingGemma300MQ4),
        ),
        ("minilm-l6-q", Some(crate::embed::ModelType::AllMiniLML6V2Q)),
        ("bge-base", Some(crate::embed::ModelType::BGEBaseENV15)),
        // An unknown recorded name must not be silently coerced to the default:
        // callers fall back explicitly, and the resolver reports "no answer".
        ("not-a-real-model", None),
    ];

    for (model_short_name, expected) in cases {
        let (_tmp, repo_path, state) = state_with_repo("repo");
        let db_path = repo_path.join(DB_DIR_NAME);
        std::fs::create_dir_all(&db_path).unwrap();
        std::fs::write(
            db_path.join("metadata.json"),
            format!(r#"{{"model_short_name":"{model_short_name}","dimensions":768}}"#),
        )
        .unwrap();

        assert_eq!(
            state.model_for_alias("repo"),
            expected,
            "metadata model_short_name '{model_short_name}' must drive the query model"
        );
    }
}

/// Missing metadata (unindexed / legacy index) yields `None`, so the caller's
/// documented fallback to the default applies — and an unknown alias cannot
/// borrow another repo's model.
#[test]
fn model_for_alias_is_none_without_index_metadata() {
    let (_tmp, _repo_path, state) = state_with_repo("repo");
    assert_eq!(state.model_for_alias("repo"), None);
    assert_eq!(state.model_for_alias("not-registered"), None);
}

/// A single hub can hold indexes built with different models: each alias
/// resolves independently, so a group fan-out embeds each store's query with
/// that store's own model.
#[test]
fn model_for_alias_is_per_repo_not_hub_wide() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");
    let mut config = ReposConfig::default();
    for (alias, model) in [("legacy", "minilm-l6-q"), ("rebuilt", "embeddinggemma-q4")] {
        let repo_path = tmp.path().join(alias);
        std::fs::create_dir(&repo_path).unwrap();
        config
            .register_with_alias(repo_path.clone(), Some(alias.to_string()))
            .unwrap();
        let db_path = repo_path.join(DB_DIR_NAME);
        std::fs::create_dir_all(&db_path).unwrap();
        std::fs::write(
            db_path.join("metadata.json"),
            format!(r#"{{"model_short_name":"{model}"}}"#),
        )
        .unwrap();
    }
    config.save_to(&config_file).unwrap();
    let state = ServeState::new(config, Some(config_file));

    assert_eq!(
        state.model_for_alias("legacy"),
        Some(crate::embed::ModelType::AllMiniLML6V2Q)
    );
    assert_eq!(
        state.model_for_alias("rebuilt"),
        Some(crate::embed::ModelType::EmbeddingGemma300MQ4)
    );
}

/// The serve MCP service resolves the query model through the routed repo, not
/// its own (default) field. This is the exact seam the hub got wrong: it is the
/// service, not `ServeState`, that hands the model to the embedder.
#[test]
fn serve_service_uses_repo_model_not_default() {
    let (_tmp, repo_path, state) = state_with_repo("gemma-repo");
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();
    std::fs::write(
        db_path.join("metadata.json"),
        r#"{"model_short_name":"embeddinggemma-q4","dimensions":768}"#,
    )
    .unwrap();

    let svc = crate::mcp::CodesearchService::new_for_serve(std::sync::Arc::new(state)).unwrap();

    assert_eq!(
        svc.query_model(Some("gemma-repo")),
        crate::embed::ModelType::EmbeddingGemma300MQ4,
        "serve must embed a repo's queries with the model that repo was indexed with"
    );
    // No alias (unscoped) or an unknown alias falls back to the service default.
    assert_eq!(svc.query_model(None), crate::embed::ModelType::default());
    assert_eq!(
        svc.query_model(Some("not-registered")),
        crate::embed::ModelType::default()
    );
}

/// The grouped `status` model label must reflect the members' recorded models,
/// not the service default: a same-model group names that model, a mixed-model
/// hub says `mixed`. Regression guard for the status field reporting the
/// hardcoded default (`minilm-l6-q`) for every repo.
#[test]
fn group_status_model_label_is_common_or_mixed() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");
    let mut config = ReposConfig::default();
    for (alias, model) in [("legacy", "minilm-l6-q"), ("rebuilt", "embeddinggemma-q4")] {
        let repo_path = tmp.path().join(alias);
        std::fs::create_dir(&repo_path).unwrap();
        config
            .register_with_alias(repo_path.clone(), Some(alias.to_string()))
            .unwrap();
        let db_path = repo_path.join(DB_DIR_NAME);
        std::fs::create_dir_all(&db_path).unwrap();
        std::fs::write(
            db_path.join("metadata.json"),
            format!(r#"{{"model_short_name":"{model}"}}"#),
        )
        .unwrap();
    }
    config.save_to(&config_file).unwrap();
    let state = std::sync::Arc::new(ServeState::new(config, Some(config_file)));
    let svc = crate::mcp::CodesearchService::new_for_serve(state).unwrap();

    assert_eq!(
        svc.group_model_label(&["legacy".to_string(), "rebuilt".to_string()]),
        "mixed",
        "a hub holding indexes built with different models must report 'mixed'"
    );
    assert_eq!(
        svc.group_model_label(&["rebuilt".to_string()]),
        "embeddinggemma-q4",
        "a single-model group must name its base model"
    );
}

/// A fresh repo added with a model override must open its store at that model's
/// dimension, not the 384-dim default. Regression guard for `POST /repos` with
/// `model=embeddinggemma-q4`: the store used to be created at 384 and the
/// override applied only to metadata, so the reindex embedded 768-dim vectors
/// into a 384-dim store and indexed nothing.
#[tokio::test]
async fn try_open_stores_honours_dimension_override_for_a_fresh_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path().join("gemmarepo");
    std::fs::create_dir(&repo_path).unwrap();
    let db_path = repo_path.join(DB_DIR_NAME);
    assert!(!db_path.exists(), "precondition: db dir must not exist yet");

    let state = state_with_config(ReposConfig::default());

    let stores = match state.try_open_stores("gemmarepo", &db_path, true, false, Some(768)) {
        Ok(OpenedStores::Write(s)) => s,
        Ok(OpenedStores::Readonly(_)) => panic!("expected Write, got Readonly"),
        Err(e) => panic!("fresh open with a dimension override must succeed, got: {e}"),
    };

    let dims = stores
        .vector_store
        .read()
        .await
        .stats()
        .expect("stats on a freshly created store")
        .dimensions;
    assert_eq!(
        dims, 768,
        "a repo added with --model embeddinggemma-q4 must open at 768 dims, not the 384 default"
    );
}

#[test]
fn is_lmdb_format_corruption_matches_known_lmdb_errors() {
    let cases = [
        (
            "MDB_BAD_VALSIZE: Unsupported size of key/DB name/data, or wrong DUPFIXED size",
            true,
        ),
        ("Symbol rebuild failed: heed -> MDB_BAD_VALSIZE", true),
        ("storage error: wrong DUPFIXED size", true),
        ("Unsupported size of key while opening vectordb", true),
        ("scip-csharp failed with exit code 1", false),
        ("MDB_NOTFOUND: No matching key/data pair found", false),
        ("Task panicked: workspace load failed", false),
        ("", false),
    ];
    for (msg, expected) in cases {
        assert_eq!(
            ServeState::is_lmdb_format_corruption(msg),
            expected,
            "unexpected classification for {msg:?}"
        );
    }
}

/// The SCIP puts wrap their failures in a `put_ctx` context. `anyhow`'s plain
/// `{}` prints ONLY the outermost context, so stringifying a rebuild error
/// that way hides the `MDB_*` code from the classifier — and with it both the
/// recovery and the refuse-a-second-wipe branch. Rebuild errors must therefore
/// be rendered with `{:#}` (the whole chain).
#[test]
fn a_contextualised_lmdb_error_still_classifies_as_format_corruption() {
    let err = anyhow::anyhow!(
        "MDB_BAD_VALSIZE: Unsupported size of key/DB name/data, or wrong DUPFIXED size"
    )
    .context("LMDB put into 'scip_symbols' failed — key 0 byte(s), value 12 byte(s), key: ");

    assert!(
        !ServeState::is_lmdb_format_corruption(&format!("{err}")),
        "precondition: plain Display hides the LMDB code — that is the trap"
    );
    assert!(
        ServeState::is_lmdb_format_corruption(&format!("{err:#}")),
        "the alternate formatter must expose the whole chain to the classifier"
    );
}

#[tokio::test]
async fn enqueue_format_recovery_dedupes_and_starts_single_worker() {
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));
    // Same alias three times must collapse to one queued entry. The recovery
    // worker is spawned but (current-thread test runtime) does not execute
    // until an await point, so the queue content is asserted deterministically.
    assert!(state.enqueue_format_recovery("ghost-repo"));
    assert!(state.enqueue_format_recovery("ghost-repo"));
    assert!(state.enqueue_format_recovery("ghost-repo"));
    let len = state
        .format_recovery_queue
        .lock()
        .expect("queue lock")
        .len();
    assert_eq!(len, 1, "duplicate enqueues must collapse to one entry");
    assert!(
        state
            .format_recovery_worker_started
            .load(std::sync::atomic::Ordering::Acquire),
        "the first enqueue must start the recovery worker"
    );
}

/// A DB this process already wiped cannot hold old-format data, so a second
/// `MDB_BAD_VALSIZE` for that alias is a write-side bug. Re-wiping it cost
/// ~20 minutes of reindex per repo per occurrence (incident 2026-09-17).
#[tokio::test]
async fn a_second_format_recovery_for_the_same_alias_is_refused() {
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));
    assert!(state.enqueue_format_recovery("ghost-repo"));
    // Drain the queue the way the worker does, then mark the wipe as done.
    state
        .format_recovery_queue
        .lock()
        .expect("queue lock")
        .clear();
    state
        .format_recovery_done
        .insert("ghost-repo".to_string(), ());

    assert!(
        !state.enqueue_format_recovery("ghost-repo"),
        "a second wipe for an already-wiped alias must be refused"
    );
    assert!(
        state
            .format_recovery_queue
            .lock()
            .expect("queue lock")
            .is_empty(),
        "the refused alias must not be queued"
    );
    assert!(
        state.enqueue_format_recovery("other-repo"),
        "the refusal must be per alias, not global"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn format_recovery_refuses_to_wipe_while_an_index_task_is_alive() {
    // recover_repo_format's safety guard: wiping the DB dir from under a
    // live indexing task can wedge the task while it holds the store handles
    // and the process-wide job permit. The refusal branch ("did not stop
    // within the cooperative budget; refusing to wipe") was pinned nowhere.
    let (_tmp, repo_path, state) = state_with_repo("liverun");
    let state = Arc::new(state);
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();
    std::fs::write(db_path.join("data.mdb"), "fake").unwrap();

    // A task with no cancellation point: it ignores the token the way a real
    // build_index parked inside its synchronous arroy pass does. The sleep is
    // virtual — with paused time the 5 s cooperative budget expires the
    // instant the timeout is awaited.
    let token = CancellationToken::new();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
    });
    state.index_tasks.insert(
        "liverun".to_string(),
        IndexTask {
            handle,
            token,
            db_path: db_path.clone(),
            started_at: Instant::now(),
        },
    );

    let result = state.recover_repo_format("liverun").await;

    let err = result.expect_err("a live indexing task must block the wipe");
    assert!(
        err.contains("refusing to wipe"),
        "the refusal must say what it refused, got: {err}"
    );
    assert!(
        db_path.exists(),
        "the DB dir must survive a refused format-recovery wipe"
    );
    assert!(
        !state.format_recovery_done.contains_key("liverun"),
        "a refused wipe must not tombstone the alias"
    );

    // Cleanup: the parked task must not outlive the test. await_index_task
    // put it back into the map ("keeping it tracked"), so abort from there.
    if let Some((_, task)) = state.index_tasks.remove("liverun") {
        task.handle.abort();
    }
}

#[tokio::test]
async fn format_recovery_permanent_delete_failure_surfaces_and_stays_retryable() {
    // A non-lock delete failure (here: the db path is a regular file) must
    // NOT burn the lock-class retry budget, must surface as an explicit
    // error, and — because format_recovery_done records at the WIPE, not at
    // the rebuild — the alias stays eligible for another attempt instead of
    // entering the one-wipe-per-process tombstone on a wipe that never
    // happened.
    let (_tmp, repo_path, state) = state_with_repo("filerun");
    let state = Arc::new(state);
    let db_path = repo_path.join(DB_DIR_NAME);
    // A FILE: remove_dir_all fails immediately with a non-lock error on
    // every OS, deterministically taking the permanent-failure branch.
    std::fs::write(&db_path, "not a directory").unwrap();

    let result = state.recover_repo_format("filerun").await;
    let err = result.expect_err("a permanent delete failure must surface");
    assert!(
        err.contains("could not wipe"),
        "the error must name the failed wipe, got: {err}"
    );
    assert!(
        state.enqueue_format_recovery("filerun"),
        "a wipe that never happened must not tombstone the alias — the next \
         corruption detection has to be able to retry once the holder is gone"
    );
}

#[tokio::test]
#[serial]
async fn await_index_task_cancels_and_releases_the_task_handles() {
    // Regression: a leaked index task used to keep its `Arc<SharedStores>`
    // after every cooperative path forgot it — the LMDB env and
    // `.writer.lock` stayed held, and later writes failed with "Database is
    // locked by another process" on a repo that logged as idle. Cancellation
    // must be delivered AND the handle joined (not detached), so the stores
    // are released. The staleness check no longer cancels live tasks (see
    // `stale_marker_with_live_task_is_renewed_not_cancelled`);
    // `await_index_task` is the cancellation trigger.
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));

    // Stand-in for the store handles the real task captures.
    let stores = Arc::new(());
    let stores_task = stores.clone();
    let token = CancellationToken::new();
    let task_token = token.clone();
    let handle = tokio::spawn(async move {
        task_token.cancelled().await;
        drop(stores_task);
    });
    state.index_tasks.insert(
        "leaky".to_string(),
        IndexTask {
            handle,
            token: token.clone(),
            db_path: std::path::PathBuf::from("/nonexistent-db"),
            started_at: Instant::now(),
        },
    );

    assert!(
        state.await_index_task("leaky").await,
        "a task observing cancellation must join within the cooperative budget"
    );
    assert!(
        token.is_cancelled(),
        "the cancelled task must observe cancellation"
    );
    assert!(
        !state.index_tasks.contains_key("leaky"),
        "a joined task must be reaped from index_tasks"
    );
    assert_eq!(
        Arc::strong_count(&stores),
        1,
        "the cancelled task must have released its store handle"
    );
}

#[tokio::test]
#[serial]
async fn fresh_indexing_marker_leaves_its_index_task_running() {
    let _env = crate::testing::EnvRestore::set(&[(crate::constants::MAX_INDEXING_SECS_ENV, "600")]);
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));

    let token = CancellationToken::new();
    let task_token = token.clone();
    let handle = tokio::spawn(async move { task_token.cancelled().await });
    state.index_tasks.insert(
        "busy".to_string(),
        IndexTask {
            handle,
            token: token.clone(),
            db_path: std::path::PathBuf::from("/nonexistent-db"),
            started_at: Instant::now(),
        },
    );
    state.begin_indexing("busy", IndexingOwner::Reindex);

    assert!(
        state.is_indexing("busy"),
        "a fresh marker still reads as indexing"
    );
    assert!(
        !token.is_cancelled(),
        "a live reindex must never be cancelled by the staleness check"
    );
    token.cancel();
}

#[tokio::test]
async fn self_clean_keeps_the_db_dir_of_a_still_registered_repo() {
    // Regression: the post-build guards reach their cleanup branch whenever
    // their token is cancelled, and a cancelled token no longer means "the
    // repo was removed" (idle eviction and the stale-marker cleanup both
    // cancel one). Deleting on the token alone wiped a live repo's index.
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = crate::cache::safe_canonicalize(tmp.path()).unwrap();
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("live".to_string()))
        .unwrap();
    let state = Arc::new(ServeState::new(config, None));

    state.self_clean_if_unregistered("live", &db_path);
    assert!(
        db_path.exists(),
        "a registered repo's DB dir must survive a cancelled task"
    );

    // Unregistered alias: the orphan cleanup still runs.
    state.self_clean_if_unregistered("gone", &db_path);
    assert!(
        !db_path.exists(),
        "an unregistered alias's orphaned DB dir must still be cleaned up"
    );
}

#[tokio::test]
#[serial]
async fn own_lmdb_holder_and_indexing_count_as_held_in_process() {
    let _env = crate::testing::EnvRestore::set(&[(crate::constants::MAX_INDEXING_SECS_ENV, "600")]);
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));
    let tmp = tempfile::tempdir().unwrap();
    let db_path = crate::cache::safe_canonicalize(tmp.path())
        .unwrap()
        .join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();

    assert!(!state.is_held_in_process("own", &db_path));
    let store = crate::vectordb::VectorStore::new(&db_path, 4).unwrap();
    assert!(
        state.is_held_in_process("own", &db_path),
        "an env this process holds must never read as an external lock"
    );
    drop(store);
    assert!(!state.is_held_in_process("own", &db_path));

    state.begin_indexing("own", IndexingOwner::Reindex);
    assert!(state.is_held_in_process("own", &db_path));
    state.end_indexing("own", IndexingOwner::Reindex);
}

#[tokio::test]
#[serial]
async fn warm_query_does_not_start_fsw_while_warmup_indexes() {
    let _env = crate::testing::EnvRestore::set(&[(crate::constants::MAX_INDEXING_SECS_ENV, "600")]);
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));
    let tmp = tempfile::tempdir().unwrap();
    let db_path = crate::cache::safe_canonicalize(tmp.path())
        .unwrap()
        .join(DB_DIR_NAME);
    let OpenedStores::Write(stores) = state
        .try_open_stores("warming", &db_path, true, false, None)
        .expect("open must succeed")
    else {
        panic!("brand-new repo must open Write");
    };
    state
        .repos
        .insert("warming".to_string(), RepoState::Warm { stores });
    state.begin_indexing("warming", IndexingOwner::Warmup);

    let result = state.try_cached_stores("warming", true);
    assert!(
        matches!(result, Some(Ok(_))),
        "queries must be answered from Warm"
    );
    assert!(
        matches!(
            state.repos.get("warming").unwrap().value(),
            RepoState::Warm { .. }
        ),
        "a query during warmup indexing must not transition Warm to Write"
    );
    state.end_indexing("warming", IndexingOwner::Warmup);
    state.repos.remove("warming");
}

#[tokio::test]
#[serial]
async fn indexing_marker_heartbeat_survives_past_max() {
    // Regression: a large-repo warmup runs longer than MAX_INDEXING_SECS, so
    // the lazy stale-marker eviction dropped its marker mid-refresh and the
    // reaper could evict the repo while the refresh still held its stores.
    // The per-batch heartbeat must keep the marker alive for as long as the
    // refresh is making progress.
    let _env = crate::testing::EnvRestore::set(&[(crate::constants::MAX_INDEXING_SECS_ENV, "2")]);
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));

    // Control: without renewal the marker expires at the threshold.
    state.begin_indexing("ctrl", IndexingOwner::Warmup);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(
        !state.is_indexing("ctrl"),
        "an unrenewed marker must expire at the threshold"
    );

    // Heartbeat: renewal inside the window keeps the marker alive past max.
    state.begin_indexing("hb", IndexingOwner::Warmup);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    state.renew_indexing("hb", IndexingOwner::Warmup);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        state.is_indexing("hb"),
        "a heartbeat-renewed marker must stay alive past the threshold"
    );
    state.end_indexing("hb", IndexingOwner::Warmup);
    assert!(
        !state.is_indexing("hb"),
        "end_indexing must clear the marker"
    );
}

#[tokio::test]
#[serial]
async fn fsw_callback_end_cannot_clear_a_concurrent_reindex_marker() {
    // Regression: the FSW completion callback cleared the whole alias entry
    // with a plain bool, so a watcher refresh finishing mid-run erased a
    // concurrent force reindex's marker — the reaper could then evict the
    // repo while the reindex still held its stores. Markers are owner-scoped
    // now: the watcher ends only its own.
    let _env = crate::testing::EnvRestore::set(&[(crate::constants::MAX_INDEXING_SECS_ENV, "600")]);
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));

    assert!(state.begin_indexing("race", IndexingOwner::Reindex));
    let cb = state.make_indexing_status_callback("race");
    cb(true);
    assert!(
        state.is_indexing("race"),
        "watcher marker stacks on reindex's"
    );
    cb(false);
    assert!(
        state.is_indexing("race"),
        "the watcher's completion must NOT clear the reindex marker"
    );
    state.end_indexing("race", IndexingOwner::Reindex);
    assert!(!state.is_indexing("race"));
}

#[tokio::test]
#[serial]
async fn begin_indexing_rejects_while_any_owner_marker_is_fresh() {
    // The exclusivity guard must survive owner-scoping: a force reindex is
    // still rejected while warmup (or any other owner) is indexing, and
    // allowed again once that marker ends.
    let _env = crate::testing::EnvRestore::set(&[(crate::constants::MAX_INDEXING_SECS_ENV, "600")]);
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));

    assert!(state.begin_indexing("excl", IndexingOwner::Warmup));
    assert!(
        !state.begin_indexing("excl", IndexingOwner::Reindex),
        "a fresh marker from another owner must reject a concurrent reindex"
    );
    state.end_indexing("excl", IndexingOwner::Warmup);
    assert!(
        state.begin_indexing("excl", IndexingOwner::Reindex),
        "ending the warmup marker must let the reindex proceed"
    );
    state.end_indexing("excl", IndexingOwner::Reindex);
    assert!(!state.is_indexing("excl"));
}

#[tokio::test]
#[serial]
async fn stale_eviction_is_scoped_to_the_stale_owner() {
    // A stale marker from a leaked task must not block (or cancel for) an
    // owner whose marker is still fresh — and an eviction must not touch
    // other owners' markers.
    let _env = crate::testing::EnvRestore::set(&[(crate::constants::MAX_INDEXING_SECS_ENV, "1")]);
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));

    // Fresh warmup marker; a stale reindex marker from a leaked task.
    state.begin_indexing("scoped", IndexingOwner::Warmup);
    let stale = Instant::now()
        .checked_sub(Duration::from_secs(2))
        .expect("monotonic clock at least 2s old");
    state
        .active_reindexes
        .entry("scoped".to_string())
        .or_default()
        .insert(IndexingOwner::Reindex, stale);

    assert!(
        state.is_indexing("scoped"),
        "the fresh warmup marker must survive the stale owner's eviction"
    );

    // Once warmup's marker ages out too, everything is evicted.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(!state.is_indexing("scoped"));
    assert!(
        !state.active_reindexes.contains_key("scoped"),
        "an empty owner map must be removed, not left behind"
    );
}

// ── Always-on symbol indexes (features/always-on-symbol-indexes) ──────────

/// The per-(alias, language) claim must reject a second fresh claim, accept
/// one per language independently, become claimable after release, and take
/// over a claim older than the indexing timeout (leaked by a crashed task).
#[test]
fn symbol_rebuild_claim_rejects_fresh_and_takes_over_stale() {
    let state = ServeState::new(ReposConfig::default(), None);

    assert!(
        state.begin_symbol_rebuild("repo", "csharp"),
        "first claim must succeed"
    );
    assert!(
        !state.begin_symbol_rebuild("repo", "csharp"),
        "a second fresh claim must be rejected"
    );
    assert!(
        state.begin_symbol_rebuild("repo", "typescript"),
        "a different language must claim independently"
    );

    state.end_symbol_rebuild("repo", "csharp");
    assert!(
        state.begin_symbol_rebuild("repo", "csharp"),
        "a released slot must be claimable again"
    );

    // Stale takeover: a claim past the indexing timeout is taken over.
    let max = state.indexing_timeout();
    state.symbol_rebuild_in_flight.insert(
        "csharp:stale".to_string(),
        std::time::Instant::now() - max - std::time::Duration::from_secs(1),
    );
    assert!(
        state.begin_symbol_rebuild("stale", "csharp"),
        "a stale claim must be taken over"
    );
}

/// The generic gate must answer NotApplicable for a repo without the
/// language's entrypoint (no tsconfig.json) — deterministically, before the
/// machine-dependent helper check — and route C# through its dedicated
/// evaluator (.sln check → NoSolutionFile).
#[test]
fn evaluate_symbol_rebuild_gates_applicability_before_availability() {
    // `evaluate_symbol_rebuild` takes `self: &Arc<Self>` (it may be forwarded
    // to spawn_blocking by callers), so the probe state lives in an Arc.
    let state = std::sync::Arc::new(ServeState::new(ReposConfig::default(), None));
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(DB_DIR_NAME);

    assert_eq!(
        state.evaluate_symbol_rebuild("typescript", "x", tmp.path(), &db),
        RebuildDecision::NotApplicable,
        "a repo without tsconfig.json is not applicable, helper or not"
    );
    assert_eq!(
        state.evaluate_symbol_rebuild("csharp", "x", tmp.path(), &db),
        RebuildDecision::NoSolutionFile,
        "C# must keep its dedicated .sln-first evaluator"
    );
}

/// The find_impact self-heal must no-op quietly for an unknown alias (remote
/// mounts, typos) and for a registered repo the language does not apply to —
/// no claim taken, no C# status touched, no panic.
#[tokio::test]
async fn heal_missing_symbol_index_noops_outside_its_remit() {
    // Unknown alias.
    let state = std::sync::Arc::new(ServeState::new(ReposConfig::default(), None));
    heal_missing_symbol_index(&state, "ghost", "csharp").await;
    assert!(
        state.symbol_rebuild_in_flight.is_empty(),
        "unknown alias must not take a claim"
    );

    // Registered alias, non-applicable language (no .sln / tsconfig.json).
    let (_tmp, _path, bare) = state_with_repo("plainrepo");
    let state = std::sync::Arc::new(bare);
    heal_missing_symbol_index(&state, "plainrepo", "csharp").await;
    heal_missing_symbol_index(&state, "plainrepo", "typescript").await;
    assert!(
        state.symbol_rebuild_in_flight.is_empty(),
        "non-applicable repo must not take a claim"
    );
    assert!(
        !state.csharp_index_status.contains_key("plainrepo"),
        "non-applicable repo must not flip the C# indicator"
    );
}

/// `maybe_rebuild_symbols` on a repo no language applies to must neither
/// panic nor leave claims behind — with and without force (the force path
/// skips the gate but trigger_symbol_rebuild still skips non-applicable
/// repos before claiming).
#[tokio::test]
async fn maybe_rebuild_symbols_skips_non_applicable_repo_under_force() {
    let (_tmp, _path, bare) = state_with_repo("plainrepo2");
    let state = std::sync::Arc::new(bare);
    let path = state.config.read().unwrap().resolve("plainrepo2").unwrap();
    let db = path.join(DB_DIR_NAME);
    maybe_rebuild_symbols("plainrepo2", &path, &db, &state, true).await;
    assert!(
        state.symbol_rebuild_in_flight.is_empty(),
        "force must not claim a slot for a non-applicable repo"
    );
}

/// A dummy `scip-csharp` helper at the env-override path: available for
/// detection, fails on execution — which flips the C# status to Error while
/// proving a rebuild was actually attempted. The helper file lives in
/// `root`, which the caller keeps alive via its TempDir.
fn dummy_csharp_helper(root: &std::path::Path) -> crate::testing::EnvRestore {
    let helper = root.join(if cfg!(windows) {
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

/// Phase-2 composition wiring: a registered repo whose language applies and
/// whose index is missing must flow gate → queue → trigger end-to-end. The
/// dummy helper makes the rebuild FAIL, observable as C# status Error once
/// `run_phase_2_symbols` completes — None would mean the candidate was never
/// queued (the composition gap this test pins).
#[tokio::test]
#[serial]
async fn run_phase_2_symbols_wires_candidate_through_to_trigger() {
    let helper_root = tempfile::tempdir().unwrap();
    let _env = dummy_csharp_helper(helper_root.path());

    let (_tmp, repo_path, bare) = state_with_repo("phase2repo");
    std::fs::write(
        repo_path.join("test.sln"),
        b"Microsoft Visual Studio Solution File",
    )
    .unwrap();

    let state = std::sync::Arc::new(bare);
    state.run_phase_2_symbols().await;

    assert_eq!(
        state
            .csharp_index_status
            .get("phase2repo")
            .map(|e| *e.value()),
        Some(CSharpIndexStatus::Error),
        "the missing C# index must be queued and the rebuild attempted \
         (dummy helper → Error); None means never queued"
    );
    assert!(
        state.symbol_rebuild_in_flight.is_empty(),
        "the claim must be released after the failed rebuild"
    );
}

/// A successful rebuild WITHOUT survived failures is Ready with the error
/// cleared; one WITH survived workspace failures is Partial (red `C#⚠` in
/// the TUI) with a bounded detail line for the info panel — capped at the
/// first three entries so the detail row stays readable.
#[test]
fn csharp_success_outcome_partial_when_warnings_survived() {
    assert_eq!(
        csharp_success_outcome(&[]),
        (CSharpIndexStatus::Ready, None),
        "a clean rebuild must read Ready with no error detail"
    );

    let degraded = [
        "summary".to_string(),
        "A.csproj: failure one".to_string(),
        "B.csproj: failure two".to_string(),
        "C.csproj: failure three".to_string(),
        "D.csproj: failure four".to_string(),
    ];
    let (status, detail) = csharp_success_outcome(&degraded);
    assert_eq!(status, CSharpIndexStatus::Partial);
    let detail = detail.expect("partial carries detail");
    assert!(
        detail.contains("summary") && detail.contains("B.csproj"),
        "detail joins the leading entries, got: {detail}"
    );
    assert!(
        !detail.contains("D.csproj"),
        "detail is capped at three entries, got: {detail}"
    );
}

/// The watcher notifier must apply the SAME status contract as
/// trigger_symbol_rebuild's success arm: Succeeded-with-warnings → Partial
/// (red C#⚠) with bounded detail — never green — and a clean Succeeded →
/// Ready with the detail cleared. Reverting the arm to insert(Ready) ships
/// the round-1 defect (green indicator on a degraded index) with a green
/// suite.
#[test]
fn csharp_notifier_partial_on_succeeded_with_warnings() {
    let state = ServeState::new(ReposConfig::default(), None);
    let notifier = state.make_csharp_notifier("wrepo");

    notifier(SymbolRebuildSignal::Started);
    assert_eq!(
        state.csharp_index_status.get("wrepo").map(|e| *e.value()),
        Some(CSharpIndexStatus::Indexing)
    );

    notifier(SymbolRebuildSignal::Succeeded {
        index_warnings: vec![
            "summary".to_string(),
            "A.csproj: boom".to_string(),
            "B.csproj: boom2".to_string(),
            "C.csproj: boom3".to_string(),
        ],
    });
    assert_eq!(
        state.csharp_index_status.get("wrepo").map(|e| *e.value()),
        Some(CSharpIndexStatus::Partial),
        "a degraded watcher rebuild must render Partial, never green"
    );
    let detail = state
        .csharp_index_error
        .get("wrepo")
        .map(|e| e.value().clone())
        .expect("detail stored");
    assert!(detail.contains("summary") && !detail.contains("C.csproj"));

    notifier(SymbolRebuildSignal::Succeeded {
        index_warnings: Vec::new(),
    });
    assert_eq!(
        state.csharp_index_status.get("wrepo").map(|e| *e.value()),
        Some(CSharpIndexStatus::Ready)
    );
    assert!(
        !state.csharp_index_error.contains_key("wrepo"),
        "a clean rebuild must clear the detail"
    );
}

/// `repo_statuses_lightweight()` must carry the Partial status AND its
/// detail message — that pair is what renders the red `C#⚠` and the info
/// panel's failure line.
#[test]
fn repo_statuses_lightweight_carries_partial_with_detail() {
    let (_tmp, _path, bare) = state_with_repo("partialrepo");
    let state = std::sync::Arc::new(bare);
    state
        .csharp_index_status
        .insert("partialrepo".to_string(), CSharpIndexStatus::Partial);
    state.csharp_index_error.insert(
        "partialrepo".to_string(),
        "summary | A.csproj: failure one".to_string(),
    );

    let statuses = state.repo_statuses_lightweight();
    let (_, info) = statuses
        .iter()
        .find(|(alias, _)| alias == "partialrepo")
        .expect("partialrepo row");
    assert_eq!(info.csharp_index, CSharpIndexStatus::Partial);
    assert_eq!(
        info.csharp_error.as_deref(),
        Some("summary | A.csproj: failure one"),
        "the detail message must ride on the status row"
    );
}

/// The serve-tui `i` overlay reads the DURABLE warnings from the on-disk
/// meta (survives a restart, unlike the in-memory status) and prefixes them
/// with the language. A repo whose meta carries no warnings yields none.
#[test]
fn build_info_overlay_reads_symbol_warnings_from_meta() {
    use crate::serve::tui::build_info_overlay;

    let (_tmp, repo_path, bare) = state_with_repo("inforepo");
    let db = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir_all(&db).unwrap();
    std::fs::write(
        db.join("metadata.json"),
        r#"{"schema_version":1,"dimensions":2,"model_short_name":"minilm-l6-q"}"#,
    )
    .unwrap();
    // Seed the meta the way a degraded rebuild does (see the roundtrip test
    // in csharp_tests for the read side).
    let env = crate::symbols::get_shared_scip_env(&db).unwrap();
    let mut wtxn = env.write_txn().unwrap();
    let meta: heed::Database<heed::types::Str, heed::types::Str> = env
        .create_database(&mut wtxn, Some(crate::constants::SCIP_META_DB_NAME))
        .unwrap();
    meta.put(
        &mut wtxn,
        crate::constants::SCIP_INDEX_WARNINGS_KEY,
        r#"["summary","A.csproj: failure one"]"#,
    )
    .unwrap();
    wtxn.commit().unwrap();

    let state = std::sync::Arc::new(bare);
    let statuses = state.repo_statuses_lightweight();
    let idx = statuses
        .iter()
        .position(|(alias, _)| alias == "inforepo")
        .expect("inforepo row");
    let overlay = build_info_overlay(idx, &statuses, &state).expect("overlay");
    match overlay {
        crate::serve::tui_common::OverlayState::Info {
            symbol_warnings, ..
        } => {
            assert!(
                symbol_warnings.len() == 2 && symbol_warnings[0].starts_with("[csharp] summary"),
                "warnings must be language-prefixed from meta, got: {symbol_warnings:?}"
            );
        }
        other => panic!("expected Info overlay, got {other:?}"),
    }
}

/// The serve-mode find_impact self-heal spawn must reach
/// `trigger_symbol_rebuild` for a registered, applicable repo: after the
/// warned empty answer, the C# status eventually flips to Error (dummy
/// helper) in the serve state the heal captured.
#[tokio::test]
#[serial]
async fn find_impact_self_heal_runs_the_rebuild_in_serve_mode() {
    let helper_root = tempfile::tempdir().unwrap();
    let _env = dummy_csharp_helper(helper_root.path());

    let (_tmp, repo_path, bare) = state_with_repo("healrepo");
    std::fs::write(
        repo_path.join("test.sln"),
        b"Microsoft Visual Studio Solution File",
    )
    .unwrap();
    // The routing layer requires an existing index dir before it will open
    // the repo (seed like the find_impact fixtures do).
    let db = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir_all(&db).unwrap();
    std::fs::write(
        db.join("metadata.json"),
        r#"{"schema_version":1,"dimensions":2,"model_short_name":"minilm-l6-q"}"#,
    )
    .unwrap();
    let state = std::sync::Arc::new(bare);

    let request = crate::mcp::types::FindImpactRequest {
        symbol_name: Some("ResolveLinkedActivityAsync".to_string()),
        file: None,
        line: None,
        symbol_key: None,
        language: Some("csharp".to_string()),
        project: Some("healrepo".to_string()),
        group: None,
    };
    // The REST mirror builds its own serve-mode service from the state —
    // the same path the MCP tool takes for a project-scoped query.
    let v = crate::mcp::rest_find_impact_handler(
        axum::extract::State(state.clone()),
        axum::Json(request),
    )
    .await
    .expect("rest mirror result")
    .0;
    let out = v.to_string();
    assert!(
        v["warnings"]
            .as_array()
            .is_some_and(|w| !w.is_empty() && w[0].as_str().is_some_and(|s| s.contains("UNKNOWN"))),
        "the never-built index answer must carry the UNKNOWN warning, got: {out}"
    );

    // The detached heal task runs the rebuild (dummy helper → Error).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if state
            .csharp_index_status
            .get("healrepo")
            .map(|e| *e.value())
            == Some(CSharpIndexStatus::Error)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the self-heal never flipped the C# status — spawn branch not wired"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// ── Rebuild gate: builder-version stamp + degraded retry ─────────────────

/// Seeds the SCIP meta for `db` exactly like `rebuild` stamps it (rebuild
/// timestamp + key format), plus the caller's extra key/values — so gate
/// tests can shape a fresh, stale or degraded index without the helper.
fn seed_scip_meta(db: &std::path::Path, extra: &[(&str, &str)]) {
    let env = crate::symbols::get_shared_scip_env(db).unwrap();
    let mut wtxn = env.write_txn().unwrap();
    let meta: heed::Database<heed::types::Str, heed::types::Str> = env
        .open_database(&wtxn, Some(crate::constants::SCIP_META_DB_NAME))
        .unwrap()
        .unwrap();
    meta.put(&mut wtxn, crate::constants::SCIP_REBUILD_TIMESTAMP_KEY, "0")
        .unwrap();
    meta.put(
        &mut wtxn,
        crate::constants::SCIP_KEY_FORMAT_KEY,
        crate::constants::SCIP_KEY_FORMAT,
    )
    .unwrap();
    for (key, value) in extra {
        meta.put(&mut wtxn, *key, *value).unwrap();
    }
    wtxn.commit().unwrap();
}

/// Shared arrangement: applicable repo (`.sln`), dummy helper, seeded SCIP
/// meta, config timestamps showing a fresh build (last_scip ≥ last_changed)
/// so ONLY the builder-version / degraded-retry checks can move the gate.
/// The helper-env guard and both tempdirs are returned so they outlive the
/// fixture.
#[allow(clippy::type_complexity)]
fn gate_fixture(
    alias: &str,
    meta_extra: &[(&str, &str)],
) -> (
    crate::testing::EnvRestore,
    tempfile::TempDir,
    tempfile::TempDir,
    std::path::PathBuf,
    std::sync::Arc<ServeState>,
) {
    let helper_root = tempfile::tempdir().unwrap();
    let env_guard = dummy_csharp_helper(helper_root.path());
    let (tmp, repo_path, bare) = state_with_repo(alias);
    std::fs::write(repo_path.join("test.sln"), b"Solution File").unwrap();
    let db = repo_path.join(DB_DIR_NAME);
    seed_scip_meta(&db, meta_extra);
    let state = std::sync::Arc::new(bare);
    let now = ServeState::now_unix_secs();
    {
        let mut cfg = state.config.write().unwrap();
        cfg.touch_last_changed(alias, now - 100);
        cfg.touch_last_scip(alias, now);
    }
    (env_guard, helper_root, tmp, repo_path, state)
}

/// An index this binary never produced (no builder stamp — anything built
/// before the stamp existed) must read as stale: a deployed binary rebuilds
/// the indexes it inherited itself at startup, never waits for a manual
/// command. This is the DRM-24427_Versioning defect class.
#[test]
#[serial]
fn evaluate_rebuilds_an_index_from_an_older_binary() {
    let (_env, _h, _tmp, repo_path, state) = gate_fixture("oldbinary", &[]);

    assert_eq!(
        state.evaluate_symbol_rebuild(
            "csharp",
            "oldbinary",
            &repo_path,
            &repo_path.join(DB_DIR_NAME)
        ),
        RebuildDecision::ChangedSinceLastBuild,
        "absent builder stamp must read as stale"
    );
}

/// A degraded index (warnings in meta, current builder version) rebuilds
/// exactly once per serve process: the second evaluation is Fresh (no loop
/// while the environment stays broken); a fresh process gets a fresh set.
#[test]
#[serial]
fn evaluate_retries_a_degraded_index_once_per_process() {
    let (_env, _h, _tmp, repo_path, state) = gate_fixture(
        "degraded",
        &[(
            crate::constants::SCIP_INDEX_BUILDER_VERSION_KEY,
            crate::constants::INDEX_BUILDER_VERSION,
        )],
    );
    let db = repo_path.join(DB_DIR_NAME);
    // Re-seed WITH warnings (the fixture seeded only the version stamp).
    seed_scip_meta(
        &db,
        &[(
            crate::constants::SCIP_INDEX_WARNINGS_KEY,
            r#"["Broken.csproj: Msbuild failed when processing the file"]"#,
        )],
    );

    assert_eq!(
        state.evaluate_symbol_rebuild("csharp", "degraded", &repo_path, &db),
        RebuildDecision::ChangedSinceLastBuild,
        "a degraded index must get its one automatic retry"
    );
    assert_eq!(
        state.evaluate_symbol_rebuild("csharp", "degraded", &repo_path, &db),
        RebuildDecision::Fresh,
        "the retry must not loop within one serve process"
    );
}

/// A clean index with the current builder stamp and fresh timestamps stays
/// Fresh — the gate must not churn indexes that are actually fine.
#[test]
#[serial]
fn evaluate_stays_fresh_when_index_is_current_and_clean() {
    let (_env, _h, _tmp, repo_path, state) = gate_fixture(
        "cleanidx",
        &[(
            crate::constants::SCIP_INDEX_BUILDER_VERSION_KEY,
            crate::constants::INDEX_BUILDER_VERSION,
        )],
    );

    assert_eq!(
        state.evaluate_symbol_rebuild(
            "csharp",
            "cleanidx",
            &repo_path,
            &repo_path.join(DB_DIR_NAME)
        ),
        RebuildDecision::Fresh,
        "current + clean + fresh timestamps must not rebuild"
    );
    assert!(
        !state.degraded_rebuild_attempted.contains("cleanidx"),
        "a clean index must not consume the degraded retry"
    );
}

// ---------------------------------------------------------------------------
// Queue-backlog regressions: tracked index tasks, stale-marker liveness,
// deferred DB cleanup.
// ---------------------------------------------------------------------------

/// Age `alias`'s Reindex marker past the stale threshold.
fn aged_marker(state: &ServeState, alias: &str) {
    state
        .active_reindexes
        .entry(alias.to_string())
        .or_default()
        .insert(
            IndexingOwner::Reindex,
            Instant::now() - Duration::from_secs(MAX_INDEXING_SECS * 2),
        );
}

/// A handle whose task has already completed (the handle is kept, not awaited).
async fn finished_handle() -> tokio::task::JoinHandle<()> {
    let handle = tokio::spawn(async {});
    while !handle.is_finished() {
        tokio::task::yield_now().await;
    }
    handle
}

fn tracked_task(handle: tokio::task::JoinHandle<()>, db_path: std::path::PathBuf) -> IndexTask {
    IndexTask {
        handle,
        token: CancellationToken::new(),
        db_path,
        started_at: Instant::now(),
    }
}

#[tokio::test]
#[serial]
async fn stale_marker_with_live_task_is_renewed_not_cancelled() {
    // Regression: a reindex task queued behind the process-wide job gate can
    // outlive MAX_INDEXING_SECS without being a leak. Evicting its marker and
    // cancelling the task cancels healthy queued work whenever a backlog
    // outlives the threshold.
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));
    let alias = "live-task-repo";
    let handle = tokio::spawn(async { std::future::pending::<()>().await });
    let token = CancellationToken::new();
    state.index_tasks.insert(
        alias.to_string(),
        IndexTask {
            handle,
            token: token.clone(),
            db_path: std::path::PathBuf::from("/nonexistent-db"),
            started_at: Instant::now(),
        },
    );

    aged_marker(&state, alias);
    assert!(
        !state.begin_indexing(alias, IndexingOwner::Reindex),
        "a live task must block a second run even with a stale marker"
    );

    // A stale marker owned by another flow (Watcher) carries no liveness
    // evidence of its own and must still be evicted, even while the index
    // task lives.
    state
        .active_reindexes
        .entry(alias.to_string())
        .or_default()
        .insert(
            IndexingOwner::Watcher,
            Instant::now() - Duration::from_secs(MAX_INDEXING_SECS * 2),
        );
    aged_marker(&state, alias);
    assert!(
        state.is_indexing(alias),
        "a live task must keep reporting indexing"
    );
    assert!(
        state
            .active_reindexes
            .get(alias)
            .is_some_and(|owners| !owners.contains_key(&IndexingOwner::Watcher)),
        "a stale non-Reindex marker must still be evicted"
    );
    assert!(
        !token.is_cancelled(),
        "the marker check must never cancel a live task"
    );
    let renewed = state
        .active_reindexes
        .get(alias)
        .and_then(|owners| owners.get(&IndexingOwner::Reindex).copied())
        .expect("the marker must survive the check");
    assert!(
        Instant::now().duration_since(renewed) < Duration::from_secs(5),
        "a live marker must be renewed, not evicted"
    );

    {
        let entry = state.index_tasks.get(alias);
        if let Some(entry) = entry {
            entry.value().handle.abort();
        }
    }
}

#[tokio::test]
#[serial]
async fn stale_marker_without_live_task_is_evicted_and_entry_reaped() {
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));
    let alias = "dead-task-repo";
    let handle = finished_handle().await;
    state.index_tasks.insert(
        alias.to_string(),
        tracked_task(handle, std::path::PathBuf::from("/nonexistent-db")),
    );
    aged_marker(&state, alias);

    assert!(
        !state.is_indexing(alias),
        "with no live task the stale marker must be evicted"
    );
    assert!(
        !state.index_tasks.contains_key(alias),
        "the finished entry must be reaped"
    );
    assert!(
        !state.active_reindexes.contains_key(alias),
        "the evicted marker must be cleaned up"
    );
}

#[tokio::test]
#[serial]
async fn sweeper_reaps_finished_unregistered_task_and_cleans_db() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("gone-repo").join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();
    std::fs::write(db_path.join("metadata.json"), b"{}").unwrap();

    let state = Arc::new(ServeState::new(ReposConfig::default(), None));
    let handle = finished_handle().await;
    state.index_tasks.insert(
        "gone-repo".to_string(),
        tracked_task(handle, db_path.clone()),
    );
    state
        .active_reindexes
        .entry("gone-repo".to_string())
        .or_default()
        .insert(IndexingOwner::Reindex, Instant::now());

    state.sweep_index_tasks();

    assert!(!state.index_tasks.contains_key("gone-repo"));
    assert!(!db_path.exists(), "the orphaned DB dir must be deleted");
    assert!(!state.active_reindexes.contains_key("gone-repo"));
}

#[tokio::test]
#[serial]
async fn await_index_task_timeout_keeps_task_tracked() {
    // Regression: the old code removed the entry before waiting and dropped
    // the handle on timeout, making the detached task invisible while it held
    // its stores and the job permit.
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));
    let alias = "parked-task";
    let token = CancellationToken::new();
    let handle = tokio::spawn(async { std::future::pending::<()>().await });
    state.index_tasks.insert(
        alias.to_string(),
        IndexTask {
            handle,
            token: token.clone(),
            db_path: std::path::PathBuf::from("/nonexistent-db"),
            started_at: Instant::now(),
        },
    );

    let exited = state
        .await_index_task_with_budget(alias, Duration::from_millis(50))
        .await;
    assert!(!exited, "a parked task cannot exit within the budget");
    assert!(
        state.index_tasks.contains_key(alias),
        "the task must stay tracked — never invisible"
    );
    assert!(token.is_cancelled(), "cancellation must still be signalled");

    {
        let entry = state.index_tasks.get(alias);
        if let Some(entry) = entry {
            entry.value().handle.abort();
        }
    }
}

#[tokio::test]
#[serial]
async fn await_index_task_returns_true_when_task_exits() {
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));
    let handle = tokio::spawn(async {});
    state.index_tasks.insert(
        "quick-task".to_string(),
        tracked_task(handle, std::path::PathBuf::from("/nonexistent-db")),
    );
    assert!(
        state
            .await_index_task_with_budget("quick-task", Duration::from_secs(5))
            .await
    );
    assert!(!state.index_tasks.contains_key("quick-task"));
}

#[tokio::test]
#[serial]
async fn remove_repo_defers_db_delete_while_index_task_is_still_running() {
    // Regression: deleting the DB dir under a live (uninterruptible) index
    // task is what can wedge a hub — the task never reaches a cancellation
    // point and keeps its stores and the job permit for the process lifetime.
    let (_tmp, repo_path, state) = state_with_repo("slowrepo");
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();
    std::fs::write(db_path.join("data.mdb"), "fake").unwrap();

    // A task that ignores cancellation entirely, like a parked build.
    let token = CancellationToken::new();
    let handle = tokio::spawn(async { std::future::pending::<()>().await });
    state.index_tasks.insert(
        "slowrepo".to_string(),
        IndexTask {
            handle,
            token,
            db_path: db_path.clone(),
            started_at: Instant::now(),
        },
    );

    let outcome = state
        .remove_repo_with_budget("slowrepo", Duration::from_millis(100))
        .await
        .expect("remove_repo should succeed");

    assert!(
        !outcome.db_deleted,
        "the DB dir must NOT be deleted out from under a live task"
    );
    assert!(db_path.exists(), "the dir stays until the task exits");
    assert!(
        state.index_tasks.contains_key("slowrepo"),
        "the running task must stay tracked for the sweeper"
    );

    // Simulate the task finally exiting (aborted here; a real one would run
    // its post-build guard): the sweeper is the backstop that reaps the entry
    // and deletes the orphaned dir.
    if let Some(entry) = state.index_tasks.get("slowrepo") {
        entry.value().handle.abort();
    }
    loop {
        let finished = state
            .index_tasks
            .get("slowrepo")
            .is_some_and(|entry| entry.value().handle.is_finished());
        if finished {
            break;
        }
        tokio::task::yield_now().await;
    }
    state.sweep_index_tasks();

    assert!(!state.index_tasks.contains_key("slowrepo"));
    assert!(
        !db_path.exists(),
        "the sweeper must clean the orphaned DB dir"
    );
}

#[tokio::test]
#[serial]
async fn sweeper_reasserts_cancellation_for_a_removed_live_task() {
    let state = Arc::new(ServeState::new(ReposConfig::default(), None));
    let token = CancellationToken::new();
    let handle = tokio::spawn(async { std::future::pending::<()>().await });
    state.index_tasks.insert(
        "removed-repo".to_string(),
        IndexTask {
            handle,
            token: token.clone(),
            db_path: std::path::PathBuf::from("/nonexistent-db"),
            started_at: Instant::now(),
        },
    );

    state.sweep_index_tasks();

    assert!(
        token.is_cancelled(),
        "a removed repo's live task must have cancellation re-asserted"
    );
    assert!(
        state.index_tasks.contains_key("removed-repo"),
        "a live task must stay tracked until it exits"
    );

    {
        let entry = state.index_tasks.get("removed-repo");
        if let Some(entry) = entry {
            entry.value().handle.abort();
        }
    }
}

// === Group fan-out chunk-id collision (misattribution) =================
//
// Chunk ids are per-repo counters, so every repo in a group holds chunk id 0.
// The group tools used to resolve a bare id by probing every store and taking
// the first answer — the alphabetically-first repo won the hit, the true
// definition was filtered out as "wrong kind", and unrelated content rode the
// stolen score. These fixtures seed BOTH repos' only chunk at id 0 so any
// resolution that ignores the origin repo fails deterministically.

/// Seed `<root>/.codesearch.db` with one chunk (id 0) whose content mentions
/// the search term, then release the writer handle so ServeState can open it.
async fn seed_collision_repo(
    root: &std::path::Path,
    rel_path: &str,
    kind: crate::chunker::ChunkKind,
    signature: Option<&str>,
    content: &str,
) {
    let db_path = root.join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();
    std::fs::write(
        db_path.join("metadata.json"),
        r#"{"schema_version":1,"dimensions":2,"model_short_name":"minilm-l6-q"}"#,
    )
    .unwrap();
    let stores = crate::index::SharedStores::new(&db_path, 2).expect("shared stores");
    {
        let mut vs = stores.vector_store.write().await;
        let mut chunk = crate::chunker::Chunk::new(
            content.to_string(),
            0,
            4,
            kind,
            rel_path.to_string(),
        );
        chunk.signature = signature.map(str::to_string);
        vs.insert_chunks(vec![crate::embed::EmbeddedChunk::new(
            chunk,
            vec![0.25, 0.75],
        )])
        .expect("insert chunk");
        vs.build_index().expect("build index");
    }
    {
        let mut fts = stores.fts_store.write().await;
        fts.add_chunk(0, content, rel_path, signature, &format!("{:?}", kind))
            .expect("fts doc");
        fts.commit().expect("fts commit");
    }
    drop(stores);
}

/// Two-repo uvz-shaped fixture where both repos hold exactly one chunk at the
/// SAME id 0: `accounting-operations` (alphabetically first — the thief before
/// the fix) has a prose comment mentioning the symbol, `bankruptcy` holds the
/// real Java interface definition.
async fn colliding_group_fixture() -> (tempfile::TempDir, crate::mcp::CodesearchService) {
    let (tmp, _state, service) = colliding_group_fixture_with_state().await;
    (tmp, service)
}

/// Same fixture, keeping the `ServeState` handle so tests can reach the very
/// `SharedStores` instances routing opens (`get_opened_stores`) and hold
/// their vector write guards — a fresh `SharedStores::new` would build other
/// RwLocks and the held guard would gate nothing.
async fn colliding_group_fixture_with_state()
-> (tempfile::TempDir, std::sync::Arc<ServeState>, crate::mcp::CodesearchService) {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("accounting-operations");
    let root_b = tmp.path().join("bankruptcy");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    seed_collision_repo(
        &root_a,
        "src/hooks/useBankruptcyFolder.ts",
        crate::chunker::ChunkKind::Comment,
        None,
        "// TODO migrate this hook to BankruptcyFolderService from the bankruptcy service",
    )
    .await;
    seed_collision_repo(
        &root_b,
        "src/main/java/ru/sberbank/bankruptcy/BankruptcyFolderService.java",
        crate::chunker::ChunkKind::Interface,
        Some("public interface BankruptcyFolderService"),
        "public interface BankruptcyFolderService { BankruptcyFolder folderFor(String inn); }",
    )
    .await;

    let mut config = ReposConfig::default();
    config
        .register_with_alias(root_a.clone(), Some("accounting-operations".to_string()))
        .unwrap();
    config
        .register_with_alias(root_b.clone(), Some("bankruptcy".to_string()))
        .unwrap();
    config.groups.insert(
        "uvz".to_string(),
        vec![
            "accounting-operations".to_string(),
            "bankruptcy".to_string(),
        ],
    );
    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();
    let state = std::sync::Arc::new(ServeState::new(config, Some(config_file)));
    let service = crate::mcp::CodesearchService::new_for_serve(state.clone()).unwrap();
    (tmp, state, service)
}

fn tool_text(res: &rmcp::model::CallToolResult) -> String {
    match res.content.first() {
        Some(rmcp::model::ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

#[tokio::test]
async fn group_find_definition_attributes_hit_to_origin_repo_on_id_collision() {
    use rmcp::handler::server::wrapper::Parameters;

    let (_tmp, service) = colliding_group_fixture().await;

    let request = crate::mcp::types::FindRequest {
        kind: Some("definition".to_string()),
        symbol: "BankruptcyFolderService".to_string(),
        definition_kind: None,
        limit: Some(10),
        project: None,
        group: Some("uvz".to_string()),
    };
    let res = service
        .find(Parameters(request))
        .await
        .expect("group find must succeed");
    let text = tool_text(&res);

    // Before the fix this was the empty-result arm ("No definition found"):
    // both hits resolved in accounting-operations' comment chunk, failed the
    // DEFINITION_KINDS filter, and the true interface never surfaced.
    let parsed: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("non-JSON answer: {text}"));
    let items = parsed
        .as_array()
        .unwrap_or_else(|| panic!("expected a bare result array, got: {text}"));
    assert_eq!(items.len(), 1, "exactly the interface definition, got: {text}");
    assert_eq!(
        items[0]["path"],
        "bankruptcy/src/main/java/ru/sberbank/bankruptcy/BankruptcyFolderService.java",
        "the hit must carry its origin repo's alias prefix: {text}"
    );
    assert_eq!(items[0]["kind"], "Interface", "got: {text}");
    assert_eq!(
        items[0]["line"], 1,
        "responses must emit 1-based editor lines (chunk starts at 0-based line 0): {text}"
    );
}

/// Group-scoped get_chunk must read surrounding lines from the OWNING repo's
/// root. Stored chunk paths are project-relative, and the old context read
/// joined them onto the serve process's own project_path after alias
/// prefixing — a path that never exists — so every group answer came back
/// with the "source file not readable" note and zero context. Here chunk id 5
/// lives only in accounting-operations (bankruptcy holds just id 0), so the
/// smart-candidate detection routes to the unique owner.
#[tokio::test]
async fn group_get_chunk_resolves_context_against_the_owning_repo_root() {
    use rmcp::handler::server::wrapper::Parameters;

    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("accounting-operations");
    let root_b = tmp.path().join("bankruptcy");
    std::fs::create_dir_all(root_a.join("src/service")).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    // The real source file the context read has to find.
    std::fs::write(
        root_a.join("src/service/TargetService.java"),
        "line one\nline two\nTARGET LINE THREE\nTARGET LINE FOUR\nTARGET LINE FIVE\nline six\nline seven\nline eight\n",
    )
    .unwrap();

    // Repo A: five filler chunks (ids 0..=4) then the target at id 5,
    // spanning 0-based lines [2, 4] of the file above.
    let db_a = root_a.join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_a).unwrap();
    std::fs::write(
        db_a.join("metadata.json"),
        r#"{"schema_version":1,"dimensions":2,"model_short_name":"minilm-l6-q"}"#,
    )
    .unwrap();
    let stores = crate::index::SharedStores::new(&db_a, 2).expect("shared stores");
    {
        let mut vs = stores.vector_store.write().await;
        for i in 0..5u32 {
            vs.insert_chunks(vec![crate::embed::EmbeddedChunk::new(
                crate::chunker::Chunk::new(
                    format!("filler {i}"),
                    0,
                    0,
                    crate::chunker::ChunkKind::Comment,
                    "src/Filler.java".to_string(),
                ),
                vec![0.25, 0.75],
            )])
            .expect("insert filler");
        }
        let mut target = crate::chunker::Chunk::new(
            "TARGET LINE THREE\nTARGET LINE FOUR\nTARGET LINE FIVE".to_string(),
            2,
            4,
            crate::chunker::ChunkKind::Function,
            "src/service/TargetService.java".to_string(),
        );
        target.signature = Some("fn target()".to_string());
        vs.insert_chunks(vec![crate::embed::EmbeddedChunk::new(target, vec![0.5, 0.5])])
            .expect("insert target");
        vs.build_index().expect("build index");
    }
    {
        let mut fts = stores.fts_store.write().await;
        fts.add_chunk(
            5,
            "target",
            "src/service/TargetService.java",
            Some("fn target()"),
            "Function",
        )
        .expect("fts doc");
        fts.commit().expect("fts commit");
    }
    drop(stores);

    // Repo B holds only id 0, so chunk id 5 is unique to repo A.
    seed_collision_repo(
        &root_b,
        "src/B.java",
        crate::chunker::ChunkKind::Comment,
        None,
        "// b",
    )
    .await;

    let mut config = ReposConfig::default();
    config
        .register_with_alias(root_a.clone(), Some("accounting-operations".to_string()))
        .unwrap();
    config
        .register_with_alias(root_b.clone(), Some("bankruptcy".to_string()))
        .unwrap();
    config.groups.insert(
        "uvz".to_string(),
        vec![
            "accounting-operations".to_string(),
            "bankruptcy".to_string(),
        ],
    );
    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();
    let state = std::sync::Arc::new(ServeState::new(config, Some(config_file)));
    let service = crate::mcp::CodesearchService::new_for_serve(state).unwrap();

    let req = crate::mcp::types::GetChunkRequest {
        chunk_id: 5,
        chunk_ref: None,
        context_lines: Some(2),
        project: None,
        group: Some("uvz".to_string()),
    };
    let res = service
        .get_chunk(Parameters(req))
        .await
        .expect("group get_chunk must succeed");
    let text = tool_text(&res);
    let parsed: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("non-JSON answer: {text}"));

    assert_eq!(
        parsed["path"], "accounting-operations/src/service/TargetService.java",
        "got: {text}"
    );
    assert_eq!(
        parsed["context_before"], "line one\nline two",
        "context above must come from the owning repo's real file: {text}"
    );
    assert_eq!(
        parsed["context_after"], "TARGET LINE FIVE\nline six",
        "context below must come from the owning repo's real file: {text}"
    );
    assert!(
        parsed.get("note").is_none(),
        "the context read resolved — no fallback note expected, got: {text}"
    );
}

// ═══ Interactive fan-outs skip busy stores instead of queueing behind the
// write lock (review cluster B). The priming call opens the group's stores
// so `get_opened_stores` hands back the SAME SharedStores the handlers
// resolve; holding its vector write guard then reproduces an active indexing
// run. A regression to bounded waits wedges each test on the 300 s lock
// timeout instead of returning the skip-with-warning answer asserted here.

#[tokio::test]
async fn group_find_skips_a_write_locked_store_instead_of_waiting() {
    use rmcp::handler::server::wrapper::Parameters;

    let (_tmp, state, service) = colliding_group_fixture_with_state().await;

    // Prime routing so the group's stores are opened and lockable.
    let prime = crate::mcp::types::FindRequest {
        kind: Some("definition".to_string()),
        symbol: "BankruptcyFolderService".to_string(),
        definition_kind: None,
        limit: Some(10),
        project: None,
        group: Some("uvz".to_string()),
    };
    service
        .find(Parameters(prime))
        .await
        .expect("priming find must succeed");

    let alpha = state
        .get_opened_stores("accounting-operations")
        .expect("priming call must open accounting-operations");
    let _guard = alpha.vector_store.write().await;

    let request = crate::mcp::types::FindRequest {
        kind: Some("definition".to_string()),
        symbol: "BankruptcyFolderService".to_string(),
        definition_kind: None,
        limit: Some(10),
        project: None,
        group: Some("uvz".to_string()),
    };
    let res = service
        .find(Parameters(request))
        .await
        .expect("group find must succeed without waiting on the locked store");
    let text = tool_text(&res);

    // The healthy repo still answers; the busy one is named in `warnings`,
    // not silently folded into a plausible-looking short result list.
    let parsed: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("non-JSON answer: {text}"));
    let items = parsed
        .as_object()
        .and_then(|o| o.get("results"))
        .and_then(|r| r.as_array())
        .unwrap_or_else(|| panic!("expected a {{results, warnings}} object, got: {text}"));
    assert_eq!(
        items.len(),
        1,
        "only the bankruptcy definition must be reported, got: {text}"
    );
    assert_eq!(
        items[0]["path"],
        "bankruptcy/src/main/java/ru/sberbank/bankruptcy/BankruptcyFolderService.java",
        "got: {text}"
    );
    assert!(
        text.contains("store busy") && text.contains("accounting-operations"),
        "the skipped repo must be named with a store-busy warning, got: {text}"
    );
}

#[tokio::test]
async fn group_get_chunk_skips_a_write_locked_candidate_repo() {
    use rmcp::handler::server::wrapper::Parameters;

    let (_tmp, state, service) = colliding_group_fixture_with_state().await;

    // Prime routing so the group's stores are opened and lockable.
    let prime = crate::mcp::types::FindRequest {
        kind: Some("definition".to_string()),
        symbol: "BankruptcyFolderService".to_string(),
        definition_kind: None,
        limit: Some(10),
        project: None,
        group: Some("uvz".to_string()),
    };
    service
        .find(Parameters(prime))
        .await
        .expect("priming find must succeed");

    let alpha = state
        .get_opened_stores("accounting-operations")
        .expect("priming call must open accounting-operations");
    let _guard = alpha.vector_store.write().await;

    // Both repos hold id 0; with accounting-operations locked, the candidate
    // scan must skip it (not wait) and auto-route to bankruptcy alone.
    let req = crate::mcp::types::GetChunkRequest {
        chunk_id: 0,
        chunk_ref: None,
        context_lines: None,
        project: None,
        group: Some("uvz".to_string()),
    };
    let res = service
        .get_chunk(Parameters(req))
        .await
        .expect("group get_chunk must succeed without waiting on the locked store");
    let text = tool_text(&res);
    let parsed: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("non-JSON answer: {text}"));

    assert_eq!(
        parsed["path"],
        "bankruptcy/src/main/java/ru/sberbank/bankruptcy/BankruptcyFolderService.java",
        "the only answering candidate must own the answer, got: {text}"
    );
    assert!(
        text.contains("store busy") && text.contains("accounting-operations"),
        "the skipped candidate must be named with a store-busy warning, got: {text}"
    );
}

#[tokio::test]
async fn group_similar_skips_a_write_locked_store_instead_of_waiting() {
    use rmcp::handler::server::wrapper::Parameters;

    let (_tmp, state, service) = colliding_group_fixture_with_state().await;

    // Prime routing so the group's stores are opened and lockable; the
    // unprimed answer would be an id-0 ambiguity report, which is fine —
    // only the side effect (stores opened) matters here.
    let prime = crate::mcp::types::ExploreRequest {
        kind: Some("similar".to_string()),
        target: "0".to_string(),
        limit: Some(5),
        project: None,
        group: Some("uvz".to_string()),
    };
    service
        .explore(Parameters(prime))
        .await
        .expect("priming explore must succeed");

    let alpha = state
        .get_opened_stores("accounting-operations")
        .expect("priming call must open accounting-operations");
    let _guard = alpha.vector_store.write().await;

    // Covers BOTH converted fan-out sites of similar_chunks in one call:
    // the embedding lookup and the neighbour search both try-lock alpha.
    let req = crate::mcp::types::SimilarChunksRequest {
        chunk_id: 0,
        limit: Some(5),
        project: None,
        group: Some("uvz".to_string()),
    };
    let res = service
        .similar_chunks(Parameters(req))
        .await
        .expect("group similar must succeed without waiting on the locked store");
    let text = tool_text(&res);

    // The embedding resolves against bankruptcy alone (unique holder), and
    // its neighbour set is just the source chunk itself — filtered out — so
    // the answer is the empty message PLUS the store-busy warnings naming
    // the skipped repo.
    assert!(
        text.contains("No similar chunks found for chunk_id 0"),
        "expected the empty-result answer from the sole answering repo, got: {text}"
    );
    assert!(
        text.contains("store busy") && text.contains("accounting-operations"),
        "the skipped repo must be named with a store-busy warning, got: {text}"
    );
}

#[tokio::test]
async fn group_literal_search_attributes_each_hit_to_its_origin_repo() {
    use rmcp::handler::server::wrapper::Parameters;

    let (_tmp, service) = colliding_group_fixture().await;

    let request = crate::mcp::types::LiteralSearchRequest {
        query: "BankruptcyFolderService".to_string(),
        regex: None,
        phrase: None,
        limit: Some(10),
        file_glob: None,
        language: None,
        format: None,
        project: None,
        group: Some("uvz".to_string()),
    };
    let res = service
        .literal_search(Parameters(request))
        .await
        .expect("group literal search must succeed");
    let text = tool_text(&res);

    // Before the fix BOTH hits resolved against accounting-operations' chunk
    // (first store answering id 0): two identical hook paths, the Java file's
    // content lost. After the fix each hit resolves in its own repo and every
    // path carries its origin alias.
    let parsed: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("non-JSON answer: {text}"));
    let items = parsed["results"]
        .as_array()
        .unwrap_or_else(|| panic!("expected a results array, got: {text}"))
        .clone();
    assert_eq!(items.len(), 2, "both repos must answer, got: {text}");

    let mut paths: Vec<String> = items
        .iter()
        .map(|i| i["path"].as_str().unwrap_or_default().to_string())
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "accounting-operations/src/hooks/useBankruptcyFolder.ts".to_string(),
            "bankruptcy/src/main/java/ru/sberbank/bankruptcy/BankruptcyFolderService.java"
                .to_string(),
        ],
        "each hit must keep its own repo's path and alias prefix: {text}"
    );

    // Snippet attribution: the prefixed java hit must be the interface content,
    // not the accounting comment that stole it before the fix.
    let java = items
        .iter()
        .find(|i| i["path"].as_str().unwrap_or("").starts_with("bankruptcy/"))
        .expect("java hit present");
    assert!(
        java["snippet"]
            .as_str()
            .unwrap_or_default()
            .contains("public interface BankruptcyFolderService"),
        "the bankruptcy hit must carry bankruptcy's own content: {text}"
    );
}

/// Exact-AND literal search returns silence for multi-word queries whose
/// relevant chunks hold only MOST of the terms. The relaxed fallback must
/// surface those partial hits (>=60% significant-term coverage, >=2 terms)
/// instead of an empty answer, flag `relaxed_fallback` and explain itself
/// in the note — while single-identifier queries keep their honest refusal.
#[tokio::test]
async fn group_literal_search_relaxed_fallback_covers_partial_term_hits() {
    use rmcp::handler::server::wrapper::Parameters;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("partial");
    std::fs::create_dir_all(&root).unwrap();
    {
        let db_path = root.join(DB_DIR_NAME);
        std::fs::create_dir_all(&db_path).unwrap();
        std::fs::write(
            db_path.join("metadata.json"),
            r#"{"schema_version":1,"dimensions":2,"model_short_name":"minilm-l6-q"}"#,
        )
        .unwrap();
        let stores = crate::index::SharedStores::new(&db_path, 2).expect("shared stores");
        {
            let mut vs = stores.vector_store.write().await;
            let rich = crate::chunker::Chunk::new(
                "create package documents quickly".to_string(),
                0,
                3,
                crate::chunker::ChunkKind::Comment,
                "docs/create.md".to_string(),
            );
            let poor = crate::chunker::Chunk::new(
                "package".to_string(),
                0,
                1,
                crate::chunker::ChunkKind::Comment,
                "docs/other.md".to_string(),
            );
            vs.insert_chunks(vec![
                crate::embed::EmbeddedChunk::new(rich, vec![0.25, 0.75]),
                crate::embed::EmbeddedChunk::new(poor, vec![0.5, 0.5]),
            ])
            .expect("insert chunks");
            vs.build_index().expect("build index");
        }
        {
            let mut fts = stores.fts_store.write().await;
            fts.add_chunk(
                0,
                "create package documents quickly",
                "docs/create.md",
                None,
                "Comment",
            )
            .expect("fts doc");
            fts.add_chunk(1, "package", "docs/other.md", None, "Comment")
                .expect("fts doc");
            fts.commit().expect("fts commit");
        }
        drop(stores);
    }

    let mut config = ReposConfig::default();
    config
        .register_with_alias(root.clone(), Some("partial".to_string()))
        .unwrap();
    config.groups.insert("solo".to_string(), vec!["partial".to_string()]);
    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();
    let state = std::sync::Arc::new(ServeState::new(config, Some(config_file)));
    let service = crate::mcp::CodesearchService::new_for_serve(state).unwrap();

    // Four significant terms; no chunk holds all four, so exact AND is empty.
    // The rich chunk covers 3/4 (>=60%), the poor one 1/4 (<2 terms).
    let request = crate::mcp::types::LiteralSearchRequest {
        query: "create package documents signature".to_string(),
        regex: None,
        phrase: None,
        limit: Some(10),
        file_glob: None,
        language: None,
        format: None,
        project: None,
        group: Some("solo".to_string()),
    };
    let res = service
        .literal_search(Parameters(request))
        .await
        .expect("relaxed literal search must succeed");
    let text = tool_text(&res);
    let parsed: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("non-JSON answer: {text}"));
    let items = parsed["results"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("expected a results array, got: {text}"));
    assert_eq!(
        items.len(),
        1,
        "only the >=60%-coverage chunk survives the relaxed gate: {text}"
    );
    assert_eq!(items[0]["path"], "partial/docs/create.md", "got: {text}");
    assert_eq!(
        items[0]["start_line"], 1,
        "responses must emit 1-based editor lines (chunk starts at 0-based line 0): {text}"
    );
    assert_eq!(
        parsed["relaxed_fallback"],
        serde_json::json!(true),
        "the fallback must be visible to the caller: {text}"
    );
    assert!(
        parsed["note"].as_str().unwrap_or_default().contains("relaxed"),
        "the note must explain the mode: {text}"
    );

    // A single-identifier query never relaxes: the empty exact answer stays
    // an honest refusal, not a wall of partial matches.
    let request = crate::mcp::types::LiteralSearchRequest {
        query: "QzMissingUvzBusinessSymbol99Xy".to_string(),
        regex: None,
        phrase: None,
        limit: Some(10),
        file_glob: None,
        language: None,
        format: None,
        project: None,
        group: Some("solo".to_string()),
    };
    let res = service
        .literal_search(Parameters(request))
        .await
        .expect("refusal literal search must succeed");
    let text = tool_text(&res);
    let parsed: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("non-JSON answer: {text}"));
    assert!(
        parsed["results"].as_array().map(Vec::is_empty).unwrap_or(true),
        "an unknown identifier must stay empty: {text}"
    );
    assert!(
        parsed.get("relaxed_fallback").is_none(),
        "single-term queries must not trigger the fallback: {text}"
    );
}

/// `min_score` lets a caller turn the nearest-neighbour firehose into an
/// honest refusal: hits below the threshold are dropped and the empty
/// answer explains itself in `note`, instead of presenting irrelevant
/// neighbours as if they were matches. Lexical mode rides the same
/// `build_semantic_response` funnel as the embedding-backed modes, so it
/// exercises the threshold without needing a real embedding model.
#[tokio::test]
async fn group_semantic_search_min_score_refuses_instead_of_returning_noise() {
    use rmcp::handler::server::wrapper::Parameters;

    let (_tmp, service) = colliding_group_fixture().await;

    let make_request = |min_score: Option<f32>| crate::mcp::types::SearchRequest {
        query: "BankruptcyFolderService".to_string(),
        mode: None,
        compact: None,
        semantic_mode: Some("lexical".to_string()),
        filter_path: None,
        min_score,
        regex: None,
        phrase: None,
        file_glob: None,
        language: None,
        format: None,
        limit: Some(10),
        project: None,
        group: Some("uvz".to_string()),
    };

    // An unreachable threshold must refuse with an explanation, not return
    // the (weakly scored) candidates it found.
    let res = service
        .search(Parameters(make_request(Some(10_000.0))))
        .await
        .expect("min_score search must succeed");
    let text = tool_text(&res);
    let parsed: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("non-JSON answer: {text}"));
    assert!(
        parsed["results"].as_array().map(Vec::is_empty).unwrap_or(false),
        "an unreachable threshold must yield an explicit refusal: {text}"
    );
    assert_eq!(
        parsed["low_confidence"],
        serde_json::json!(true),
        "a refusal is a low-confidence outcome: {text}"
    );
    assert!(
        parsed["note"].as_str().unwrap_or_default().contains("min_score"),
        "the refusal must name the threshold so the caller can retune it: {text}"
    );

    // Without the threshold the same query must still answer with hits:
    // `min_score` adds a refusal path, never a silent filter.
    let res = service
        .search(Parameters(make_request(None)))
        .await
        .expect("unfiltered search must succeed");
    let text = tool_text(&res);
    let parsed: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("non-JSON answer: {text}"));
    assert!(
        !parsed["results"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .is_empty(),
        "the same query without a threshold must return its hits: {text}"
    );
}

/// Regression for the pilot's "ghost loss" of freshly registered aliases.
///
/// The debounced persist clones its snapshot and writes the file WITHOUT the
/// config lock, so a registration landing in that window used to exist only
/// in memory while the stale snapshot hit disk. Every reader-side
/// `reload_if_changed` (warmup / aliases / info / group resolve) then saw the
/// fresh mtime, trusted "the file changed", and swapped the stale disk copy
/// into memory — silently dropping the registration (alias 404s in /info, its
/// build ending with "repo removed or cancelled mid-index").
///
/// Two guards fix it: `persist_config` adopts its own write's mtime so an
/// internal persist never masquerades as an external edit, and
/// `run_persist_pass` re-verifies memory against the snapshot and rewrites
/// until disk converges.
#[tokio::test]
async fn own_persist_never_clobbers_a_racing_registration() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("alpha-repo");
    let root_b = tmp.path().join("beta-repo");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(root_a.clone(), Some("alpha".to_string()))
        .unwrap();
    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();
    let state = std::sync::Arc::new(ServeState::new(config, Some(config_file.clone())));

    // Warm the mtime cache exactly as production does after its first reader
    // passes (ServeState starts with a None mtime; this reload adopts it).
    let _ = state.aliases();

    // The debounce worker clones its snapshot...
    let stale = state.config.read().unwrap().clone();

    // ...and a registration lands + persists while the worker is mid-write.
    {
        let mut cfg = state.config.write().unwrap();
        cfg.register_with_alias(root_b.clone(), Some("beta".to_string()))
            .unwrap();
    }
    let current = state.config.read().unwrap().clone();
    state.persist_config(&current).unwrap();

    // The worker's stale snapshot finally hits the file — on disk, beta is gone.
    state.persist_config(&stale).unwrap();
    let disk = ReposConfig::load_from(&config_file).unwrap();
    assert!(
        !disk.repos.contains_key("beta"),
        "fixture premise: the stale snapshot must have won the disk race"
    );

    // Before the fix this reader-side reload saw the fresh mtime and clobbered
    // the in-memory registration with the disk copy.
    let aliases = state.aliases();
    assert!(
        aliases.iter().any(|a| a == "beta"),
        "the in-memory registration must survive the worker's stale write"
    );
    assert!(
        state.config_snapshot().repos.contains_key("beta"),
        "beta must still resolve from the live config"
    );

    // The divergence check converges disk back to memory.
    state.run_persist_pass().await;
    let disk = ReposConfig::load_from(&config_file).unwrap();
    assert!(
        disk.repos.contains_key("beta"),
        "run_persist_pass must rewrite the racing registration to disk"
    );
}
