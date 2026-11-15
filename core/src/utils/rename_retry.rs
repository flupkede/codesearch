//! Windows transient rename-error classification + bounded retry budget.
//!
//! Extracted from vectordb/store.rs so FileMetaStore (cache) and the vector
//! store share one copy of the raw-code list instead of growing a third.

/// Windows-only classification for a transient handle-holder racing our
/// rename: ERROR_ACCESS_DENIED (5), ERROR_SHARING_VIOLATION (32),
/// ERROR_LOCK_VIOLATION (33) — the same raw codes `ServeState::is_db_locked_error`
/// (`src/serve/mod.rs`, CLI crate) retries on. On Windows, AV/Search-indexer
/// momentarily opening a just-written small JSON file makes
/// `MOVEFILE_REPLACE_EXISTING` fail with "Access is denied" purely from
/// timing, not a real conflict — most visible under `cargo test --lib --bins`
/// parallel load. Unix renames are atomic replace and never hit this path,
/// so the retry is a no-op there.
pub fn is_transient_rename_error(e: &std::io::Error) -> bool {
    if let Some(raw) = e.raw_os_error() {
        if matches!(raw, 5 | 32 | 33) {
            return true;
        }
    }
    let msg = e.to_string();
    msg.contains("being used") || msg.contains("is in use") || msg.contains("Access is denied")
}

/// Bounded retry budget for rename steps: short, since a genuine
/// conflict (not a transient handle) should surface quickly rather than
/// stall the caller. Shared by the vector-store metadata rename and
/// `FileMetaStore::save`, which mirror this tmp+fsync+rename pattern.
pub const RENAME_RETRY_ATTEMPTS: u32 = 5;
pub const RENAME_RETRY_DELAY_MS: u64 = 20;
