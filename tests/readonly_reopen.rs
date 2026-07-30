//! Regression guard for the read-only reopen path.
//!
//! A read-only `VectorStore` is what every `repo_read_only` repo runs on — in
//! the cloud federation that is the entire DOCS corpus on the serve replica.
//! Until this test existed the path was only ever exercised as a rare fallback
//! (database happened to be locked by another process), so a defect that made
//! *every* read fail could sit in the code from the initial commit without
//! anyone noticing: `open_readonly` opened its LMDB database handles inside a
//! read transaction and then ABORTED that transaction by dropping it. LMDB
//! closes handles opened in an aborted transaction, so `stats()` and `search()`
//! afterwards failed with a bare EINVAL (os error 22).
//!
//! The write path never had the problem because it opens its handles in a
//! committed write transaction — which is exactly why the bug was invisible
//! until read-only became a permanent operating mode.
//!
//! The store must be built in a SEPARATE PROCESS: heed keeps a process-global
//! registry of opened environments and refuses to reopen the same path with
//! different options, so a write-open followed by a read-only-open cannot both
//! happen in one process. The test therefore re-executes its own binary to run
//! the `build_db_child` helper.

use codesearch::chunker::{Chunk, ChunkKind};
use codesearch::embed::EmbeddedChunk;
use codesearch::vectordb::VectorStore;

const BUILD_DB_ENV: &str = "CODESEARCH_TEST_BUILD_DB";

fn sample_chunks() -> Vec<EmbeddedChunk> {
    vec![
        EmbeddedChunk::new(
            Chunk::new(
                "fn authenticate() {}".to_string(),
                0,
                1,
                ChunkKind::Function,
                "auth.rs".to_string(),
            ),
            vec![1.0, 0.0, 0.0, 0.0],
        ),
        EmbeddedChunk::new(
            Chunk::new(
                "fn calculate() {}".to_string(),
                2,
                3,
                ChunkKind::Function,
                "math.rs".to_string(),
            ),
            vec![0.0, 1.0, 0.0, 0.0],
        ),
    ]
}

/// Child-process helper: builds a small indexed store at `$CODESEARCH_TEST_BUILD_DB`.
/// Ignored by default so a normal `cargo test` run never executes it directly.
#[test]
#[ignore]
fn build_db_child() {
    let path = std::env::var(BUILD_DB_ENV)
        .unwrap_or_else(|_| panic!("{BUILD_DB_ENV} must be set for the child helper"));
    let mut store = VectorStore::new(std::path::Path::new(&path), 4).expect("create store");
    store.insert_chunks(sample_chunks()).expect("insert chunks");
    store.build_index().expect("build index");
    assert!(store.is_indexed());
}

#[test]
fn readonly_reopen_supports_stats_and_search() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("ro.db");

    let status = std::process::Command::new(std::env::current_exe().expect("current_exe"))
        .args(["--exact", "build_db_child", "--ignored", "--nocapture"])
        .env(BUILD_DB_ENV, &db_path)
        .status()
        .expect("spawn child to build the store");
    assert!(
        status.success(),
        "child failed to build the store: {status}"
    );
    assert!(
        db_path.exists(),
        "child did not create {}",
        db_path.display()
    );

    let store = VectorStore::open_readonly(&db_path, 4)
        .unwrap_or_else(|e| panic!("open_readonly failed: {e:#}"));

    // Cached at open time, so it stays `true` even when the handles below are
    // broken — on its own it proves nothing. The real assertions follow.
    assert!(store.is_indexed(), "read-only reopen lost the HNSW graph");

    let stats = store
        .stats()
        .unwrap_or_else(|e| panic!("stats() must work on a read-only store, got: {e:#}"));
    assert_eq!(stats.total_chunks, 2);
    assert_eq!(stats.total_files, 2);
    assert_eq!(stats.max_chunk_id, 1);
    assert!(stats.indexed);

    let results = store
        .search(&[0.9, 0.1, 0.0, 0.0], 2)
        .unwrap_or_else(|e| panic!("search() must work on a read-only store, got: {e:#}"));
    assert_eq!(results.len(), 2);
    assert!(
        results[0].content.contains("authenticate"),
        "nearest neighbour should be the query-adjacent chunk, got {:?}",
        results[0].content
    );
}
