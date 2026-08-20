//! LMDB environment tracking to prevent double-open panics.
//!
//! LMDB does not allow two `EnvOpenOptions::open()` handles on the same directory
//! in the same process with different options. Violating this causes runtime panics
//! and corrupted indexes.
//!
//! This module provides [`TrackedEnv`], a thin wrapper around `heed::Env` that
//! registers every open in a global `DashMap` and unregisters on Drop. If a
//! second open is attempted on the same canonical path, it returns a clear error
//! instead of a cryptic LMDB panic.

use anyhow::{Context, Result};
use dashmap::DashMap;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use crate::cache::safe_canonicalize;

// ── Baseline env flags ──────────────────────────────────────────

/// Flags every codesearch LMDB environment MUST be opened with.
///
/// `NO_TLS` is LMDB's `MDB_NOTLS`: it detaches read transactions from
/// thread-local storage. Without it LMDB hands out exactly ONE reader
/// lock-table slot per OS thread, so a second *concurrently live* read
/// transaction on the same thread fails with
/// `MDB_BAD_RSLOT: Invalid reuse of reader locktable slot`.
///
/// This is defensive hardening, not a fix for a known live call path. Every
/// current reader (`VectorStore::stats`, `::search`, …) opens and drops its own
/// `RoTxn` inside one function body, so no two are live at once today, and
/// `MDB_BAD_RSLOT` is per-environment so a group query across repos cannot
/// trigger it either. The flag is here because the failure is real, silent and
/// easy to reintroduce: it was reproduced against the production `inriver`
/// database simply by holding two read transactions at once, and nothing in the
/// type system stops a future refactor (e.g. reading stats while a search txn
/// is open) from doing exactly that.
///
/// Because heed refuses to reopen the same path with different options, this
/// must be applied at EVERY env-open site, not only the read-only one — a
/// partial rollout would turn a working reopen into an intermittent failure.
pub const BASE_ENV_FLAGS: heed::EnvFlags = heed::EnvFlags::NO_TLS;

// ── Global registry ─────────────────────────────────────────────

static LMDB_REGISTRY: OnceLock<DashMap<PathBuf, LmdbEntry>> = OnceLock::new();

#[derive(Debug)]
struct LmdbEntry {
    description: String,
    opened_at: Instant,
}

fn register(path: &Path, description: &str) -> Result<PathBuf> {
    let registry = LMDB_REGISTRY.get_or_init(DashMap::new);
    let canonical = safe_canonicalize(path)
        .with_context(|| format!("Cannot canonicalize LMDB path: {}", path.display()))?;

    // Use DashMap's atomic entry API to prevent TOCTOU race between check+insert.
    use dashmap::mapref::entry::Entry;
    match registry.entry(canonical.clone()) {
        Entry::Occupied(existing) => {
            let entry = existing.get();
            anyhow::bail!(
                "LMDB double-open prevented: {} is already open ({}, opened {:.1}s ago)",
                canonical.display(),
                entry.description,
                entry.opened_at.elapsed().as_secs_f64()
            );
        }
        Entry::Vacant(slot) => {
            slot.insert(LmdbEntry {
                description: description.to_string(),
                opened_at: Instant::now(),
            });
        }
    }

    Ok(canonical)
}

fn unregister(canonical: &Path) {
    if let Some(registry) = LMDB_REGISTRY.get() {
        registry.remove(canonical);
    }
}

/// Check whether an LMDB environment at `path` is currently registered as open
/// in this process — without attempting to open it.
///
/// Returns `false` if the registry is uninitialized, the path cannot be
/// canonicalized, or no live [`TrackedEnv`] holds the canonical path. Returns
/// `true` if a `TrackedEnv` for the canonical path is currently alive.
///
/// Use this to avoid a doomed second [`TrackedEnv::open`] when the path is
/// known to be held by another component in the same process — e.g. the serve
/// process holds the embedding cache via `EmbeddingService` while `doctor` runs
/// in-process via the TUI HTTP handler. Calling `open` anyway would trip the
/// double-open guard; `is_open` lets the caller fall back to file-based stats.
pub fn is_open(path: &Path) -> bool {
    match LMDB_REGISTRY.get() {
        Some(registry) => match safe_canonicalize(path) {
            Ok(canonical) => registry.contains_key(&canonical),
            Err(_) => false,
        },
        None => false,
    }
}

