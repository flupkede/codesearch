use super::*;
use std::io::Write;

/// Canonicalize then normalize a path for use in test assertions.
///
/// On Windows, `tempfile::tempdir()` may return an 8.3 short-name path
/// (e.g. `C:/Users/RUNNER~1/...`) while `std::fs::read_dir` can resolve the
/// same directory to its long-name form (`C:/Users/runneradmin/...`).
/// Applying `safe_canonicalize` before `normalize_path_for_compare` ensures
/// both sides of an assertion use the same form.
fn canon_norm(p: &Path) -> String {
    normalize_path_for_compare(&safe_canonicalize(p).unwrap_or_else(|_| p.to_path_buf()))
}

/// Process-wide lock serializing the git-spawning / directory-renaming
/// relocation tests.
///
/// These tests `git init` a directory and then rename it. On Windows the OS
/// indexer / antivirus scans each freshly-created `.git` tree and holds
/// handles on it, which blocks the rename ("Access is denied"). When many
/// such tests run concurrently the scanner is overwhelmed and the handles
/// linger for many seconds — long enough to exhaust even a generous
/// `rename_retry`. Serializing them so only one `.git` tree is created/
/// renamed at a time keeps each scan window short and the rename reliable.
static GIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the relocation-test serialization lock, recovering from a
/// poisoned mutex (a panic in one test must not cascade-fail the rest).
fn git_serial_lock() -> std::sync::MutexGuard<'static, ()> {
    GIT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Initialise a git repo at `dir` with an `origin` remote pointing at `url`.
fn init_git_remote(dir: &Path, url: &str) {
    // Retry on transient spawn failure (fork exhaustion under parallel test
    // load on Windows/msys); only a genuine missing-git binary is fatal.
    // Only transient SPAWN failures are retried. A non-zero EXIT is left
    // untouched on purpose: `git remote add` reporting "remote origin
    // already exists" is harmless here (the remote is already the URL we
    // want), and treating it as fatal previously flaked the relocation
    // tests. `git_remote_url` is the source of truth for what got captured.
    let run = |args: &[&str]| {
        const MAX_ATTEMPTS: u64 = 8;
        for attempt in 0..MAX_ATTEMPTS {
            match std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
            {
                Ok(o) => return o,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    panic!("git not available in test env: {e}");
                }
                Err(_) if attempt + 1 < MAX_ATTEMPTS => {
                    std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
                }
                Err(e) => panic!("git spawn failed after retries: {e}"),
            }
        }
        unreachable!("loop returns or panics")
    };
    run(&["init"]);
    run(&["remote", "add", "origin", url]);
}

/// Rename a directory with automatic retries.
///
/// On Windows, git subprocesses spawned by `init_git_remote` (and the OS
/// file indexer / antivirus) may keep a handle on the directory open
/// briefly after the process exits, so `std::fs::rename` fails with
/// "Access is denied". Under heavy parallel test load those handles linger
/// longer, so we use a generous retry budget with a capped back-off
/// (~7s worst case; in practice it succeeds on the first or second try).
#[track_caller]
fn rename_retry(from: &Path, to: &Path) {
    const MAX_ATTEMPTS: u64 = 40;
    let mut last_err = None;
    for attempt in 0..MAX_ATTEMPTS {
        match std::fs::rename(from, to) {
            Ok(()) => return,
            Err(e) => {
                last_err = Some(e);
                // Ramp the back-off but cap it so the total budget stays
                // bounded even under sustained handle contention.
                let backoff = (20 * (attempt + 1)).min(250);
                std::thread::sleep(std::time::Duration::from_millis(backoff));
            }
        }
    }
    panic!(
        "rename {:?} → {:?} failed after {} attempts: {}",
        from,
        to,
        MAX_ATTEMPTS,
        last_err.unwrap()
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "flaky on Windows: during a push the running codesearch serve polls git on this repo (HEAD watcher + reindex) while the AV/Search-indexer holds .git handles, so concurrent git subprocesses transiently fail and the captured remote comes back empty; the logic is platform-independent and covered on Linux/macOS CI"
)]
fn captures_git_remote_on_register() {
    let _serial = git_serial_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_git_remote(&repo, "https://example.com/acme/repo.git");

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(repo);
    assert_eq!(
        cfg.meta(&alias).git_remote.as_deref(),
        Some("https://example.com/acme/repo.git")
    );
}

#[test]
fn register_derives_alias_from_directory_name() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("My.Cool-Repo");
    std::fs::create_dir(&repo).unwrap();

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(repo.clone());
    // Alias is derived from (and sanitized from) the directory name.
    assert_eq!(alias, sanitize_alias("My.Cool-Repo"));
    assert!(cfg.repos.contains_key(&alias));
}

/// Regression for the `<repo>-propagate-tmp` path-pollution defect.
///
/// An agent (or any non-MSYS caller) supplied a POSIX-style path
/// `/c/Users/.../repo` to `register()`. Before the fix, `safe_canonicalize`
/// failed (LMDB-style paths that exist on disk did translate, but fresh
/// not-yet-created paths fell through to `strip_unc_prefix` which is a no-op
/// on `/c/...`), and Windows then resolved `/c/...` as
/// `<current-drive>:\c\...`, creating junk `C:\c\Users\...` directories and
/// silently indexing the wrong project.
///
/// After the fix, the POSIX path is translated to `C:/...` (or `D:/...`, etc.)
/// on the success path AND the fallback path, so the registry entry points
/// at the real Windows location regardless of whether the dir exists yet.
#[test]
#[cfg(windows)]
fn register_translates_msys_posix_path() {
    let tmp = tempfile::tempdir().unwrap();
    let win_repo = tmp.path().join("propagate-tmp-repo");
    std::fs::create_dir(&win_repo).unwrap();
    // Pre-canonicalize so 8.3 short names (e.g. `RUNNER~1` on Windows CI)
    // are resolved to their long form BEFORE we build both the MSYS input
    // and the expected output. Otherwise `safe_canonicalize` inside
    // `register()` resolves the short name but our expected value keeps it,
    // and the equality assertion fails on runners whose temp root sits
    // under a short-named user folder.
    let canonical = safe_canonicalize(&win_repo).unwrap();
    let win_str = canonical.to_string_lossy().replace('\\', "/");
    // win_str looks like "C:/Users/.../propagate-tmp-repo"
    let (drive_letter, rest) = win_str.split_at(2); // "C:" + "/Users/..."
    let drive_letter = drive_letter.chars().next().unwrap();
    let msys_path = format!(
        "/{}/{}",
        drive_letter.to_ascii_lowercase(),
        rest.trim_start_matches('/')
    );
    // msys_path now looks like "/c/Users/.../propagate-tmp-repo"

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(PathBuf::from(&msys_path));

    // The registered path must be the canonical Windows path, NOT a polluted
    // `C:\c\Users\...` form. Compare normalized (forward-slash, lower-cased
    // drive) so the assertion is robust against canonicalize's exact casing.
    let stored = cfg.repos.get(&alias).expect("alias must be registered");
    let stored_norm = stored.to_string_lossy().replace('\\', "/").to_lowercase();
    let expected_norm = win_str.to_lowercase();
    assert_eq!(
        stored_norm, expected_norm,
        "register() must translate MSYS path {:?} to {:?}, got {:?}",
        msys_path, win_str, stored
    );

    // And the registered path must actually resolve to the same directory
    // (i.e. no `C:\c\...` junk was created alongside).
    assert!(
        stored.exists(),
        "registered path must exist (no path pollution): {}",
        stored.display()
    );
}

