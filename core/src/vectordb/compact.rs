//! LMDB compaction ("VACUUM").
//!
//! An LMDB file never shrinks: deletions only grow the freelist, and the
//! file keeps its all-time high-water size forever. Before the batched
//! re-index wave (2026-11-10) every changed file drove a full HNSW graph
//! rebuild, which drove multi-GB high-water marks; a fresh build of the same
//! corpus wraps 3-4× less. `compact_db_dir` rewrites `data.mdb` with only
//! the live pages (heed's compact copy) and swaps it in.

use crate::lmdb_registry::{TrackedEnv, BASE_ENV_FLAGS};
use crate::utils::rename_retry::{
    is_transient_rename_error, RENAME_RETRY_ATTEMPTS, RENAME_RETRY_DELAY_MS,
};
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const COMPACT_TMP_SUFFIX: &str = "data.mdb.compact-tmp";
pub const ROLLBACK_SUFFIX: &str = "data.mdb.pre-compact";

/// Result of one compaction: the file size before and after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct CompactionOutcome {
    pub old_bytes: u64,
    pub new_bytes: u64,
}

impl CompactionOutcome {
    pub fn freed_bytes(&self) -> u64 {
        self.old_bytes.saturating_sub(self.new_bytes)
    }
}

/// Smallest MB count covering `bytes`.
fn ceil_mb(bytes: u64) -> usize {
    bytes.div_ceil(1024 * 1024) as usize
}

fn human_bytes(n: u64) -> String {
    let mb = n as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else {
        format!("{:.0} MB", mb)
    }
}

/// Free the freelist dead pages of `db_dir`'s `data.mdb`.
///
/// Runs even when there is nothing to free: the call is also the sanctioned
/// moment to (re)stamp `last_compact_unix_ms` in metadata.json, so an
/// auto-compact sweep does not retry every 5 minutes. Returns the sizes
/// observed; `freed_bytes() == 0` means the copy showed no gain and the
/// original file was kept.
pub fn compact_db_dir(db_dir: &Path) -> Result<CompactionOutcome> {
    let data_path = db_dir.join("data.mdb");
    if !data_path.is_file() {
        return Err(anyhow!(
            "no data.mdb under {} — nothing to compact",
            db_dir.display()
        ));
    }
    let old_bytes = std::fs::metadata(&data_path)?.len();
    let tmp_path = db_dir.join(COMPACT_TMP_SUFFIX);
    let _ = std::fs::remove_file(&tmp_path);

    // The copy reads every page of the source file, so the map must cover it:
    // a smaller map fails deep inside the copy with a page-range error.
    // BASE_ENV_FLAGS per the AGENTS.md LMDB rule.
    let open_map_mb = ceil_mb(old_bytes).max(crate::constants::DEFAULT_LMDB_MAP_SIZE_MB);
    let mut opts = heed::EnvOpenOptions::new();
    opts.map_size(open_map_mb * 1024 * 1024).max_dbs(16);
    unsafe { opts.flags(BASE_ENV_FLAGS) };

    // SAFETY: this env is a short-lived, single-owner open of a directory
    // whose normal store env is NOT open (serve-side eviction guaranteed it;
    // the CLI runs it in a process that never held the store open).
    let env = unsafe { TrackedEnv::open(&opts, db_dir, "compact copy source") }
        .with_context(|| format!("cannot open '{}' for compaction", db_dir.display()))?;
    let mut copy_dest = std::fs::File::create(&tmp_path)
        .with_context(|| format!("cannot create {}", tmp_path.display()))?;
    let copy = env.copy_to_file(&mut copy_dest, heed::CompactionOption::Enabled);
    drop(env); // close (prepare_for_closing) BEFORE touching the files
    copy.with_context(|| format!("compaction copy of '{}' failed", data_path.display()))?;

    let new_bytes = std::fs::metadata(&tmp_path)?.len();

    if new_bytes < old_bytes {
        // Publish: the rebuilt file drops in via the two-phase swap.
        swap_in_compact(db_dir)
            .with_context(|| format!("swap of the compact copy in {} failed", db_dir.display()))?;
        stamp_last_compact(db_dir)?;
        // Post-compact headroom: the rebuilt file is dense, but indexing must
        // be able to grow again without an immediate map-full resize storm.
        // 2× the live size, floored at the default, never above the cap.
        let target_mb = (ceil_mb(new_bytes) * 2)
            .max(crate::constants::DEFAULT_LMDB_MAP_SIZE_MB)
            .min(crate::constants::max_lmdb_map_size_mb());
        if target_mb != crate::vectordb::store::read_persisted_map_size(db_dir) {
            crate::vectordb::store::persist_map_size(db_dir, target_mb)?;
        }
        tracing::info!(
            "✅ compacted {}: {} → {} (freed {})",
            db_dir.display(),
            human_bytes(old_bytes),
            human_bytes(new_bytes),
            human_bytes(new_bytes)
        );
        Ok(CompactionOutcome {
            old_bytes,
            new_bytes,
        })
    } else {
        // No gain (already dense): the fresh copy validated the original, but
        // there is no freelist worth honoring a swap. Keep the original; the
        // stamp is still written so auto-compact does not retry every sweep.
        let _ = std::fs::remove_file(&tmp_path);
        stamp_last_compact(db_dir)?;
        tracing::info!(
            "✅ compact copy of {} validated — already dense ({}), nothing to free",
            db_dir.display(),
            human_bytes(old_bytes)
        );
        Ok(CompactionOutcome {
            old_bytes,
            new_bytes: old_bytes,
        })
    }
}

