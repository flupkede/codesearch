//! Lock-visibility verification for todo #96 step 5.
//!
//! The busy answer deliberately carries no `lock_status` field — that is
//! only warranted if readers can actually BLOCK on an index the indexer is
//! writing. LMDB is MVCC: a read transaction never waits on the single
//! writer. These tests pin that property at the exact level `find_impact`
//! reads (a `read_txn` on the shared env): a reader issued while another
//! thread holds an uncommitted write transaction must complete promptly.
//! If LMDB semantics ever changed such that readers blocked, the 5s
//! timeout here fails — and `lock_status` in the busy envelope becomes the
//! follow-up, not before.

use std::time::Duration;

use heed::types::Str;
use heed::Database;

#[test]
fn readers_do_not_block_while_a_write_txn_holds_the_write_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut opts = heed::EnvOpenOptions::new();
    opts.map_size(10 * 1024 * 1024).max_dbs(3);
    // SAFETY: same flags as every env in this repo (see BASE_ENV_FLAGS).
    unsafe {
        opts.flags(crate::lmdb_registry::BASE_ENV_FLAGS);
    }
    let env = unsafe { opts.open(&dir) }.expect("open env");

    let db: Database<Str, Str> = {
        let mut wtxn = env.write_txn().expect("setup wtxn");
        let db = env.create_database(&mut wtxn, Some("t")).expect("db");
        wtxn.commit().expect("setup commit");
        db
    };

    // Hold the write lock with an UNCOMMITTED write.
    let mut wtxn = env.write_txn().expect("wtxn");
    db.put(&mut wtxn, "k", "v1").expect("put");

    // The find_impact shape: a reader on the same env, another thread.
    let reader_env = env.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rtxn = reader_env.read_txn();
        let outcome = rtxn.and_then(|rt| db.get(&rt, "k").map(|v| v.map(str::to_string)));
        let _ = tx.send(outcome);
    });

    let read_while_locked = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("reader must FINISH while the write lock is held — it blocked!");
    assert!(
        matches!(read_while_locked, Ok(None)),
        "uncommitted write must be invisible: {read_while_locked:?}"
    );

    // Control: the write was real — after commit it is visible.
    wtxn.commit().expect("commit");
    let rtxn = env.read_txn().expect("rtxn");
    assert_eq!(db.get(&rtxn, "k").expect("get"), Some("v1"));
}

#[test]
fn concurrent_reader_and_writer_do_not_deadlock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut opts = heed::EnvOpenOptions::new();
    opts.map_size(10 * 1024 * 1024).max_dbs(3);
    // SAFETY: same flags as every env in this repo (see BASE_ENV_FLAGS).
    unsafe {
        opts.flags(crate::lmdb_registry::BASE_ENV_FLAGS);
    }
    let env = unsafe { opts.open(&dir) }.expect("open env");

    let db: Database<Str, Str> = {
        let mut wtxn = env.write_txn().expect("setup wtxn");
        let db = env.create_database(&mut wtxn, Some("t")).expect("db");
        wtxn.commit().expect("setup commit");
        db
    };

    // Writer and reader interleaved on two threads — the steady state of a
    // serve with an active indexer and a busy find_impact.
    let writer_env = env.clone();
    let writer = std::thread::spawn(move || {
        for i in 0..25u32 {
            let mut wtxn = writer_env.write_txn().expect("wtxn");
            db.put(&mut wtxn, "k", i.to_string().as_str()).expect("put");
            wtxn.commit().expect("commit");
        }
    });
    let reader_env = env.clone();
    let reader = std::thread::spawn(move || {
        for _ in 0..25u32 {
            let rtxn = reader_env.read_txn().expect("rtxn");
            let _ = db.get(&rtxn, "k").expect("get");
            drop(rtxn);
        }
    });
    writer.join().expect("writer");
    reader.join().expect("reader");
}