/// Descriptions of every live [`TrackedEnv`] whose canonical path is `path`
/// itself or lives UNDER it (component-wise prefix, so `base/foo` does not
/// match `base/foobar`). An empty `Vec` means no in-process holder keeps any
/// LMDB env open anywhere in that subtree.
///
/// This is the single source of truth for "can this DB directory be deleted
/// by this process right now": it sees through every holder shape — an outer
/// `Arc<SharedStores>` clone held by an in-flight search, an inner
/// `Arc<RwLock<VectorStore>>` captured by a `spawn_blocking` embed pass, a
/// `SCIP(...)` env in a `scip/` subdirectory — because all of them keep their
/// `TrackedEnv` (and therefore its registry slot) alive. `remove_repo` waits
/// on this before retrying a locked delete.
///
/// A path that cannot be canonicalized (e.g. already deleted) reports no
/// holders — the safe "nothing left to wait for" answer.
pub fn open_holders_under(path: &Path) -> Vec<String> {
    let canonical = match safe_canonicalize(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    match LMDB_REGISTRY.get() {
        Some(registry) => registry
            .iter()
            .filter(|entry| entry.key().starts_with(&canonical))
            .map(|entry| entry.value().description.clone())
            .collect(),
        None => Vec::new(),
    }
}

// ── Shared-env cache ────────────────────────────────────────────

/// Process-wide cache of LMDB environments that multiple components must use
/// CONCURRENTLY (queries, rebuilds, per-language adapters on the same
/// directory). Holds only [`Weak`] references: the cache never keeps an
/// environment alive, it just hands out the live one when it exists. When the
/// last user drops their `Arc`, the [`TrackedEnv`] drops, its registry slot
/// frees, and the stale cache entry is reaped on the next lookup.
static SHARED_ENVS: OnceLock<DashMap<PathBuf, Weak<TrackedEnv>>> = OnceLock::new();

/// Open the environment at `path`, or return the already-open shared instance.
///
/// LMDB permits exactly one open environment per directory per process, so
/// callers that may overlap in time (a rebuild vs. an in-flight query, the C#
/// vs. TypeScript adapters on the same `db_path/scip`) must not each open
/// their own — the second [`TrackedEnv::open`] trips the double-open guard and
/// one side fails outright. This getter makes the collision impossible: the
/// first caller opens (running `init` once to create the named databases) and
/// everyone else receives a clone of the same `Arc`.
///
/// `build_opts` configures the [`heed::EnvOpenOptions`] and `init` runs once
/// per environment lifetime, right after the open, before the handle is
/// published to other threads. Writers then serialise on LMDB's own
/// single-writer mutex and readers never block.
///
/// Both closures run while the cache's shard lock is held: they must not
/// re-enter `get_or_open_shared_env` (a path hashing to the same shard
/// self-deadlocks) and should stay cheap.
///
/// The env-var lookups inside `build_opts`/`init` run only when the directory
/// is opened for the first time in this process — later override changes do
/// not affect an already-shared environment.
pub fn get_or_open_shared_env(
    path: &Path,
    description: &str,
    build_opts: impl FnOnce(&mut heed::EnvOpenOptions),
    init: impl FnOnce(&TrackedEnv) -> Result<()>,
) -> Result<Arc<TrackedEnv>> {
    let canonical = safe_canonicalize(path)
        .with_context(|| format!("Cannot canonicalize LMDB path: {}", path.display()))?;
    let cache = SHARED_ENVS.get_or_init(DashMap::new);

    loop {
        use dashmap::mapref::entry::Entry;
        match cache.entry(canonical.clone()) {
            Entry::Occupied(occupied) => {
                if let Some(env) = occupied.get().upgrade() {
                    return Ok(env);
                }
                // Last Arc dropped but the entry survived — reap and retry so
                // the Vacant arm below performs a fresh open.
                occupied.remove();
            }
            Entry::Vacant(vacant) => {
                let mut opts = heed::EnvOpenOptions::new();
                build_opts(&mut opts);
                // SAFETY: caller contract — same as `TrackedEnv::open`. The
                // registry slot guards against any concurrent direct open.
                let tracked = unsafe { TrackedEnv::open(&opts, path, description)? };
                // Run init before publishing: the `?` early-return drops
                // `tracked`, freeing the registry slot, so a failed init
                // leaves the path openable.
                init(&tracked)?;
                let env = Arc::new(tracked);
                vacant.insert(Arc::downgrade(&env));
                return Ok(env);
            }
        }
    }
}

// ── TrackedEnv wrapper ──────────────────────────────────────────

/// Wrapper around [`heed::Env`] that prevents double-open panics.
///
/// On creation, registers the LMDB path in a global registry. If another
/// `TrackedEnv` is already open on the same canonical path, returns an error
/// with context about who opened it and when. On drop, unregisters automatically.
///
/// Implements `Deref<Target = heed::Env>` so all existing `env.method()` calls
/// work without changes.
pub struct TrackedEnv {
    /// Wrapped in `ManuallyDrop` so [`Drop`] can release the underlying
    /// `heed::Env` BEFORE freeing our own registry slot. See the `Drop` impl
    /// for why the ordering is load-bearing.
    inner: ManuallyDrop<heed::Env>,
    canonical: PathBuf,
}

impl TrackedEnv {
    /// Open a new LMDB environment, registered in the global tracker.
    ///
    /// # Safety
    /// Same as `heed::EnvOpenOptions::open` — caller must ensure no other process
    /// opens the same path with incompatible options (different map_size or flags).
    pub unsafe fn open(
        opts: &heed::EnvOpenOptions,
        path: &Path,
        description: &str,
    ) -> Result<Self> {
        let canonical = register(path, description)?;

        match opts.open(path) {
            Ok(env) => Ok(Self {
                inner: ManuallyDrop::new(env),
                canonical,
            }),
            Err(e) => {
                unregister(&canonical);
                Err(e.into())
            }
        }
    }
}

impl Drop for TrackedEnv {
    fn drop(&mut self) {
        // Ordering here is load-bearing, twice over.
        //
        // (1) The env must be closed BEFORE we free our own registry slot.
        // heed maintains its OWN process-global registry of opened
        // environments (`OPENED_ENV`), keyed by canonical path. If we
        // `unregister()` from our registry FIRST and let the field drop
        // afterwards (the default Rust drop order: body, then fields), there is
        // a window where our slot is free but heed's env is still alive. A
        // concurrent `TrackedEnv::open` on the same path — e.g. the idle reaper
        // dropping a repo while a reindex/query reopens it — then passes our
        // `register()` guard and falls through to `opts.open()`, which heed
        // rejects with the cryptic "an environment is already opened with
        // different options" (once a prior MDB_MAP_FULL resize left the live
        // env's recorded map_size differing from the reopen's resolved size).
        // Closing the env before `unregister()` enforces the invariant
        // "our slot free ⟹ heed's slot free": a concurrent open either sees
        // our slot still occupied (clear "double-open prevented" + retry) or
        // sees both free (clean reopen). It can never observe the inconsistent
        // state that produces heed's raw error.
        //
        // (2) A plain drop of the `heed::Env` does NOT close the environment.
        // heed 0.20's `OPENED_ENV` entry itself holds a strong `Env` clone
        // (`EnvEntry { env: Some(env.clone()), .. }` — inserted at open, used
        // to hand out further clones on re-open). With our wrapper as the only
        // user-side reference, dropping it leaves the Arc count at exactly 1:
        // the entry's own clone. `EnvInner::drop` — and with it
        // `mdb_env_close` — therefore NEVER runs. On POSIX that leaks an fd
        // and an mmap silently; on Windows it locks `data.mdb`/`lock.mdb`
        // against deletion for the lifetime of the process, which is why
        // `index rm` against a running serve could not delete the DB dir
        // (deterministic os error 32 after the whole retry budget, LMDB
        // registry long empty — the holder is invisible to it).
        // `prepare_for_closing()` is heed's one real close path: it takes the
        // entry's reference out and drops the last one synchronously
        // (entry removed, `mdb_env_close` called, waiters signalled) before
        // returning. It is correct here because nothing in this codebase
        // clones the `heed::Env` out of a `TrackedEnv` (the deref only lends
        // `&Env`; `TrackedEnv` itself is not `Clone`), so this wrapper holds
        // the last user-side reference.
        //
        // SAFETY: `inner` is taken exactly once, here, and the `ManuallyDrop`
        // slot is never touched again afterwards (the surrounding
        // `TrackedEnv` is being destroyed).
        let env = unsafe { ManuallyDrop::take(&mut self.inner) };
        env.prepare_for_closing();
        unregister(&self.canonical);
    }
}

impl Deref for TrackedEnv {
    type Target = heed::Env;
    fn deref(&self) -> &heed::Env {
        &self.inner
    }
}

impl std::fmt::Debug for TrackedEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackedEnv")
            .field("path", &self.canonical)
            .finish()
    }
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_opts() -> heed::EnvOpenOptions {
        make_opts_sized(1024 * 1024)
    }

    fn make_opts_sized(map_size: usize) -> heed::EnvOpenOptions {
        let mut opts = heed::EnvOpenOptions::new();
        opts.map_size(map_size).max_dbs(1);
        // Every test open carries the baseline flags per the AGENTS.md rule
        // ("open every env with BASE_ENV_FLAGS") so new tests cannot drift
        // from production open options.
        unsafe { opts.flags(BASE_ENV_FLAGS) };
        opts
    }

    /// Two read transactions must be able to be live at the same time on ONE
    /// thread. Without `NO_TLS` in [`BASE_ENV_FLAGS`] LMDB gives each thread a
    /// single reader lock-table slot and the second begin fails with
    /// `MDB_BAD_RSLOT: Invalid reuse of reader locktable slot` — reachable in
    /// `serve` whenever one handler holds a read txn open while starting
    /// another (e.g. `/info` stats while a search txn is live).
    #[test]
    fn base_flags_allow_concurrent_read_txns_on_one_thread() {
        let dir = TempDir::new().unwrap();
        let mut opts = make_opts();
        unsafe { opts.flags(BASE_ENV_FLAGS) };
        let env = unsafe { TrackedEnv::open(&opts, dir.path(), "concurrent-read-txn-test") }
            .expect("open env");

        let first = env.read_txn().expect("first read txn");
        let second = env
            .read_txn()
            .expect("second concurrent read txn on the same thread (needs MDB_NOTLS)");
        drop(first);
        drop(second);
    }

    #[test]
    fn test_registry_prevents_double_open() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();
        let opts = make_opts();

        // First open should succeed
        let _env1 = unsafe { TrackedEnv::open(&opts, path, "test-1").unwrap() };

        // Second open on same path should fail
        let result = unsafe { TrackedEnv::open(&opts, path, "test-2") };
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("double-open prevented"));
        assert!(err.contains("test-1"));
    }
    #[test]
    fn test_registry_allows_reopen_after_drop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();
        let opts = make_opts();

        {
            let _env1 = unsafe { TrackedEnv::open(&opts, path, "test-1").unwrap() };
            // env1 dropped here
        }

        // Should succeed after drop
        let _env2 = unsafe { TrackedEnv::open(&opts, path, "test-2").unwrap() };
    }

    /// Dropping the last `TrackedEnv` must REALLY close the heed environment.
    ///
    /// heed 0.20's `OPENED_ENV` entry holds a strong `Env` clone of its own,
    /// so a plain drop of the user-side `Env` leaves the Arc count at 1 (the
    /// entry's) and `mdb_env_close` never runs — the env stays open invisibly:
    /// `env_closing_event` keeps answering `Some`, and on Windows `data.mdb`/
    /// `lock.mdb` stay locked against deletion for the life of the process
    /// (the deterministic `index rm` os-error-32 failure this test pins).
    /// `TrackedEnv::drop` therefore closes via `prepare_for_closing()`.
    ///
    /// Cross-platform: the `env_closing_event` assert fails everywhere when
    /// the close path regresses; the directory-delete assert is the
    /// Windows-visible consequence of the same leak and guards it directly.
    #[test]
    fn drop_really_closes_heed_env_and_releases_the_files() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        std::fs::create_dir(&db_path).unwrap();
        let opts = make_opts();

        {
            let _env = unsafe { TrackedEnv::open(&opts, &db_path, "close-on-drop-test").unwrap() };
            assert!(
                heed::env_closing_event(&db_path).is_some(),
                "while the TrackedEnv lives, heed must report the env open"
            );
        }

        // The env must now be gone from heed's own registry too — not just
        // from ours (`open_holders_under` is vacuous here, it tracks
        // TrackedEnvs only).
        assert!(
            heed::env_closing_event(&db_path).is_none(),
            "heed's OPENED_ENV entry must be removed on TrackedEnv drop; \
             a surviving entry means mdb_env_close never ran"
        );

        // ...which is what makes the DB directory deletable on Windows.
        std::fs::remove_dir_all(&db_path)
            .expect("db dir must be deletable after the last TrackedEnv drops");
        assert!(!db_path.exists());
    }

    #[test]
    fn test_different_paths_both_allowed() {
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();
        let opts = make_opts();

        let _env1 = unsafe { TrackedEnv::open(&opts, dir1.path(), "test-1").unwrap() };
        let _env2 = unsafe { TrackedEnv::open(&opts, dir2.path(), "test-2").unwrap() };
    }

    fn shared_opts(opts: &mut heed::EnvOpenOptions) {
        opts.map_size(1024 * 1024).max_dbs(4);
        // Every test open carries the baseline flags per the AGENTS.md rule.
        unsafe { opts.flags(BASE_ENV_FLAGS) };
    }

    /// Direct-open options IDENTICAL to `shared_opts`. heed refuses to reopen
    /// a path with different options (max_dbs included) even after the prior
    /// env dropped — the AGENTS.md "same options on every open" rule — so the
    /// direct opens in the shared-env tests must not reuse `make_opts`
    /// (max_dbs 1).
    fn shared_compatible_opts() -> heed::EnvOpenOptions {
        let mut opts = heed::EnvOpenOptions::new();
        shared_opts(&mut opts);
        opts
    }

    /// Two callers of the shared getter receive the SAME environment — this is
    /// the property whose absence made a watcher rebuild fail with
    /// `LMDB double-open prevented` while a lazy find-refs held its own env.
    #[test]
    fn shared_env_returns_live_instance_to_concurrent_callers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();

        let env1 = get_or_open_shared_env(&path, "shared-1", shared_opts, |_| Ok(())).unwrap();
        let env2 = get_or_open_shared_env(&path, "shared-2", shared_opts, |_| Ok(())).unwrap();
        assert!(Arc::ptr_eq(&env1, &env2));

        // While the shared env is alive, a DIRECT open on the same path must
        // still trip the guard — the shared env genuinely occupies the slot.
        let direct = unsafe { TrackedEnv::open(&shared_compatible_opts(), &path, "direct") };
        let err = direct.unwrap_err().to_string();
        assert!(err.contains("double-open prevented"));
        assert!(err.contains("shared-1"));
    }

    /// The cache holds only weak refs: once the last user drops their Arc the
    /// registry slot frees (a direct open succeeds) and the next shared caller
    /// transparently reopens.
    #[test]
    fn shared_env_reopens_after_all_arcs_drop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();

        {
            let _env = get_or_open_shared_env(&path, "shared-1", shared_opts, |_| Ok(())).unwrap();
        }

        // Slot freed after the last Arc dropped.
        {
            let _direct =
                unsafe { TrackedEnv::open(&shared_compatible_opts(), &path, "direct").unwrap() };
        }

        // And the shared getter opens fresh again.
        let _again = get_or_open_shared_env(&path, "shared-2", shared_opts, |_| Ok(())).unwrap();
    }

    /// An `init` failure must not leak the registry slot: the error propagates
    /// and the env is dropped, leaving the path openable.
    #[test]
    fn shared_env_init_failure_frees_slot() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();

        let result = get_or_open_shared_env(&path, "shared-1", shared_opts, |_| {
            Err(anyhow::anyhow!("boom"))
        });
        assert!(result.is_err());

        let _direct =
            unsafe { TrackedEnv::open(&shared_compatible_opts(), &path, "direct").unwrap() };
    }

    /// N threads racing the FIRST open all succeed and all hold the same env —
    /// no thread sees the double-open error the per-caller open produced.
    #[test]
    fn shared_env_concurrent_first_open_is_safe() {
        let dir = TempDir::new().unwrap();
        let path = Arc::new(dir.path().to_path_buf());

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let p = Arc::clone(&path);
                std::thread::spawn(move || {
                    get_or_open_shared_env(&p, "shared-race", shared_opts, |_| Ok(()))
                })
            })
            .collect();

        let envs: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().expect("thread panicked").expect("open failed"))
            .collect();
        assert!(envs.windows(2).all(|w| Arc::ptr_eq(&w[0], &w[1])));
    }

    /// `open_holders_under` reports every live env at or under the queried
    /// path — the precondition check `remove_repo`'s lock-class retry waits
    /// on. Must see (a) an env whose path IS the queried path and (b) an env
    /// in a SUBDIRECTORY (the `scip/` case), with each holder's description
    /// carried for triage.
    #[test]
    fn open_holders_under_reports_envs_at_and_below_path() {
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("proj").join(".codesearch.db");
        std::fs::create_dir_all(db_dir.join("scip")).unwrap();
        let opts = make_opts();

        let _env_db = unsafe { TrackedEnv::open(&opts, &db_dir, "SharedStores(proj)").unwrap() };
        let _env_scip =
            unsafe { TrackedEnv::open(&opts, &db_dir.join("scip"), "SCIP(proj)").unwrap() };

        let holders = open_holders_under(&tmp.path().join("proj").join(".codesearch.db"));
        assert_eq!(
            holders.len(),
            2,
            "both the db-dir env and the scip-subdir env must be reported, got: {holders:?}"
        );
        assert!(holders.contains(&"SharedStores(proj)".to_string()));
        assert!(holders.contains(&"SCIP(proj)".to_string()));
    }

    /// Component-boundary safety: `base/foobar` must NOT count as a holder
    /// under `base/foo` — `Path::starts_with` is component-wise, and a string
    /// prefix match here would make `remove_repo` wait on (and warn about)
    /// holders of an entirely different repo's database.
    #[test]
    fn open_holders_under_does_not_match_partial_component() {
        let tmp = TempDir::new().unwrap();
        let foo = tmp.path().join("foo");
        let foobar = tmp.path().join("foobar");
        std::fs::create_dir_all(&foo).unwrap();
        std::fs::create_dir_all(&foobar).unwrap();
        let opts = make_opts();

        let _env = unsafe { TrackedEnv::open(&opts, &foobar, "other-repo").unwrap() };

        assert!(
            open_holders_under(&foo).is_empty(),
            "env at a sibling path sharing a string prefix must not be reported"
        );
    }

    /// The drain contract: once the last `TrackedEnv` drops, its registry slot
    /// goes with it — an empty `Vec` is `remove_repo`'s "deletable now" signal
    /// and must not lag behind the actual drop.
    #[test]
    fn open_holders_under_is_empty_after_env_drops() {
        let dir = TempDir::new().unwrap();
        let opts = make_opts();

        {
            let _env = unsafe { TrackedEnv::open(&opts, dir.path(), "transient-search").unwrap() };
            assert!(
                !open_holders_under(dir.path()).is_empty(),
                "holder must be visible while the env is live"
            );
        }

        assert!(
            open_holders_under(dir.path()).is_empty(),
            "holder must be gone after the env drops"
        );
    }

    /// A path that cannot be canonicalized (e.g. already deleted by a
    /// concurrent cleaner) reports no holders — the safe "nothing left to
    /// wait for" answer, never a spurious error.
    #[test]
    fn open_holders_under_reports_none_for_missing_path() {
        let missing = std::env::temp_dir().join("codesearch-never-exists-here");
        assert!(open_holders_under(&missing).is_empty());
    }

    /// Regression guard for the concurrent open→drop→reopen path that produced
    /// the production 500 ("an environment is already opened with different
    /// options").
    ///
    /// Contract: every open of a given path within the process MUST use the
    /// same `map_size` (the store layer enforces this via its process-global
    /// per-path map-size pin — see `vectordb::store::resolve_map_size`). heed
    /// rejects a reopen whose recorded options differ from a still-live env, and
    /// because heed defers env close, a reopen can briefly observe the prior
    /// env; the `TrackedEnv` `Drop` reorder (drop the heed env before freeing
    /// our slot) narrows that window but the consistent-size contract is what
    /// makes it fully safe — matching options mean heed reuses/reopens cleanly
    /// instead of erroring.
    ///
    /// This test churns open→drop→reopen on a single shared path from many
    /// threads (behind a barrier to maximize overlap), all using the SAME size,
    /// and asserts the forbidden heed string NEVER appears. Our own "double-open
    /// prevented" error IS allowed (it means `register()` serialized the race).
    /// The assertion can only fail on a real regression — never flaky.
    #[test]
    fn test_concurrent_reopen_same_size_never_conflicts() {
        use std::sync::{Arc, Barrier};

        const THREADS: usize = 8;
        const ITERS: usize = 4000;
        const MAP_SIZE: usize = 1024 * 1024;

        let dir = TempDir::new().unwrap();
        let path: Arc<std::path::PathBuf> = Arc::new(dir.path().to_path_buf());
        let barrier = Arc::new(Barrier::new(THREADS));

        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..ITERS {
                        let opts = make_opts_sized(MAP_SIZE);
                        match unsafe { TrackedEnv::open(&opts, &path, "race") } {
                            Ok(env) => drop(env),
                            Err(e) => {
                                let msg = e.to_string();
                                assert!(
                                    !msg.contains("already opened with different options"),
                                    "heed slot leaked past our registry slot: {msg}"
                                );
                                // "double-open prevented" is the expected,
                                // benign outcome of a serialized race.
                            }
                        }
                    }
                })
            })
            .collect();

        for h in threads {
            h.join().unwrap();
        }
    }
}
