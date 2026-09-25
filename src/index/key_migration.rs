//! Repair of indexes written before chunk paths became project-relative.
//!
//! 1. absolute `FileMetaStore` keys are re-keyed to their relative form, so
//!    unchanged files are recognised as up to date instead of re-embedded;
//! 2. the chunks of those files get the relative path in both stores;
//! 3. chunks no tracked file references are deleted — the duplicates a
//!    previous (possibly interrupted) re-embed under relative paths left.

use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::Arc;
use tracing::info;

use super::SharedStores;
use crate::cache::FileMetaStore;
use crate::fts::FtsStore;
use crate::vectordb::{ChunkMetadata, VectorStore};

/// In-process variant for callers that own both stores exclusively (CLI).
/// Returns `true` when `file_meta` changed and must be saved by the caller.
pub(crate) fn repair_legacy_index(
    project_root: &Path,
    file_meta: &mut FileMetaStore,
    vector_store: &mut VectorStore,
    fts_store: &mut FtsStore,
) -> Result<bool> {
    let migration = file_meta.relativize_legacy_keys(project_root);
    let rewritten = rewrite_vector_paths(vector_store, &migration.rekeyed)?;
    mirror_paths_into_fts(fts_store, &rewritten)?;
    log_migration(
        migration.rekeyed.len(),
        rewritten.len(),
        migration.superseded,
    );

    let untracked = untracked_chunk_ids(file_meta, vector_store)?;
    if !untracked.is_empty() {
        delete_from_vector_store(vector_store, &untracked)?;
        delete_from_fts(fts_store, &untracked)?;
        log_sweep(untracked.len());
    }
    Ok(!migration.is_empty())
}

/// Shared-stores variant. Each phase takes ONE store lock on the blocking
/// pool, never both at once, so it cannot deadlock against a reader that
/// holds the other lock.
pub(crate) async fn repair_legacy_index_shared(
    project_root: &Path,
    file_meta: &mut FileMetaStore,
    stores: &SharedStores,
) -> Result<bool> {
    let migration = file_meta.relativize_legacy_keys(project_root);

    let rekeyed = migration.rekeyed.clone();
    let rewritten = on_store(&stores.vector_store, move |vs| {
        rewrite_vector_paths(vs, &rekeyed)
    })
    .await?;
    let rewritten_count = rewritten.len();
    on_store(&stores.fts_store, move |fts| {
        mirror_paths_into_fts(fts, &rewritten)
    })
    .await?;
    log_migration(
        migration.rekeyed.len(),
        rewritten_count,
        migration.superseded,
    );

    let tracked = file_meta.tracked_chunk_ids();
    let meta_empty = file_meta.is_empty();
    let untracked = on_store(&stores.vector_store, move |vs| {
        if meta_empty {
            return Ok(Vec::new());
        }
        let untracked = untracked_among(vs, &tracked)?;
        if !untracked.is_empty() {
            delete_from_vector_store(vs, &untracked)?;
        }
        Ok(untracked)
    })
    .await?;
    if !untracked.is_empty() {
        let count = untracked.len();
        on_store(&stores.fts_store, move |fts| {
            delete_from_fts(fts, &untracked)
        })
        .await?;
        log_sweep(count);
    }
    Ok(!migration.is_empty())
}

async fn on_store<S, T, F>(store: &Arc<tokio::sync::RwLock<S>>, f: F) -> Result<T>
where
    S: Send + Sync + 'static,
    T: Send + 'static,
    F: FnOnce(&mut S) -> Result<T> + Send + 'static,
{
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || f(&mut store.blocking_write()))
        .await
        .map_err(|e| anyhow!("legacy index repair task panicked: {}", e))?
}

fn rewrite_vector_paths(
    vector_store: &mut VectorStore,
    rekeyed: &[(String, Vec<u32>)],
) -> Result<Vec<(u32, ChunkMetadata)>> {
    if rekeyed.is_empty() {
        return Ok(Vec::new());
    }
    let updates: Vec<(u32, String)> = rekeyed
        .iter()
        .flat_map(|(key, ids)| ids.iter().map(move |id| (*id, key.clone())))
        .collect();
    vector_store.rewrite_chunk_paths(&updates)
}

fn mirror_paths_into_fts(
    fts_store: &mut FtsStore,
    rewritten: &[(u32, ChunkMetadata)],
) -> Result<()> {
    if rewritten.is_empty() {
        return Ok(());
    }
    for (id, meta) in rewritten {
        fts_store.delete_chunk(*id)?;
        fts_store.add_chunk(
            *id,
            &meta.content,
            &meta.path,
            meta.signature.as_deref(),
            &meta.kind,
        )?;
    }
    fts_store.commit()
}

fn untracked_chunk_ids(file_meta: &FileMetaStore, vector_store: &VectorStore) -> Result<Vec<u32>> {
    if file_meta.is_empty() {
        return Ok(Vec::new());
    }
    untracked_among(vector_store, &file_meta.tracked_chunk_ids())
}

/// Chunk ids in the store that no tracked file references. The full scan
/// only runs when the store holds more chunks than the metadata accounts for.
fn untracked_among(
    vector_store: &VectorStore,
    tracked: &std::collections::HashSet<u32>,
) -> Result<Vec<u32>> {
    let (total_chunks, _) = vector_store.index_health()?;
    if total_chunks <= tracked.len() {
        return Ok(Vec::new());
    }
    Ok(vector_store
        .get_chunks_by_file()?
        .into_values()
        .flatten()
        .filter(|id| !tracked.contains(id))
        .collect())
}

fn delete_from_vector_store(vector_store: &mut VectorStore, ids: &[u32]) -> Result<()> {
    vector_store.delete_chunks(ids)?;
    // Deletes invalidate the vector graph; a refresh with nothing else to
    // embed would otherwise leave search failing with "Index not built".
    vector_store.build_index()
}

fn delete_from_fts(fts_store: &mut FtsStore, ids: &[u32]) -> Result<()> {
    for id in ids {
        fts_store.delete_chunk(*id)?;
    }
    fts_store.commit()
}

fn log_migration(rekeyed: usize, rewritten: usize, superseded: usize) {
    if rekeyed + superseded > 0 {
        info!(
            "🔄 Migrated {} legacy absolute file key(s) to project-relative ({} chunk path(s) \
             rewritten, {} superseded entr(ies) dropped)",
            rekeyed, rewritten, superseded
        );
    }
}

fn log_sweep(count: usize) {
    info!(
        "🧹 Deleted {} chunk(s) not referenced by any tracked file (duplicates from an \
         earlier re-index)",
        count
    );
}
