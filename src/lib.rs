pub mod bench;
pub mod cache;
pub mod chunker;
pub mod constants;
pub mod db_discovery;
pub mod embed;
pub mod error;
pub mod file;
pub mod fts;
pub mod index;
pub mod mcp;
pub mod output;
pub mod rerank;
pub mod search;
pub mod server;
pub mod utils;
pub mod vectordb;
pub mod watch;

// Re-export commonly used types
pub use chunker::{Chunk, ChunkKind, Chunker};
pub use embed::{CacheStats, EmbeddedChunk, EmbeddingService, ModelType};
pub use error::{CodeSearchError, Result as CsResult};
pub use file::{FileInfo, FileWalker, Language, WalkStats};
pub use fts::{FtsResult, FtsStore};
pub use utils::{
    group_chunks_by_path, group_chunks_by_path_with_capacity, group_embedded_chunks_by_path,
};
pub use vectordb::{SearchResult, StoreStats, VectorStore};
