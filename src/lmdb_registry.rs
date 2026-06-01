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
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use crate::cache::safe_canonicalize;

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
    inner: heed::Env,
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
                inner: env,
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
        let mut opts = heed::EnvOpenOptions::new();
        opts.map_size(1024 * 1024).max_dbs(1);
        opts
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
}
