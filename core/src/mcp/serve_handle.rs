//! Inversion point between core MCP handlers and the CLI serve orchestrator.
//!
//! `ServeState` (axum routes, TUI, file-watching, lifecycle) stays in the CLI
//! crate; the MCP handlers in core only need its question-answering surface.
//! This trait mirrors exactly that surface. The CLI implements it for
//! `ServeState`, and `CodesearchService` holds `Option<Arc<dyn ServeHandle>>`
//! so handler call sites keep their method names.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::db_discovery::ReposConfig;
use crate::embed::{EmbeddingModel, EmbeddingServicePool};
use crate::index::SharedStores;
use crate::symbols::SymbolIndexerRegistry;

use super::status_snapshot::StatusSnapshot;

/// The serve-side surface MCP handlers may touch. All reads only — lifecycle
/// (indexing, pruning, watching) stays on the concrete CLI type.
#[async_trait]
pub trait ServeHandle: Send + Sync {
    fn config_snapshot(&self) -> ReposConfig;
    async fn get_or_open_stores(
        &self,
        alias: &str,
        touch: bool,
    ) -> Result<Arc<SharedStores>, String>;
    fn get_opened_stores(&self, alias: &str) -> Option<Arc<SharedStores>>;
    fn is_held_in_process(&self, alias: &str, db_path: &Path) -> bool;
    fn repo_lock_status(&self, alias: &str) -> Option<&'static str>;
    fn is_indexing(&self, alias: &str) -> bool;
    fn touch_access(&self, alias: &str);
    fn record_tool_call(&self, alias: &str, tool_name: &str);
    fn record_remote_peer_activity(&self, peer_name: &str);
    fn session_disconnected(&self);
    fn resolve_group_aliases(&self, group: &str) -> Result<Vec<String>, String>;
    async fn backfill_group_identities(&self, group: &str);
    fn symbol_registry(&self) -> Arc<SymbolIndexerRegistry>;
    fn embedding_pool(&self) -> Arc<EmbeddingServicePool>;
    fn default_model(&self) -> Option<EmbeddingModel>;
    /// Per-repo recorded embedding model, if the repo's index metadata says one.
    fn model_for_alias(&self, alias: &str) -> Option<EmbeddingModel>;
    /// One-shot latch: true only the first time a legacy-model warning fires
    /// for this repo, so the warning logs once per serve lifetime.
    fn mark_legacy_model_warned(&self, alias: &str) -> bool;
    fn latest_status_snapshot(&self) -> Option<Arc<StatusSnapshot>>;
    /// Fire-and-forget rebuild of a stale symbol index; `Arc<Self>` because
    /// the implementation spawns blocking work that must outlive the call.
    async fn heal_missing_symbol_index(self: Arc<Self>, alias: &str, lang: &str);
}
