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
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");
    let repo_path = tmp.path().join("somerepo");
    std::fs::create_dir(&repo_path).unwrap();
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::create_dir_all(&db_path).unwrap();
    // Put a file in the DB dir so delete has real work.
    std::fs::write(db_path.join("data.mdb"), "fake").unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("somerepo".to_string()))
        .unwrap();
    config.save_to(&config_file).unwrap();
    let state = ServeState::new(config, Some(config_file));

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
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");
    let repo_path = tmp.path().join("somerepo");
    std::fs::create_dir(&repo_path).unwrap();
    // db_path is a FILE, not a directory -> remove_dir_all fails every retry.
    let db_path = repo_path.join(DB_DIR_NAME);
    std::fs::write(&db_path, "not a directory").unwrap();

    let mut config = ReposConfig::default();
    config
        .register_with_alias(repo_path.clone(), Some("somerepo".to_string()))
        .unwrap();
    config.save_to(&config_file).unwrap();
    let state = ServeState::new(config, Some(config_file));

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