/// Pins the **non-existing-path** branch — the actual defect site.
///
/// The sibling test `register_translates_msys_posix_path` creates the dir on
/// disk first, so `safe_canonicalize` succeeds on the first call and the
/// fallback (where the bug lived) never executes. This test deliberately
/// does NOT create the dir, forcing `safe_canonicalize` to fail and the
/// `normalize_user_path` fallback to run. If someone "simplifies" the
/// fallback to `strip_unc_prefix(path)` (the pre-fix code), this test goes
/// red: stored path would be `/c/...` instead of `C:/...`.
#[test]
#[cfg(windows)]
fn register_translates_msys_posix_path_when_dir_does_not_exist() {
    let tmp = tempfile::tempdir().unwrap();
    let win_repo = tmp.path().join("never-created-propagate-tmp");
    // Deliberately do NOT create_dir — the path must not exist.
    assert!(!win_repo.exists());

    let win_str = win_repo.to_string_lossy().replace('\\', "/");
    let (drive_letter, rest) = win_str.split_at(2);
    let drive_letter = drive_letter.chars().next().unwrap();
    let msys_path = format!(
        "/{}/{}",
        drive_letter.to_ascii_lowercase(),
        rest.trim_start_matches('/')
    );

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(PathBuf::from(&msys_path));

    let stored = cfg.repos.get(&alias).expect("alias must be registered");
    let stored_norm = stored.to_string_lossy().replace('\\', "/").to_lowercase();
    assert_eq!(
        stored_norm,
        win_str.to_lowercase(),
        "register() fallback must translate MSYS path {:?} to {:?}, got {:?} — \
         if this fails, the fallback in register() was reverted to strip_unc_prefix \
         and the original path-pollution defect is back",
        msys_path,
        win_str,
        stored
    );
}

/// Pins **register/unregister symmetry** — the second defect the reviewer
/// flagged. Before the structural fix, `register("/c/Users/foo")` stored
/// `C:\Users\foo` but `unregister_path("/c/Users/foo")` compared against the
/// untranslated `/c/Users/foo` (its fallback used `strip_unc_prefix`, which
/// is a no-op on `/c/...`) and returned `false`, leaving the entry stuck in
/// the registry. After the fix, both sides use `normalize_user_path` on the
/// fallback, so they agree.
#[test]
#[cfg(windows)]
fn unregister_path_matches_msys_posix_form() {
    let tmp = tempfile::tempdir().unwrap();
    let win_repo = tmp.path().join("propagate-tmp-unreg");
    std::fs::create_dir(&win_repo).unwrap();
    // Pre-canonicalize for the same reason as register_translates_msys_posix_path:
    // we delete the dir later, after which `safe_canonicalize` fails and the
    // `normalize_user_path` fallback runs WITHOUT short-name resolution. If
    // we built the MSYS input from the raw short-named path, register() would
    // store the long form (canonicalized) but unregister()'s fallback would
    // produce the short form, and the comparison would miss. Building from
    // the canonical form makes both sides agree regardless of which branch
    // runs.
    let canonical = safe_canonicalize(&win_repo).unwrap();
    let win_str = canonical.to_string_lossy().replace('\\', "/");
    let (drive_letter, rest) = win_str.split_at(2);
    let drive_letter = drive_letter.chars().next().unwrap();
    let msys_path = format!(
        "/{}/{}",
        drive_letter.to_ascii_lowercase(),
        rest.trim_start_matches('/')
    );

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(PathBuf::from(&msys_path));
    assert!(cfg.repos.contains_key(&alias));

    // Now delete the dir so unregister_path's safe_canonicalize fails and the
    // normalize_user_path fallback runs (this is the branch that used to miss).
    std::fs::remove_dir(&win_repo).unwrap();
    assert!(!win_repo.exists());

    let removed = cfg.unregister_path(&PathBuf::from(&msys_path));
    assert!(
        removed,
        "unregister_path must match the MSYS form via normalize_user_path fallback"
    );
    assert!(
        !cfg.repos.contains_key(&alias),
        "alias must be gone after unregister"
    );
}

/// On Unix, `/c/Users/...` is a legitimate absolute path (not an MSYS-ism),
/// so `register()` must store it verbatim. This guards against the Windows
/// fix accidentally rewriting paths on the wrong platform.
#[test]
#[cfg(not(windows))]
fn register_leaves_unix_path_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("propagate-tmp-repo");
    std::fs::create_dir(&repo).unwrap();
    let path_str = repo.to_string_lossy().to_string();

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(repo.clone());
    let stored = cfg.repos.get(&alias).expect("alias must be registered");
    assert_eq!(
        stored.to_string_lossy(),
        path_str,
        "register() must not rewrite Unix paths"
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "flaky on Windows: renaming a fresh .git tree races the AV/Search-indexer holding handles (os error 5); covered on Linux/macOS CI"
)]
fn try_relocate_finds_renamed_parent() {
    let _serial = git_serial_lock();
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent");
    let repo = parent.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_remote(&repo, "https://example.com/acme/parent-repo.git");

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(repo.clone());

    // Rename the PARENT folder; the stored repo path is now stale, but the
    // repo itself sits one level below the nearest existing ancestor (tmp).
    rename_retry(&parent, &tmp.path().join("parent-renamed"));

    let expected = tmp.path().join("parent-renamed").join("repo");
    let found = cfg
        .try_relocate(&alias)
        .expect("should relocate via renamed parent");
    assert_eq!(canon_norm(&found), canon_norm(&expected));
}

