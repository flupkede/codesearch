pub mod compact;
pub(crate) mod store;

pub(crate) use store::INDEX_NOT_BUILT_MSG;
pub use store::{
    atomic_write_json, merge_metadata_atomic, ChunkMetadata, SearchResult, StoreStats, VectorStore,
    PATH_REWRITE_BATCH,
};

// Crash-atomic JSON writers outside this module (`FileMetaStore::save`)
// reuse the same transient-rename classification and retry budget instead
// of duplicating the raw-error-code list per call site.
