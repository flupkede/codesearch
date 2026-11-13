//! codesearch-core: the shared library extracted from the codesearch CLI
//! (stage 1 of the codesearch-server plan, GH #384).
//!
//! Modules land here in dependency order: plumbing (error, constants, limits,
//! file, utils, logger, cache, lmdb_registry), then chunker, embed, retrieval
//! (fts, vectordb, rerank, search), symbols, federation, db_discovery, mcp,
//! and the SharedStores handle. CLI wiring, serve orchestration, watch and the
//! index lifecycle stay in the `codesearch` crate.

pub mod cache;
pub mod constants;
pub mod db_discovery;
pub mod error;
pub mod file;
pub mod limits;
pub mod lmdb_registry;
pub mod logger;
pub mod output;
pub mod testing;
pub mod utils;