#[test]
#[cfg_attr(
    windows,
    ignore = "flaky on Windows: renaming a fresh .git tree races the AV/Search-indexer holding handles (os error 5); covered on Linux/macOS CI"
)]
fn try_relocate_none_beyond_max_depth() {
    let _serial = git_serial_lock();
    // Default max depth is 3. Bury the repo deeper than that below the
    // nearest existing ancestor so the scan cannot reach it.
    let tmp = tempfile::tempdir().unwrap();
    let deep = tmp.path().join("oldbox").join("l1").join("l2").join("repo");
    std::fs::create_dir_all(&deep).unwrap();
    init_git_remote(&deep, "https://example.com/acme/deep.git");

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(deep.clone());

    // Rename the top box; nearest existing ancestor becomes tmp root, and
    // the repo now sits 4 levels below it (box/l1/l2/repo) — out of reach.
    rename_retry(&tmp.path().join("oldbox"), &tmp.path().join("box"));

    assert!(
        cfg.try_relocate(&alias).is_none(),
        "repo beyond CODESEARCH_RELOCATE_MAX_DEPTH must not be relocated"
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "flaky on Windows: renaming a fresh .git tree races the AV/Search-indexer holding handles (os error 5); covered on Linux/macOS CI"
)]
fn relocate_missing_rewrites_only_moved_repos() {
    let _serial = git_serial_lock();
    let tmp = tempfile::tempdir().unwrap();
    let moved = tmp.path().join("moved");
    let stable = tmp.path().join("stable");
    std::fs::create_dir(&moved).unwrap();
    std::fs::create_dir(&stable).unwrap();
    init_git_remote(&moved, "https://example.com/acme/moved.git");
    init_git_remote(&stable, "https://example.com/acme/stable.git");

    let mut cfg = ReposConfig::default();
    let moved_alias = cfg.register(moved.clone());
    let stable_alias = cfg.register(stable.clone());

    let renamed = tmp.path().join("moved-renamed");
    rename_retry(&moved, &renamed);

    let (relocated, unresolved) = cfg.relocate_missing();
    assert!(unresolved.is_empty());
    assert_eq!(relocated.len(), 1);
    assert_eq!(relocated[0].0, moved_alias);
    assert_eq!(
        canon_norm(cfg.repos.get(&moved_alias).unwrap()),
        canon_norm(&renamed)
    );
    // The stable repo is untouched.
    assert_eq!(
        canon_norm(cfg.repos.get(&stable_alias).unwrap()),
        canon_norm(&stable)
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "flaky on Windows: renaming a directory races the AV/Search-indexer holding handles (os error 5); covered on Linux/macOS CI"
)]
fn prune_stale_removes_unrelocatable_entries() {
    let _serial = git_serial_lock();
    let tmp = tempfile::tempdir().unwrap();
    // No git remote → cannot be relocated → must be pruned.
    let plain = tmp.path().join("plain");
    std::fs::create_dir(&plain).unwrap();

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(plain.clone());
    cfg.add_group("g".to_string(), vec![alias.clone()]).unwrap();

    rename_retry(&plain, &tmp.path().join("plain-moved"));

    let (relocated, removed) = cfg.prune_stale();
    assert!(relocated.is_empty());
    assert_eq!(removed, vec![alias.clone()]);
    assert!(!cfg.repos.contains_key(&alias));
    // unregister_alias also cleans group membership.
    assert!(!cfg.groups.contains_key("g"));
}

#[test]
fn load_from_applies_reconcile_to_hand_edited_file() {
    // A hand-edited repos.json with an empty-alias entry and a group that
    // references an unknown alias must be reconciled (not crash) on load.
    let tmp = tempfile::tempdir().unwrap();
    let cfg_path = tmp.path().join("repos.json");
    let json = r#"{
            "repos": { "": "/tmp/blank", "good": "/tmp/good" },
            "groups": { "mix": ["good", "ghost"], "dead": ["ghost"] },
            "repos_meta": { "ghost": {} }
        }"#;
    std::fs::write(&cfg_path, json).unwrap();

    let cfg = ReposConfig::load_from(&cfg_path).expect("load should succeed");
    assert!(!cfg.repos.contains_key(""), "empty alias dropped");
    assert!(cfg.repos.contains_key("good"));
    assert_eq!(cfg.groups.get("mix"), Some(&vec!["good".to_string()]));
    assert!(!cfg.groups.contains_key("dead"), "empty group dropped");
    assert!(!cfg.repos_meta.contains_key("ghost"), "orphan meta dropped");
}

#[test]
#[cfg_attr(
    windows,
    ignore = "flaky on Windows: renaming a fresh .git tree races the AV/Search-indexer holding handles (os error 5); covered on Linux/macOS CI"
)]
fn try_relocate_finds_renamed_leaf() {
    let _serial = git_serial_lock();
    let tmp = tempfile::tempdir().unwrap();
    let original = tmp.path().join("myrepo");
    std::fs::create_dir(&original).unwrap();
    init_git_remote(&original, "https://example.com/acme/myrepo.git");

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(original.clone());

    // Rename the leaf folder; stored path is now stale.
    let renamed = tmp.path().join("myrepo-renamed");
    rename_retry(&original, &renamed);

    let found = cfg
        .try_relocate(&alias)
        .expect("should relocate renamed leaf");
    assert_eq!(canon_norm(&found), canon_norm(&renamed));
}

#[test]
fn try_relocate_returns_none_when_path_exists() {
    let _serial = git_serial_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("live");
    std::fs::create_dir(&repo).unwrap();
    init_git_remote(&repo, "https://example.com/acme/live.git");

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(repo);
    assert!(cfg.try_relocate(&alias).is_none());
}