/// Publish the compacted file. Two-phase: the original becomes the rollback
/// file and the compact copy takes its name, so a crash at ANY point leaves
/// a complete `data.mdb` behind (either the original or the fresh copy).
fn swap_in_compact(db_dir: &Path) -> Result<()> {
    let data_path = db_dir.join("data.mdb");
    let tmp_path = db_dir.join(COMPACT_TMP_SUFFIX);
    let rollback_path = db_dir.join(ROLLBACK_SUFFIX);

    rename_with_retry(&data_path, &rollback_path)
        .with_context(|| format!("cannot back up {}", data_path.display()))?;
    match rename_with_retry(&tmp_path, &data_path) {
        Ok(()) => {
            // Best-effort cleanup: a locked rollback file is harmless — the
            // next compact's backup rename overwrites it.
            remove_with_retry(&rollback_path);
            Ok(())
        }
        Err(e) => {
            // Restore the original so the directory stays valid.
            let _ = rename_with_retry(&rollback_path, &data_path);
            let _ = std::fs::remove_file(&tmp_path);
            Err(e).with_context(|| {
                format!(
                    "cannot install the compact copy into {}",
                    data_path.display()
                )
            })
        }
    }
}

fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last_err = None;
    for attempt in 0..RENAME_RETRY_ATTEMPTS {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if is_transient_rename_error(&e) && attempt + 1 < RENAME_RETRY_ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(RENAME_RETRY_DELAY_MS));
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.expect("loop ran at least once"))
}

/// Plain remove; a transient lock (AV scanner) just retries briefly.
fn remove_with_retry(path: &Path) {
    for _ in 0..RENAME_RETRY_ATTEMPTS {
        match std::fs::remove_file(path) {
            Ok(()) => return,
            Err(e) if is_transient_rename_error(&e) => {
                std::thread::sleep(Duration::from_millis(RENAME_RETRY_DELAY_MS));
            }
            Err(_) => return,
        }
    }
}

/// Stamp `last_compact_unix_ms` into metadata.json (wall-clock now, ms).
pub fn stamp_last_compact(db_dir: &Path) -> Result<()> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    crate::vectordb::store::merge_metadata_atomic(db_dir, |obj| {
        obj.insert(
            "last_compact_unix_ms".to_string(),
            serde_json::Value::Number(now_ms.into()),
        );
    })
}

