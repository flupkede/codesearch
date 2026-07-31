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
use std::sync::OnceLock;
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
        // Ordering here is load-bearing. heed maintains its OWN process-global
        // registry of opened environments (`OPENED_ENV`), keyed by canonical
        // path, that outlives a `heed::Env` until its last strong ref drops.
        // If we `unregister()` from our registry FIRST and let the field drop
        // afterwards (the default Rust drop order: body, then fields), there is
        // a window where our slot is free but heed's env is still alive. A
        // concurrent `TrackedEnv::open` on the same path — e.g. the idle reaper
        // dropping a repo while a reindex/query reopens it — then passes our
        // `register()` guard and falls through to `opts.open()`, which heed
        // rejects with the cryptic "an environment is already opened with
        // different options" (once a prior MDB_MAP_FULL resize left the live
        // env's recorded map_size differing from the reopen's resolved size).
        //
        // Dropping the `heed::Env` BEFORE `unregister()` enforces the invariant
        // "our slot free ⟹ heed's slot free": a concurrent open either sees our
        // slot still occupied (clear "double-open prevented" + retry) or sees
        // both free (clean reopen). It can never observe the inconsistent state
        // that produces heed's raw error.
        //
        // SAFETY: `inner` is dropped exactly once, here, and never accessed
        // again (the surrounding `TrackedEnv` is being destroyed).
        unsafe {
            ManuallyDrop::drop(&mut self.inner);
        }
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

    #[test]
    fn test_different_paths_both_allowed() {
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();
        let opts = make_opts();

        let _env1 = unsafe { TrackedEnv::open(&opts, dir1.path(), "test-1").unwrap() };
        let _env2 = unsafe { TrackedEnv::open(&opts, dir2.path(), "test-2").unwrap() };
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