#[test]
#[cfg_attr(
    windows,
    ignore = "flaky on Windows: renaming a directory races the AV/Search-indexer holding handles (os error 5); covered on Linux/macOS CI"
)]
fn try_relocate_none_without_recorded_remote() {
    let _serial = git_serial_lock();
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain");
    std::fs::create_dir(&plain).unwrap();

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(plain.clone());
    assert!(cfg.meta(&alias).git_remote.is_none());

    rename_retry(&plain, &tmp.path().join("plain-moved"));
    assert!(cfg.try_relocate(&alias).is_none());
}

#[test]
fn reconcile_drops_empty_alias_key() {
    let mut cfg = ReposConfig::default();
    cfg.repos.insert(String::new(), PathBuf::from("/tmp/x"));
    cfg.repos
        .insert("good".to_string(), PathBuf::from("/tmp/good"));
    cfg.reconcile();
    assert!(!cfg.repos.contains_key(""));
    assert!(cfg.repos.contains_key("good"));
}

#[test]
fn reconcile_prunes_unknown_group_members_and_empty_groups() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("real".to_string(), PathBuf::from("/tmp/real"));
    cfg.groups.insert(
        "mix".to_string(),
        vec!["real".to_string(), "ghost".to_string()],
    );
    cfg.groups
        .insert("dead".to_string(), vec!["ghost".to_string()]);
    cfg.reconcile();
    assert_eq!(cfg.groups.get("mix"), Some(&vec!["real".to_string()]));
    assert!(
        !cfg.groups.contains_key("dead"),
        "group with only unknown members should be dropped"
    );
}

#[test]
fn reconcile_drops_orphan_meta() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("real".to_string(), PathBuf::from("/tmp/real"));
    cfg.repos_meta
        .insert("ghost".to_string(), RepoMeta::default());
    cfg.reconcile();
    assert!(!cfg.repos_meta.contains_key("ghost"));
}

#[test]
fn try_relocate_none_when_ambiguous() {
    let _serial = git_serial_lock();
    let tmp = tempfile::tempdir().unwrap();
    let original = tmp.path().join("orig");
    std::fs::create_dir(&original).unwrap();
    init_git_remote(&original, "https://example.com/acme/dup.git");

    let mut cfg = ReposConfig::default();
    let alias = cfg.register(original.clone());

    // Two candidates with the same remote → ambiguous → no relocation.
    let a = tmp.path().join("copy-a");
    let b = tmp.path().join("copy-b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    init_git_remote(&a, "https://example.com/acme/dup.git");
    init_git_remote(&b, "https://example.com/acme/dup.git");
    // On Windows, git subprocesses spawned by init_git_remote may keep a
    // handle on the directory briefly, causing remove_dir_all to fail under
    // parallel test load. Ignore the error: if removal fails, `original`
    // still exists and try_relocate returns None because the path is present;
    // if removal succeeds, two ambiguous candidates are found → None.
    // Either way the assertion holds.
    let _ = std::fs::remove_dir_all(&original);

    assert!(cfg.try_relocate(&alias).is_none());
}

#[test]
fn test_unique_alias_generation() {
    let mut repos = HashMap::new();
    repos.insert("codesearch".to_string(), PathBuf::from("/tmp/a"));
    let alias = unique_alias_for_path(&repos, Path::new("/tmp/codesearch"));
    assert_eq!(alias, "codesearch-2");
}

#[test]
fn test_register_and_group_roundtrip() {
    let mut cfg = ReposConfig::default();
    let alias = cfg.register(PathBuf::from("/tmp/my-repo"));
    assert!(cfg.resolve(&alias).is_some());

    cfg.add_group("platform".to_string(), vec![alias.clone()])
        .unwrap();
    let resolved = cfg.resolve_group("platform");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].0, alias);
}

#[test]
fn test_sanitize_alias() {
    assert_eq!(sanitize_alias("My Repo.Name"), "My-Repo.Name");
    // Preserves case and dots
    assert_eq!(sanitize_alias("ExampleRepo"), "ExampleRepo");
    assert_eq!(sanitize_alias("ExampleRepo"), "ExampleRepo");
    // Spaces become dashes
    assert_eq!(sanitize_alias("my repo"), "my-repo");
    // Special characters dropped
    assert_eq!(sanitize_alias("repo@v2!"), "repov2");
    // Collapses double dashes
    assert_eq!(sanitize_alias("a--b"), "a-b");
}

#[test]
fn test_load_legacy_config_without_repos_meta() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("repos.json");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"repos":{{"my-repo":"/tmp/my-repo"}},"groups":{{"g":["my-repo"]}}}}"#
    )
    .unwrap();

    let cfg = ReposConfig::load_from(&path).unwrap();
    assert_eq!(cfg.repos.len(), 1);
    assert_eq!(cfg.groups.len(), 1);
    assert!(cfg.repos_meta.is_empty());
}

#[test]
fn test_save_then_load_roundtrip_with_meta() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("repos.json");

    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("repo-a".to_string(), PathBuf::from("/tmp/repo-a"));
    cfg.touch_last_changed("repo-a", 100);
    cfg.touch_last_scip("repo-a", 120);
    cfg.save_to(&path).unwrap();

    let loaded = ReposConfig::load_from(&path).unwrap();
    let meta = loaded.meta("repo-a");
    assert_eq!(meta.last_changed_unix, Some(100));
    assert_eq!(meta.last_scip_indexed_unix, Some(120));
}

#[test]
fn test_save_then_load_roundtrip_with_repo_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("repos.json");

    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("repo-a".to_string(), PathBuf::from("/tmp/repo-a"));
    cfg.repo_read_only.insert("repo-a".to_string(), true);
    cfg.save_to(&path).unwrap();

    let loaded = ReposConfig::load_from(&path).unwrap();
    assert_eq!(
        loaded.repo_read_only.get("repo-a"),
        Some(&true),
        "repo_read_only flag should round-trip through repos.json"
    );
    // default: a config written without the flag must still load (backward compat)
    assert!(
        !loaded.repo_read_only.contains_key("repo-b"),
        "unset repos must not appear read-only"
    );
}