/// `last_compact_unix_ms` from metadata.json (0 = never compacted).
pub fn read_last_compact_unix_ms(db_dir: &Path) -> u64 {
    crate::vectordb::store::read_last_compact_unix_ms(db_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Build a real LMDB env in `db_dir`: `entries` keys of `value_len` bytes,
    /// half of them rewritten AND everything beyond `keep` deleted — dead
    /// pages on both counts — then the env closed.
    fn build_bloated_env(
        db_dir: &std::path::Path,
        entries: usize,
        value_len: usize,
        keep: usize,
    ) -> Result<()> {
        std::fs::create_dir_all(db_dir)?;
        let mut opts = heed::EnvOpenOptions::new();
        opts.map_size(256 * 1024 * 1024).max_dbs(4);
        unsafe { opts.flags(BASE_ENV_FLAGS) };
        // SAFETY: test-local single-owner env on a fresh temp dir.
        let env = unsafe { TrackedEnv::open(&opts, db_dir, "compact test fixture") }?;
        {
            let db = crate::lmdb_registry::create_named_db::<heed::types::Str, heed::types::Str>(
                &env, "chunks",
            )?;
            let value = |i: usize| {
                let key = format!("key{:06}", i);
                format!("{key}{}", "x".repeat(value_len.saturating_sub(key.len())))
            };
            let mut txn = env.write_txn()?;
            for i in 0..entries {
                db.put(&mut txn, format!("key{:06}", i).as_str(), value(i).as_str())?;
            }
            txn.commit()?;
            let mut txn = env.write_txn()?;
            for i in 0..entries / 2 {
                db.put(&mut txn, format!("key{:06}", i).as_str(), value(i).as_str())?;
            }
            txn.commit()?;
            let mut txn = env.write_txn()?;
            for i in keep..entries {
                let _ = db.delete(&mut txn, format!("key{:06}", i).as_str())?;
            }
            txn.commit()?;
        }
        drop(env);
        Ok(())
    }

    #[test]
    fn compact_swaps_in_the_dense_file_and_stamps_metadata() {
        let tmp = tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        build_bloated_env(&db_dir, 4000, 8192, 1000).unwrap();

        let before = std::fs::metadata(db_dir.join("data.mdb")).unwrap().len();
        let outcome = compact_db_dir(&db_dir).unwrap();
        let after = std::fs::metadata(db_dir.join("data.mdb")).unwrap().len();

        assert_eq!(outcome.old_bytes, before);
        assert!(
            after < before / 3,
            "freelist must be reclaimed: before {before}, after {after}"
        );
        assert_eq!(outcome.new_bytes, after);
        assert!(outcome.freed_bytes() > 0);
        assert!(db_dir.join(ROLLBACK_SUFFIX).metadata().is_err());
        assert!(db_dir.join(COMPACT_TMP_SUFFIX).metadata().is_err());
        assert!(read_last_compact_unix_ms(&db_dir) > 0);
    }

    #[test]
    fn compacted_env_reopens_with_its_live_data_intact() {
        let tmp = tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        build_bloated_env(&db_dir, 2000, 4096, 500).unwrap();
        compact_db_dir(&db_dir).unwrap();

        // Reopen exactly like the store would: the compacted file must answer
        // the same keys — the proof that the swap never lost data.
        let mut opts = heed::EnvOpenOptions::new();
        opts.map_size(256 * 1024 * 1024).max_dbs(4);
        unsafe { opts.flags(BASE_ENV_FLAGS) };
        // SAFETY: test-local single-owner env.
        let env = unsafe { TrackedEnv::open(&opts, &db_dir, "compact reopen check") }.unwrap();
        let db = crate::lmdb_registry::open_named_db::<heed::types::Str, heed::types::Str>(
            &env, "chunks",
        )
        .unwrap()
        .unwrap();
        let txn = env.read_txn().unwrap();
        let count = db.iter(&txn).unwrap().count();
        drop(txn);
        drop(env);
        assert_eq!(count, 500);
    }

    #[test]
    fn dense_env_still_trims_allocation_slack_and_stamps_metadata() {
        let tmp = tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        build_bloated_env(&db_dir, 200, 512, 200).unwrap(); // no rewrites, no deletes
        let before = std::fs::metadata(db_dir.join("data.mdb")).unwrap().len();
        let outcome = compact_db_dir(&db_dir).unwrap();
        let after = std::fs::metadata(db_dir.join("data.mdb")).unwrap().len();
        // Even without deletions the compact copy trims LMDB's allocation
        // slack, so a swap happens whenever the copy is smaller.
        assert!(after < before, "slack must shrink: {before} → {after}");
        assert_eq!(outcome.new_bytes, after);
        assert!(read_last_compact_unix_ms(&db_dir) > 0);
    }

    #[test]
    fn missing_data_mdb_is_a_clear_error_not_a_panic() {
        let tmp = tempdir().unwrap();
        let db_dir = tmp.path().join("empty");
        let err = compact_db_dir(&db_dir).unwrap_err();
        assert!(err.to_string().contains("nothing to compact"), "{err}");
    }
}
