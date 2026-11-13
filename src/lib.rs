pub mod bench;
pub mod cli;
pub mod embed;
pub mod federation;
pub mod fts;
pub mod index;
pub mod mcp;
pub mod rerank;
pub mod search;
pub mod serve;
pub mod symbols;
pub mod vectordb;
pub mod watch;

// Shared library extracted to codesearch-core; re-exported under the old
// paths so CLI-internal `crate::` references stay valid. `testing` is a plain
// core module (not cfg(test)): test-only modules vanish in dependency builds.
pub use codesearch_core::{
    cache, chunker, constants, db_discovery, error, file, limits, lmdb_registry, logger, output,
    testing, utils,
};
// #[macro_export] macros live at the defining crate's root; re-exported so
// CLI-internal `crate::info_print!` / `crate::warn_print!` keep resolving.
pub use codesearch_core::{info_print, warn_print};

// Re-export commonly used types
pub use chunker::{group_chunks_by_path, group_chunks_by_path_with_capacity};
pub use chunker::{Chunk, ChunkKind, Chunker};
pub use embed::group_embedded_chunks_by_path;
pub use embed::{CacheStats, EmbeddedChunk, EmbeddingService, ModelType};
pub use error::{CodeSearchError, Result as CsResult};
pub use file::{FileInfo, FileWalker, Language, WalkStats};
pub use fts::{FtsResult, FtsStore};
pub use vectordb::{SearchResult, StoreStats, VectorStore};