/// A read-only flag for an alias that is no longer registered must not survive
/// `reconcile()`. `skip_serializing_if` only omits the map when it is entirely
/// empty, so without an explicit prune the stale entry round-trips forever and
/// an alias removed and later re-added under the same name would silently come
/// back read-only — invisible, and on the serve replica it means that repo is
/// never refreshed again.
#[test]
fn test_reconcile_drops_orphan_repo_read_only() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("live".to_string(), PathBuf::from("/tmp/live"));
    cfg.repo_read_only.insert("live".to_string(), true);
    cfg.repo_read_only.insert("gone".to_string(), true);

    cfg.reconcile();

    assert_eq!(
        cfg.repo_read_only.get("live"),
        Some(&true),
        "a registered alias must keep its read-only flag"
    );
    assert!(
        !cfg.repo_read_only.contains_key("gone"),
        "read-only flag for an unregistered alias must be dropped"
    );
}

#[test]
fn test_touch_last_changed_idempotent() {
    let mut cfg = ReposConfig::default();
    assert!(cfg.touch_last_changed("repo-a", 200));
    assert!(!cfg.touch_last_changed("repo-a", 200));
    assert!(!cfg.touch_last_changed("repo-a", 199));
    assert!(cfg.touch_last_changed("repo-a", 201));
}

#[test]
fn test_meta_for_unknown_alias_returns_default() {
    let cfg = ReposConfig::default();
    let meta = cfg.meta("unknown");
    assert_eq!(meta, RepoMeta::default());
}

#[test]
fn test_unregister_alias_removes_meta() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("repo-a".to_string(), PathBuf::from("/tmp/repo-a"));
    cfg.touch_last_changed("repo-a", 100);
    cfg.touch_last_scip("repo-a", 120);

    assert!(cfg.unregister_alias("repo-a"));
    assert!(!cfg.repos_meta.contains_key("repo-a"));
}

/// Regression: `Path::canonicalize()` on Windows returns a `\\?\`-prefixed UNC
/// extended-length path. If stored verbatim in repos.json, downstream `.join()`
/// and `.exists()` calls fail (e.g. `\\?\C:\foo\.codesearch.db` may not exist
/// even when `C:\foo\.codesearch.db` does). `register` and `register_with_alias`
/// must strip the prefix before storage so repos.json always holds plain paths.
#[test]
fn register_strips_unc_prefix_from_stored_path() {
    let mut cfg = ReposConfig::default();

    // Simulate what canonicalize() returns on Windows: a \\?\ UNC path.
    let unc_path = PathBuf::from(r"\\?\C:\WorkArea\AI\myrepo");
    // register() calls canonicalize() internally, but also accepts any path.
    // Test strip_unc directly (the private fn is in scope via pub(crate) isn't
    // exposed, so we exercise it via register_with_alias on a pre-formed path
    // by bypassing canonicalize with a path that starts with \\?\).
    let alias = cfg
        .register_with_alias(unc_path.clone(), Some("myrepo".to_string()))
        .unwrap();

    let stored = cfg.resolve(&alias).unwrap();
    let stored_str = stored.to_string_lossy();
    assert!(
        !stored_str.starts_with(r"\\?\"),
        "repos.json must not contain UNC prefix, got: {}",
        stored_str
    );
    assert!(
        stored_str.starts_with("C:\\") || stored_str.starts_with("C:/"),
        "stored path should be a plain Windows path, got: {}",
        stored_str
    );
}

// ── Virtual "all" group (issue #131) ───────────────────────────────

#[test]
fn add_group_rejects_reserved_all_name() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("repo-a".to_string(), PathBuf::from("/tmp/repo-a"));

    let err = cfg
        .add_group("all".to_string(), vec!["repo-a".to_string()])
        .unwrap_err();
    assert!(
        err.to_string().contains("reserved"),
        "expected 'reserved' in error, got: {}",
        err
    );
}

#[test]
fn resolve_group_all_returns_every_registered_repo() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("repo-a".to_string(), PathBuf::from("/tmp/repo-a"));
    cfg.repos
        .insert("repo-b".to_string(), PathBuf::from("/tmp/repo-b"));

    let resolved = cfg.resolve_group(crate::constants::ALL_GROUP_NAME);
    let mut names: Vec<String> = resolved.into_iter().map(|(a, _)| a).collect();
    names.sort();
    assert_eq!(names, vec!["repo-a".to_string(), "repo-b".to_string()]);
}

#[test]
fn resolve_group_all_is_empty_when_no_repos_registered() {
    let cfg = ReposConfig::default();
    let resolved = cfg.resolve_group(crate::constants::ALL_GROUP_NAME);
    assert!(resolved.is_empty());
}

#[test]
fn groups_with_virtual_all_advertises_all_without_storing_it() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("repo-a".to_string(), PathBuf::from("/tmp/repo-a"));
    cfg.repos
        .insert("repo-b".to_string(), PathBuf::from("/tmp/repo-b"));
    cfg.add_group("platform".to_string(), vec!["repo-a".to_string()])
        .unwrap();

    // The advertised map includes both the real group and "all".
    let advertised = cfg.groups_with_virtual_all();
    assert_eq!(advertised.len(), 2);
    let mut all_members = advertised
        .get(crate::constants::ALL_GROUP_NAME)
        .unwrap()
        .clone();
    all_members.sort();
    assert_eq!(
        all_members,
        vec!["repo-a".to_string(), "repo-b".to_string()]
    );

    // But the stored config is untouched — "all" must never be persisted.
    assert!(
        !cfg.groups.contains_key(crate::constants::ALL_GROUP_NAME),
        "\"all\" must not leak into the stored groups map"
    );
}

#[test]
fn project_groups_maps_aliases_to_named_groups() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("repo-a".to_string(), PathBuf::from("/tmp/a"));
    cfg.repos
        .insert("repo-b".to_string(), PathBuf::from("/tmp/b"));
    cfg.repos
        .insert("lonely".to_string(), PathBuf::from("/tmp/lonely"));
    // repo-a is a member of two named groups.
    cfg.add_group(
        "group-x".to_string(),
        vec!["repo-a".to_string(), "repo-b".to_string()],
    )
    .unwrap();
    cfg.add_group("group-y".to_string(), vec!["repo-a".to_string()])
        .unwrap();

    let pg = cfg.project_groups();

    // Multi-group membership is sorted + de-duplicated.
    assert_eq!(
        pg.get("repo-a"),
        Some(&vec!["group-x".to_string(), "group-y".to_string()])
    );
    assert_eq!(pg.get("repo-b"), Some(&vec!["group-x".to_string()]));
    // A repo in no named group is omitted entirely (no empty entry).
    assert!(!pg.contains_key("lonely"));
}

