//! codesearch-core: the shared library extracted from the codesearch CLI
//! (stage 1 of the codesearch-server plan, GH #384).
//!
//! Modules land here in dependency order: plumbing (error, constants, limits,
//! file, utils, logger, cache, lmdb_registry), then chunker, embed, retrieval
//! (fts, vectordb, rerank, search), symbols, federation, db_discovery, mcp,
//! and the SharedStores handle. CLI wiring, serve orchestration, watch and the
//! index lifecycle stay in the `codesearch` crate.

pub mod cache;
pub mod chunker;
pub mod constants;
pub mod db_discovery;
pub mod embed;
pub mod error;
pub mod federation;
pub mod file;
pub mod fts;
pub mod index;
pub mod limits;
pub mod lmdb_registry;
pub mod logger;
pub mod mcp;
pub mod output;
pub mod rerank;
pub mod search;
pub mod symbols;
pub mod testing;
pub mod utils;
pub mod vectordb;

/// The host binary's version, injected by the CLI crate's `main`
/// ([`set_app_version`]) so core code comparing itself against a deployed
/// artifact (the scip-csharp helper version check) sees the PRODUCT version
/// instead of `codesearch-core`'s own 0.x `CARGO_PKG_VERSION`.
static APP_VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The value injected via [`set_app_version`], falling back to the core
/// crate's own version when nothing was injected (libraries and tests that
/// never run the CLI binary).
pub fn host_version() -> &'static str {
    APP_VERSION
        .get()
        .map(String::as_str)
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

pub fn set_app_version(version: &str) {
    let _ = APP_VERSION.set(version.to_string());
}

#[cfg(test)]
mod app_version_tests {
    /// The scip-csharp helper's version check must see the PRODUCT version the
    /// CLI injected, never `codesearch-core`'s own 0.x CARGO_PKG_VERSION —
    /// that mismatch warning fired for every correctly-deployed helper after
    /// the workspace split. (The OnceLock is process-global, so this pins the
    /// injection path rather than per-test values; the CLI's main sets it
    /// before anything logs a version line.)
    #[test]
    fn host_version_reflects_the_cli_injection() {
        crate::set_app_version("9.9.9-test");
        assert_eq!(crate::host_version(), "9.9.9-test");
    }
}
