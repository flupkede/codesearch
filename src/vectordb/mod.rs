mod store;

pub use store::{
    merge_metadata_atomic, ChunkMetadata, SearchResult, StoreStats, VectorStore, PATH_REWRITE_BATCH,
};

// Crash-atomic JSON writers outside this module (`FileMetaStore::save`)
// reuse the same transient-rename classification and retry budget instead
// of duplicating the raw-error-code list per call site.
pub(crate) use store::{
    is_transient_rename_error, RENAME_RETRY_ATTEMPTS, RENAME_RETRY_DELAY_MS,
};