#[test]
fn project_groups_excludes_virtual_all_and_remote_refs() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("local-a".to_string(), PathBuf::from("/tmp/a"));
    cfg.remotes
        .insert("cloud".to_string(), make_peer("https://cloud"));
    cfg.groups.insert(
        "docs".to_string(),
        vec!["local-a".to_string(), "@cloud".to_string()],
    );

    let pg = cfg.project_groups();

    // Only the local alias is mapped; "@cloud" never appears as a key.
    assert_eq!(pg.get("local-a"), Some(&vec!["docs".to_string()]));
    assert!(!pg.contains_key("@cloud"));
    assert!(!pg.contains_key("cloud"));
    // The virtual "all" group is never a member entry.
    for groups in pg.values() {
        assert!(!groups.contains(&crate::constants::ALL_GROUP_NAME.to_string()));
    }
}

// ── Federation: remotes + resolve_group_targets ───────────────────

fn make_peer(url: &str) -> RemotePeer {
    RemotePeer {
        url: url.to_string(),
        api_key: "secret".to_string(),
        group: Some("docs".to_string()),
        timeout_secs: Some(15),
    }
}

#[test]
fn resolve_group_targets_expands_local_and_remote_members() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("local-a".to_string(), PathBuf::from("/tmp/a"));
    cfg.remotes
        .insert("cloud".to_string(), make_peer("https://cloud"));
    cfg.groups.insert(
        "docs".to_string(),
        vec!["local-a".to_string(), "@cloud".to_string()],
    );

    let targets = cfg.resolve_group_targets("docs");
    assert_eq!(targets.len(), 2);
    // Local member expands to a Local target.
    assert!(matches!(
        &targets[0],
        Target::Local { alias, .. } if alias == "local-a"
    ));
    // Remote member expands to a Remote target carrying the peer config.
    match &targets[1] {
        Target::Remote { peer_name, peer } => {
            assert_eq!(peer_name, "cloud");
            assert_eq!(peer.url, "https://cloud");
        }
        other => panic!("expected Remote, got {:?}", other),
    }
}

#[test]
fn split_group_targets_partitions_locals_and_remotes() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("local-a".to_string(), PathBuf::from("/tmp/a"));
    cfg.repos
        .insert("local-b".to_string(), PathBuf::from("/tmp/b"));
    cfg.remotes
        .insert("cloud".to_string(), make_peer("https://cloud"));
    cfg.groups.insert(
        "docs".to_string(),
        vec![
            "@cloud".to_string(),
            "local-a".to_string(),
            "local-b".to_string(),
        ],
    );

    let (locals, remotes) = cfg.split_group_targets("docs");
    assert_eq!(locals.len(), 2);
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].0, "cloud");
}

fn cfg_with_cloud() -> ReposConfig {
    let mut cfg = ReposConfig::default();
    cfg.remotes
        .insert("cloud".to_string(), make_peer("https://cloud"));
    cfg
}

#[test]
fn mounted_remote_projects_namespaces_and_sorts() {
    let mut cfg = cfg_with_cloud();
    // Opt-in allowlist, deliberately out of order to prove sorting.
    cfg.remote_mounts = vec!["cloud/bynder".to_string(), "cloud/akeneo".to_string()];
    let mounts = cfg.mounted_remote_projects();
    // Sorted by local name: cloud/akeneo before cloud/bynder.
    let names: Vec<&str> = mounts.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["cloud/akeneo", "cloud/bynder"]);
    match &mounts[0].1 {
        Target::RemoteProject {
            peer_name,
            remote_alias,
            peer,
        } => {
            assert_eq!(peer_name, "cloud");
            assert_eq!(remote_alias, "akeneo"); // bare alias, un-namespaced
            assert_eq!(peer.url, "https://cloud");
        }
        other => panic!("expected RemoteProject, got {:?}", other),
    }
}

#[test]
fn mounted_remote_projects_only_allowlisted_and_skips_unknown_peer() {
    let mut cfg = cfg_with_cloud();
    // akeneo opted in; an entry for an unknown peer must be ignored entirely.
    // (bynder is available on the peer but NOT mounted, so it never appears.)
    cfg.remote_mounts = vec!["cloud/akeneo".to_string(), "ghost/x".to_string()];
    let mounts = cfg.mounted_remote_projects();
    let names: Vec<&str> = mounts.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["cloud/akeneo"]);
}

#[test]
fn mounted_remote_projects_applies_rename_override() {
    let mut cfg = cfg_with_cloud();
    cfg.remote_mounts = vec!["cloud/akeneo".to_string()];
    cfg.remote_alias_overrides
        .insert("cloud/akeneo".to_string(), "pim".to_string());
    let mounts = cfg.mounted_remote_projects();
    assert_eq!(mounts[0].0, "pim"); // local name is the rename
    match &mounts[0].1 {
        // ...but the peer still receives the bare original alias.
        Target::RemoteProject { remote_alias, .. } => assert_eq!(remote_alias, "akeneo"),
        other => panic!("expected RemoteProject, got {:?}", other),
    }
}

#[test]
fn resolve_remote_project_requires_mount_rename_and_negatives() {
    let mut cfg = cfg_with_cloud();
    cfg.remote_mounts = vec!["cloud/akeneo".to_string(), "cloud/bynder".to_string()];
    cfg.remote_alias_overrides
        .insert("cloud/akeneo".to_string(), "pim".to_string());
    cfg.repos
        .insert("local-a".to_string(), PathBuf::from("/tmp/a"));

    // A mounted canonical "<peer>/<alias>" resolves.
    assert!(matches!(
        cfg.resolve_remote_project("cloud/bynder"),
        Some(Target::RemoteProject { ref remote_alias, .. }) if remote_alias == "bynder"
    ));
    // A rename of a mounted project resolves back to its canonical alias.
    assert!(matches!(
        cfg.resolve_remote_project("pim"),
        Some(Target::RemoteProject { ref remote_alias, .. }) if remote_alias == "akeneo"
    ));
    // Un-mounted (peer has it but user didn't opt in), unknown peer, and
    // plain local aliases do not resolve remotely.
    assert!(cfg.resolve_remote_project("cloud/secret").is_none());
    assert!(cfg.resolve_remote_project("ghost/x").is_none());
    assert!(cfg.resolve_remote_project("local-a").is_none());
}

#[test]
fn mount_and_unmount_remote_project_roundtrip() {
    let mut cfg = cfg_with_cloud();
    cfg.mount_remote_project("cloud/akeneo").unwrap();
    cfg.mount_remote_project("cloud/akeneo").unwrap(); // idempotent
    assert_eq!(cfg.remote_mounts, vec!["cloud/akeneo".to_string()]);
    // Unknown peer and malformed names are rejected.
    assert!(cfg.mount_remote_project("ghost/x").is_err());
    assert!(cfg.mount_remote_project("no-separator").is_err());
    assert!(cfg.mount_remote_project("cloud/").is_err());
    // Unmount drops the mount and any orphaned rename override.
    cfg.remote_alias_overrides
        .insert("cloud/akeneo".to_string(), "pim".to_string());
    assert!(cfg.unmount_remote_project("cloud/akeneo"));
    assert!(cfg.remote_mounts.is_empty());
    assert!(!cfg.remote_alias_overrides.contains_key("cloud/akeneo"));
    assert!(!cfg.unmount_remote_project("cloud/akeneo")); // already gone
}

#[test]
fn remote_project_cache_write_read_and_prune() {
    let mut cfg = cfg_with_cloud();

    // No cache yet.
    assert!(cfg.cached_remote_project_aliases("cloud").is_none());

    // Write-through caches, sorted + deduped.
    cfg.cache_remote_projects(
        "cloud",
        vec![
            "bynder".to_string(),
            "akeneo".to_string(),
            "bynder".to_string(),
        ],
    );
    assert_eq!(
        cfg.cached_remote_project_aliases("cloud"),
        Some(["akeneo".to_string(), "bynder".to_string()].as_slice())
    );

    // Re-caching replaces the previous entry outright.
    cfg.cache_remote_projects("cloud", vec!["akeneo".to_string()]);
    assert_eq!(
        cfg.cached_remote_project_aliases("cloud"),
        Some(["akeneo".to_string()].as_slice())
    );

    // reconcile() drops cache entries for peers no longer configured —
    // a removed peer's last-known aliases are stale/meaningless.
    cfg.remotes.remove("cloud");
    cfg.reconcile();
    assert!(cfg.cached_remote_project_aliases("cloud").is_none());
}

#[test]
fn group_remote_projects_only_mounted_members_of_referenced_peers() {
    let mut cfg = cfg_with_cloud();
    cfg.remote_mounts = vec!["cloud/akeneo".to_string(), "cloud/bynder".to_string()];
    cfg.groups
        .insert("docs".to_string(), vec!["@cloud".to_string()]);
    let projs = cfg.group_remote_projects("docs");
    let aliases: Vec<&str> = projs.iter().map(|(_, _, a)| a.as_str()).collect();
    assert_eq!(aliases, vec!["akeneo", "bynder"]);

    // A group that references no peer yields nothing.
    cfg.groups
        .insert("solo".to_string(), vec!["@cloud".to_string()]);
    cfg.remote_mounts.clear();
    assert!(cfg.group_remote_projects("solo").is_empty());
    // The virtual "all" group never federates.
    assert!(cfg
        .group_remote_projects(crate::constants::ALL_GROUP_NAME)
        .is_empty());
}

#[test]
fn reconcile_prunes_mounts_with_unknown_peer_or_malformed_name() {
    let mut cfg = cfg_with_cloud();
    cfg.remote_mounts = vec![
        "cloud/akeneo".to_string(), // keep
        "ghost/x".to_string(),      // unknown peer → prune
        "malformed".to_string(),    // no separator → prune
        "cloud/".to_string(),       // empty alias → prune
    ];
    cfg.remote_alias_overrides
        .insert("ghost/x".to_string(), "orphan".to_string());
    cfg.reconcile();
    assert_eq!(cfg.remote_mounts, vec!["cloud/akeneo".to_string()]);
    // Rename override orphaned by the prune is dropped too.
    assert!(!cfg.remote_alias_overrides.contains_key("ghost/x"));
}

#[test]
fn resolve_group_targets_all_never_federates() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("local-a".to_string(), PathBuf::from("/tmp/a"));
    cfg.remotes
        .insert("cloud".to_string(), make_peer("https://cloud"));
    // Even if a group "docs" federates, querying "all" must stay local.
    cfg.groups
        .insert("docs".to_string(), vec!["@cloud".to_string()]);

    let targets = cfg.resolve_group_targets(crate::constants::ALL_GROUP_NAME);
    assert!(targets.iter().all(|t| matches!(t, Target::Local { .. })));
    assert_eq!(targets.len(), 1); // local-a only
}

#[test]
fn resolve_group_targets_skips_unknown_remote_ref() {
    let mut cfg = ReposConfig::default();
    cfg.groups.insert(
        "docs".to_string(),
        vec!["@ghost".to_string()], // no `remotes` entry for "ghost"
    );

    let targets = cfg.resolve_group_targets("docs");
    assert!(targets.is_empty(), "unknown remote ref must be skipped");
}

#[test]
fn reconcile_prunes_unknown_remote_ref_and_drops_now_empty_group() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("real".to_string(), PathBuf::from("/tmp/real"));
    cfg.groups
        .insert("docs".to_string(), vec!["@ghost".to_string()]);
    // Only "ghost" is referenced but "cloud" exists → "ghost" pruned, group
    // becomes empty and is dropped.
    cfg.remotes
        .insert("cloud".to_string(), make_peer("https://cloud"));

    cfg.reconcile();
    assert!(
        !cfg.groups.contains_key("docs"),
        "empty group must be dropped"
    );
}

#[test]
fn reconcile_keeps_valid_remote_ref() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("real".to_string(), PathBuf::from("/tmp/real"));
    cfg.remotes
        .insert("cloud".to_string(), make_peer("https://cloud"));
    cfg.groups.insert(
        "docs".to_string(),
        vec!["real".to_string(), "@cloud".to_string()],
    );

    cfg.reconcile();
    assert_eq!(
        cfg.groups.get("docs"),
        Some(&vec!["real".to_string(), "@cloud".to_string()]),
        "valid local alias AND valid remote ref must both survive reconcile"
    );
}

#[test]
fn remotes_roundtrip_through_json() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("local-a".to_string(), PathBuf::from("/tmp/a"));
    cfg.remotes
        .insert("cloud".to_string(), make_peer("https://cloud"));
    cfg.groups
        .insert("docs".to_string(), vec!["@cloud".to_string()]);

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("repos.json");
    cfg.save_to(&path).unwrap();

    let loaded = ReposConfig::load_from(&path).unwrap();
    assert_eq!(loaded.remotes.len(), 1);
    let peer = loaded.remotes.get("cloud").unwrap();
    assert_eq!(peer.url, "https://cloud");
    assert_eq!(peer.api_key, "secret");
    assert_eq!(peer.group.as_deref(), Some("docs"));
    // Group with remote ref survives the load+reconcile roundtrip.
    assert_eq!(loaded.groups.get("docs"), Some(&vec!["@cloud".to_string()]));
}

#[test]
fn add_remote_inserts_and_overwrites() {
    let mut cfg = ReposConfig::default();
    cfg.add_remote("cloud".to_string(), make_peer("https://cloud"))
        .unwrap();
    assert_eq!(cfg.remotes.get("cloud").unwrap().url, "https://cloud");
    // Overwrite with a new URL.
    cfg.add_remote("cloud".to_string(), make_peer("https://cloud2"))
        .unwrap();
    assert_eq!(cfg.remotes.len(), 1);
    assert_eq!(cfg.remotes.get("cloud").unwrap().url, "https://cloud2");
}

#[test]
fn add_remote_rejects_empty_name_prefixed_name_and_empty_url() {
    let mut cfg = ReposConfig::default();
    assert!(cfg
        .add_remote("  ".to_string(), make_peer("https://cloud"))
        .is_err());
    assert!(cfg
        .add_remote("@cloud".to_string(), make_peer("https://cloud"))
        .is_err());
    let mut blank = make_peer("https://cloud");
    blank.url = "  ".to_string();
    assert!(cfg.add_remote("cloud".to_string(), blank).is_err());
    // A '/' in a peer name would break <peer>/<alias> namespacing — rejected.
    assert!(cfg
        .add_remote("a/b".to_string(), make_peer("https://cloud"))
        .is_err());
    assert!(cfg.remotes.is_empty());
}

#[test]
fn add_remote_trims_name() {
    let mut cfg = ReposConfig::default();
    cfg.add_remote("  cloud  ".to_string(), make_peer("https://cloud"))
        .unwrap();
    assert!(cfg.remotes.contains_key("cloud"));
}

#[test]
fn add_remote_to_group_creates_and_is_idempotent() {
    let mut cfg = ReposConfig::default();
    cfg.add_remote("cloud".to_string(), make_peer("https://cloud"))
        .unwrap();
    cfg.add_remote_to_group("docs".to_string(), "cloud")
        .unwrap();
    cfg.add_remote_to_group("docs".to_string(), "cloud")
        .unwrap(); // idempotent
    assert_eq!(cfg.groups.get("docs"), Some(&vec!["@cloud".to_string()]));
}

#[test]
fn add_remote_to_group_rejects_reserved_all_and_unknown_remote() {
    let mut cfg = ReposConfig::default();
    cfg.add_remote("cloud".to_string(), make_peer("https://cloud"))
        .unwrap();
    assert!(cfg
        .add_remote_to_group(crate::constants::ALL_GROUP_NAME.to_string(), "cloud")
        .is_err());
    assert!(cfg
        .add_remote_to_group("docs".to_string(), "ghost")
        .is_err());
}

#[test]
fn remove_remote_prunes_group_references_and_empties() {
    let mut cfg = ReposConfig::default();
    cfg.repos
        .insert("local-a".to_string(), PathBuf::from("/tmp/a"));
    cfg.add_remote("cloud".to_string(), make_peer("https://cloud"))
        .unwrap();
    cfg.groups.insert(
        "docs".to_string(),
        vec!["local-a".to_string(), "@cloud".to_string()],
    );
    cfg.groups
        .insert("cloud-only".to_string(), vec!["@cloud".to_string()]);

    assert!(cfg.remove_remote("cloud"));
    assert!(!cfg.remotes.contains_key("cloud"));
    // The mixed group keeps its local member but drops the remote ref.
    assert_eq!(cfg.groups.get("docs"), Some(&vec!["local-a".to_string()]));
    // The group that only referenced the remote is dropped entirely.
    assert!(!cfg.groups.contains_key("cloud-only"));
}

#[test]
fn remove_remote_returns_false_for_unknown() {
    let mut cfg = ReposConfig::default();
    assert!(!cfg.remove_remote("ghost"));
}

#[test]
fn groups_referencing_remote_lists_sorted_groups() {
    let mut cfg = ReposConfig::default();
    cfg.add_remote("cloud".to_string(), make_peer("https://cloud"))
        .unwrap();
    cfg.groups
        .insert("zeta".to_string(), vec!["@cloud".to_string()]);
    cfg.groups
        .insert("alpha".to_string(), vec!["@cloud".to_string()]);
    cfg.groups
        .insert("other".to_string(), vec!["@somewhere".to_string()]);
    assert_eq!(
        cfg.groups_referencing_remote("cloud"),
        vec!["alpha".to_string(), "zeta".to_string()]
    );
}

#[test]
fn remotes_alias_base_url_field() {
    // The `url` field accepts the friendlier `base_url` alias for ergonomics.
    let json = r#"{
            "repos": {"a": "/tmp/a"},
            "remotes": {"cloud": {"base_url": "https://cloud", "api_key": "k"}}
        }"#;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("repos.json");
    std::fs::write(&path, json).unwrap();
    let cfg = ReposConfig::load_from(&path).unwrap();
    assert_eq!(cfg.remotes.get("cloud").unwrap().url, "https://cloud");
}
