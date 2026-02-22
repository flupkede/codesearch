//! MCP (Model Context Protocol) server for Claude Code integration
//!
//! Exposes codesearch's semantic search capabilities via the MCP protocol,
//! allowing AI assistants like Claude to search codebases during conversations.
//!
//! # Important: No Stdout Output
//!
//! The MCP module MUST NOT use `print!` or `println!` macros anywhere in its code.
//! All non-JSON output must go to stderr via `info_print!`, `warn_print!`, or `eprintln!`.
//! This is critical because the MCP protocol communicates over stdout via JSON-RPC,
//! and any stdout pollution will break the protocol.

#[cfg(test)]
mod tests {
    #[test]
    fn test_mcp_no_raw_stdout_calls() {
        // Verify that no raw print!/println! calls exist in the MCP module sources.
        // MCP communicates over stdout (JSON-RPC), so any stdout pollution breaks the protocol.
        // All informational output must go through info_print!/warn_print!/eprintln! (stderr).
        let src = include_str!("mod.rs");
        let violations: Vec<(usize, &str)> = src
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let trimmed = line.trim_start();
                // Skip comments and lines that are part of the detection logic itself
                if trimmed.starts_with("//") || trimmed.starts_with("\"") {
                    return false;
                }
                // Only flag lines that actually invoke print! or println! as a macro call
                // (i.e. the identifier immediately followed by '!'), not lines discussing them
                let call_println = line.contains("println!(");
                let call_print = trimmed.starts_with("print!(")
                    || line.contains(" print!(")
                    || line.contains("\tprint!(");
                let is_prefixed =
                    line.contains("info_print!(") || line.contains("warn_print!(");
                let is_detection_code = line.contains("line.contains(");
                (call_println || call_print) && !is_prefixed && !is_detection_code
            })
            .collect();

        assert!(
            violations.is_empty(),
            "MCP module has raw stdout calls that break the JSON-RPC protocol:\n{}",
            violations
                .iter()
                .map(|(i, l)| format!("  line {}: {}", i + 1, l.trim()))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

pub mod types;

use anyhow::Result;
use rmcp::{
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::db_discovery::{find_best_database, find_databases};

/// Normalize a path string for comparison.
/// Delegates to `cache::normalize_path_str` for consistency, then strips "./" prefix.
fn normalize_path_for_compare(path: &str) -> String {
    crate::cache::normalize_path_str(path)
        .trim_start_matches("./")
        .to_string()
}
use crate::embed::{EmbeddingService, ModelType};
use crate::file::Language;
use crate::fts::FtsStore;
use crate::index::{IndexManager, SharedStores};
use crate::rerank::{rrf_fusion, rrf_fusion_with_exact, EXACT_MATCH_RRF_K};
use crate::search::{adapt_rrf_k, boost_kind, detect_identifiers, detect_structural_intent};
use crate::vectordb::VectorStore;

// Re-export types
pub use types::*;

/// Codesearch MCP service
pub struct CodesearchService {
    tool_router: ToolRouter<CodesearchService>,
    db_path: PathBuf,
    project_path: PathBuf,
    model_type: ModelType,
    dimensions: usize,
    // Lazily initialized on first search
    embedding_service: Mutex<Option<EmbeddingService>>,
    // Shared stores for concurrent access (optional - only set when running with IndexManager)
    shared_stores: Option<Arc<SharedStores>>,
}

impl std::fmt::Debug for CodesearchService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodesearchService")
            .field("db_path", &self.db_path)
            .field("model_type", &self.model_type)
            .field("dimensions", &self.dimensions)
            .field("has_shared_stores", &self.shared_stores.is_some())
            .finish()
    }
}

// === Tool Router Implementation ===

#[tool_router]
impl CodesearchService {
    /// Create a new CodesearchService (standalone mode - opens its own VectorStore)
    #[allow(dead_code)] // Reserved for standalone MCP server mode
    pub fn new(requested_path: Option<PathBuf>) -> Result<Self> {
        Self::new_with_stores(requested_path, None)
    }

    /// Create a new CodesearchService with shared stores (for use with IndexManager)
    pub fn new_with_stores(
        requested_path: Option<PathBuf>,
        shared_stores: Option<Arc<SharedStores>>,
    ) -> Result<Self> {
        // Find the best database to use
        let db_info = find_best_database(requested_path.as_deref())?;

        if db_info.is_none() {
            return Err(anyhow::anyhow!(
                "No database found in current directory, parent directories, or globally tracked repositories. \
                 Run 'codesearch index' first to index the codebase."
            ));
        }

        let db_info = db_info.unwrap();
        let db_path = db_info.db_path;
        let project_path = db_info.project_path;

        // Read model metadata from database
        let metadata_path = db_path.join("metadata.json");
        let (model_type, dimensions) = if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path)?;
            let json: serde_json::Value = serde_json::from_str(&content)?;
            let model_name = json
                .get("model_short_name")
                .and_then(|v| v.as_str())
                .unwrap_or("minilm-l6");
            let dims = json
                .get("dimensions")
                .and_then(|v| v.as_u64())
                .unwrap_or(384) as usize;
            let mt = ModelType::parse(model_name).unwrap_or_default();
            (mt, dims)
        } else {
            (ModelType::default(), 384)
        };

        Ok(Self {
            tool_router: Self::tool_router(),
            db_path,
            project_path,
            model_type,
            dimensions,
            embedding_service: Mutex::new(None),
            shared_stores,
        })
    }

    /// Get or initialize the embedding service
    fn get_embedding_service(&self) -> Result<std::sync::MutexGuard<'_, Option<EmbeddingService>>> {
        let mut guard = self.embedding_service.lock().unwrap();
        if guard.is_none() {
            let cache_dir = crate::constants::get_global_models_cache_dir()?;
            *guard = Some(EmbeddingService::with_cache_dir(
                self.model_type,
                Some(&cache_dir),
            )?);
        }
        Ok(guard)
    }

    /// Check if database exists and return error if not
    fn ensure_database_exists(&self) -> Result<(), String> {
        if !self.db_path.exists() {
            return Err(format!(
                "❌ No index database found at: {}\n\n\
                 ⚠️  IMPORTANT: This MCP server cannot index the codebase itself. Indexing takes 30-60 seconds and must be done manually.\n\n\
                 To fix this, run the following command in your terminal:\n\
                 $ cd {}\n\
                 $ codesearch index\n\n\
                 For more information about database locations, use the find_databases tool.",
                self.db_path.display(),
                self.project_path.display()
            ));
        }
        Ok(())
    }

    #[tool(
        description = "Search code semantically using natural language. Returns compact metadata by default (path, line numbers, kind, signature, score). Use the read tool with the returned line numbers to view actual code. Set compact=false only when you need full content inline. Use filter_path to narrow results to a specific directory."
    )]
    async fn semantic_search(
        &self,
        Parameters(request): Parameters<SemanticSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let limit = request.limit.unwrap_or(10);
        let compact = request.compact.unwrap_or(true);

        tracing::debug!(
            "MCP semantic_search: query='{}', limit={}, compact={}",
            request.query,
            limit,
            compact
        );

        // Ensure database exists
        if let Err(e) = self.ensure_database_exists() {
            return Ok(CallToolResult::success(vec![Content::text(e)]));
        }

        // Get embedding service and embed query
        // Note: We must drop the MutexGuard before any await points
        tracing::debug!("MCP: Getting embedding service...");
        let query_embedding = {
            let mut service_guard = match self.get_embedding_service() {
                Ok(g) => g,
                Err(e) => {
                    tracing::error!("MCP: Failed to get embedding service: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error initializing embedding service: {}",
                        e
                    ))]));
                }
            };

            let service = service_guard.as_mut().unwrap();
            tracing::debug!("MCP: Embedding query...");
            match service.embed_query(&request.query) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("MCP: Failed to embed query: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error embedding query: {}",
                        e
                    ))]));
                }
            }
            // service_guard is dropped here, before any await
        };

        // Search using shared stores if available, otherwise open a new store
        tracing::debug!(
            "MCP: Searching with {} dimensions...",
            query_embedding.len()
        );
        let vector_results = if let Some(ref stores) = self.shared_stores {
            // Use shared store with read lock
            let store = stores.vector_store.read().await;
            match store.search(&query_embedding, limit * 3) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("MCP: Search failed (shared store): {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching: {}",
                        e
                    ))]));
                }
            }
        } else {
            // Fallback: open a new store (standalone mode)
            tracing::debug!("MCP: Opening vector store (standalone mode)...");
            let store = match VectorStore::new(&self.db_path, self.dimensions) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("MCP: Failed to open vector store: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error opening database: {}. The database may be corrupted or not indexed yet.",
                        e
                    ))]));
                }
            };
            match store.search(&query_embedding, limit * 3) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("MCP: Search failed: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching: {}",
                        e
                    ))]));
                }
            }
        };

        tracing::debug!("MCP: Found {} vector results", vector_results.len());

        // --- Hybrid search with all improvements ---

        // Detect identifiers and structural intent from query
        let identifiers = detect_identifiers(&request.query);
        let structural_intent = detect_structural_intent(&request.query);
        let (vector_k, fts_k) = adapt_rrf_k(&request.query);

        tracing::debug!(
            "MCP: Query analysis - identifiers: {:?}, structural_intent: {:?}, rrf_k: ({}, {})",
            identifiers,
            structural_intent,
            vector_k,
            fts_k
        );

        // Perform FTS search and fusion
        let mut results = match FtsStore::new(&self.db_path) {
            Ok(fts_store) => {
                // FTS search
                let fts_results = fts_store
                    .search(&request.query, limit * 3, structural_intent.clone())
                    .unwrap_or_default();

                let fused = if identifiers.is_empty() {
                    // No identifiers: standard RRF fusion
                    rrf_fusion(&vector_results, &fts_results, vector_k as f32)
                } else {
                    // Has identifiers: also do exact search per identifier
                    let mut all_exact: Vec<crate::fts::FtsResult> = Vec::new();
                    for ident in &identifiers {
                        if let Ok(exact) =
                            fts_store.search_exact(ident, limit * 2, structural_intent.clone())
                        {
                            for r in exact {
                                if !all_exact.iter().any(|e| e.chunk_id == r.chunk_id) {
                                    all_exact.push(r);
                                }
                            }
                        }
                    }

                    tracing::debug!(
                        "MCP: FTS found {} results, exact found {} results",
                        fts_results.len(),
                        all_exact.len()
                    );

                    rrf_fusion_with_exact(
                        &vector_results,
                        &fts_results,
                        &all_exact,
                        vector_k as f32,
                        fts_k as f32,
                        EXACT_MATCH_RRF_K,
                    )
                };

                // Map FusedResult back to SearchResult
                let chunk_to_result: std::collections::HashMap<
                    u32,
                    &crate::vectordb::SearchResult,
                > = vector_results.iter().map(|r| (r.id, r)).collect();

                let mut mapped: Vec<crate::vectordb::SearchResult> = Vec::new();
                for f in fused.into_iter().take(limit) {
                    if let Some(result) = chunk_to_result.get(&f.chunk_id) {
                        let mut r = (*result).clone();
                        r.score = f.rrf_score;
                        mapped.push(r);
                    }
                }
                mapped
            }
            Err(e) => {
                // FTS unavailable, fall back to vector-only results
                tracing::warn!("MCP: FTS store unavailable, using vector-only: {:?}", e);
                vector_results.into_iter().take(limit).collect()
            }
        };

        // Apply language boost (improvement 2)
        if let Some((_, _, Some(primary_lang))) = crate::search::read_metadata(&self.db_path) {
            for result in &mut results {
                let file_lang = format!(
                    "{:?}",
                    Language::from_path(std::path::Path::new(&result.path))
                );
                if file_lang.to_lowercase() == primary_lang.to_lowercase() {
                    result.score *= 1.2;
                }
            }
            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }

        // Apply kind boost (improvement 3)
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut results, target_kind);
        }

        tracing::debug!("MCP: Final {} results after hybrid search", results.len());

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No results found for the query. Try rephrasing your query or using broader terms.",
            )]));
        }

        // Convert to response format, applying compact mode and filter_path
        let items: Vec<SearchResultItem> = results
            .into_iter()
            .filter(|r| {
                // Apply filter_path if specified
                if let Some(ref fp) = request.filter_path {
                    let normalized_path = crate::cache::normalize_path_str(&r.path);
                    let normalized_path = normalized_path.trim_start_matches("./");
                    let normalized_filter = crate::cache::normalize_path_str(fp);
                    let normalized_filter = normalized_filter
                        .trim_start_matches("./")
                        .trim_end_matches('/');
                    normalized_path.starts_with(normalized_filter)
                } else {
                    true
                }
            })
            .map(|r| SearchResultItem {
                path: r.path,
                start_line: r.start_line,
                end_line: r.end_line,
                kind: r.kind,
                score: r.score,
                signature: r.signature,
                content: if compact { None } else { Some(r.content) },
                context_prev: if compact { None } else { r.context_prev },
                context_next: if compact { None } else { r.context_next },
            })
            .collect();

        let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }



    #[tool(
        description = "Find all references/usages of a symbol (function, class, method, variable) across the codebase. USE THIS INSTEAD OF GREP when you need to find where a symbol is used — for refactoring, impact analysis, or understanding call sites. Returns compact list of file paths, line numbers, and containing function signatures."
    )]
    async fn find_references(
        &self,
        Parameters(request): Parameters<FindReferencesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let limit = request.limit.unwrap_or(20);

        tracing::debug!(
            "MCP find_references: symbol='{}', limit={}",
            request.symbol,
            limit
        );

        // Ensure database exists
        if let Err(e) = self.ensure_database_exists() {
            return Ok(CallToolResult::success(vec![Content::text(e)]));
        }

        // Open FTS store for full-text search on the symbol name
        let fts_store = match FtsStore::new(&self.db_path) {
            Ok(s) => s,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error opening FTS store: {}. Try re-indexing with 'codesearch index --force'.",
                    e
                ))]));
            }
        };

        // Search FTS for the symbol — returns chunk_id + score
        let fts_results = match fts_store.search(&request.symbol, limit * 2, None) {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error searching for references: {}",
                    e
                ))]));
            }
        };

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No references found for '{}'. The symbol may not be indexed or try a different name.",
                request.symbol
            ))]));
        }

        // Resolve chunk metadata from VectorStore using chunk_ids
        let items: Vec<ReferenceItem> = if let Some(ref stores) = self.shared_stores {
            let store = stores.vector_store.read().await;
            fts_results
                .iter()
                .filter_map(|fts_result| {
                    if let Ok(Some(chunk)) = store.get_chunk(fts_result.chunk_id) {
                        Some(ReferenceItem {
                            path: chunk.path,
                            line: chunk.start_line,
                            kind: chunk.kind,
                            signature: chunk.signature,
                            score: fts_result.score,
                        })
                    } else {
                        None
                    }
                })
                .take(limit)
                .collect()
        } else {
            // Standalone mode — open a new store
            let store = match VectorStore::new(&self.db_path, self.dimensions) {
                Ok(s) => s,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error opening database: {}",
                        e
                    ))]));
                }
            };
            fts_results
                .iter()
                .filter_map(|fts_result| {
                    if let Ok(Some(chunk)) = store.get_chunk(fts_result.chunk_id) {
                        Some(ReferenceItem {
                            path: chunk.path,
                            line: chunk.start_line,
                            kind: chunk.kind,
                            signature: chunk.signature,
                            score: fts_result.score,
                        })
                    } else {
                        None
                    }
                })
                .take(limit)
                .collect()
        };

        let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Get the status of the semantic search index including model info and statistics. Check this before searching to verify the index is ready."
    )]
    async fn index_status(&self) -> Result<CallToolResult, McpError> {
        let indexed = self.db_path.exists();

        if !indexed {
            let response = IndexStatusResponse {
                indexed: false,
                status: "not_indexed".to_string(),
                status_message: "No index found. Run 'codesearch index' or start with --create-index=true to automatically create one.".to_string(),
                total_chunks: 0,
                total_files: 0,
                model: "none".to_string(),
                dimensions: 0,
                max_chunk_id: 0,
                db_path: self.db_path.display().to_string(),
                project_path: self.project_path.display().to_string(),
                error_message: None,
            };
            let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        // Get stats using shared stores if available
        let stats = if let Some(ref stores) = self.shared_stores {
            let store = stores.vector_store.read().await;
            match store.stats() {
                Ok(s) => s,
                Err(e) => {
                    let response = IndexStatusResponse {
                        indexed: false,
                        status: "error".to_string(),
                        status_message: format!("Error getting index stats: {}", e),
                        total_chunks: 0,
                        total_files: 0,
                        model: self.model_type.short_name().to_string(),
                        dimensions: 0,
                        max_chunk_id: 0,
                        db_path: self.db_path.display().to_string(),
                        project_path: self.project_path.display().to_string(),
                        error_message: Some(format!("Error getting stats: {}", e)),
                    };
                    let json =
                        serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                    return Ok(CallToolResult::success(vec![Content::text(json)]));
                }
            }
        } else {
            // Fallback: open a new store (standalone mode)
            let store = match VectorStore::new(&self.db_path, self.dimensions) {
                Ok(s) => s,
                Err(e) => {
                    let response = IndexStatusResponse {
                        indexed: false,
                        status: "error".to_string(),
                        status_message: format!("Error opening database: {}", e),
                        total_chunks: 0,
                        total_files: 0,
                        model: self.model_type.short_name().to_string(),
                        dimensions: 0,
                        max_chunk_id: 0,
                        db_path: self.db_path.display().to_string(),
                        project_path: self.project_path.display().to_string(),
                        error_message: Some(format!("Error opening database: {}", e)),
                    };
                    let json =
                        serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                    return Ok(CallToolResult::success(vec![Content::text(json)]));
                }
            };

            match store.stats() {
                Ok(s) => s,
                Err(e) => {
                    let response = IndexStatusResponse {
                        indexed: false,
                        status: "error".to_string(),
                        status_message: format!("Error getting index stats: {}", e),
                        total_chunks: 0,
                        total_files: 0,
                        model: self.model_type.short_name().to_string(),
                        dimensions: 0,
                        max_chunk_id: 0,
                        db_path: self.db_path.display().to_string(),
                        project_path: self.project_path.display().to_string(),
                        error_message: Some(format!("Error getting stats: {}", e)),
                    };
                    let json =
                        serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                    return Ok(CallToolResult::success(vec![Content::text(json)]));
                }
            }
        };

        // Determine status based on database state
        let (status, status_message) = if stats.total_chunks == 0 {
            (
                "building".to_string(),
                "Index is being built in the background. Searches may fail until indexing completes. Please check back in a few minutes.".to_string(),
            )
        } else {
            (
                "ready".to_string(),
                "Index is ready for searching.".to_string(),
            )
        };

        let response = IndexStatusResponse {
            indexed: stats.indexed,
            status,
            status_message,
            total_chunks: stats.total_chunks,
            total_files: stats.total_files,
            model: self.model_type.short_name().to_string(),
            dimensions: stats.dimensions,
            max_chunk_id: stats.max_chunk_id,
            db_path: self.db_path.display().to_string(),
            project_path: self.project_path.display().to_string(),
            error_message: None,
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Find all available codesearch databases in current directory, parent directories, and globally tracked repositories. Use this to discover which databases are available for searching."
    )]
    async fn find_databases(&self) -> Result<CallToolResult, McpError> {
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let dbs = find_databases().unwrap_or_default();

        let mut response_dbs = Vec::new();

        for db_info in &dbs {
            // Get stats for this database
            let (total_chunks, total_files, model) = if db_info.db_path.exists() {
                // Try to read model from metadata
                let metadata_path = db_info.db_path.join("metadata.json");
                let model_name = if metadata_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&metadata_path) {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                            json.get("model_short_name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string()
                        } else {
                            "unknown".to_string()
                        }
                    } else {
                        "unknown".to_string()
                    }
                } else {
                    "unknown".to_string()
                };

                // Try to get stats - need to infer dimensions from model name
                let dims = match model_name.as_str() {
                    "minilm-l6" | "minilm-l6-q" | "minilm-l12" | "minilm-l12-q" | "bge-small"
                    | "bge-small-q" | "e5-multilingual" => 384,
                    "bge-base" | "jina-code" | "nomic-v1.5" => 768,
                    "bge-large" | "mxbai-large" => 1024,
                    _ => 384, // default
                };

                // Try to get stats
                if let Ok(store) = VectorStore::new(&db_info.db_path, dims) {
                    if let Ok(stats) = store.stats() {
                        (stats.total_chunks, stats.total_files, model_name)
                    } else {
                        (0, 0, model_name)
                    }
                } else {
                    (0, 0, model_name)
                }
            } else {
                (0, 0, "not found".to_string())
            };

            response_dbs.push(DatabaseInfoResponse {
                database_path: db_info.db_path.display().to_string(),
                project_path: db_info.project_path.display().to_string(),
                is_current_directory: db_info.is_current,
                depth_from_current: db_info.depth,
                total_chunks,
                total_files,
                model,
            });
        }

        // Build message based on what was found
        let message = if dbs.is_empty() {
            "❌ No databases found. Run 'codesearch index' to create an index.".to_string()
        } else if dbs.iter().any(|d| d.is_current) {
            format!(
                "✅ Found {} database(s). Current directory has an index.",
                dbs.len()
            )
        } else {
            format!("⚠️  Found {} database(s) in parent/global directories, but not in current directory.", dbs.len())
        };

        let response = FindDatabasesResponse {
            databases: response_dbs,
            message,
            current_directory: current_dir.display().to_string(),
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

// === Server Handler Implementation ===

#[tool_handler]
impl ServerHandler for CodesearchService {
    fn get_info(&self) -> ServerInfo {
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let db_exists = self.db_path.exists();

        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: rmcp::model::Implementation {
                name: "codesearch".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: None,
                icons: None,
                website_url: None,
            },
            instructions: Some(format!(
                r#"codesearch - Semantic Code Search MCP Server

codesearch provides fast, local semantic code search using natural language queries.
Search your codebase by meaning, not just by keywords.

⚠️  IMPORTANT: This MCP server CANNOT index codebases. Indexing must be done manually.
Indexing takes 30-60 seconds and should be done via the CLI: `codesearch index`

AVAILABLE TOOLS:

1. find_databases()
   Find all available databases in current directory, parent directories, and globally.
   Use this FIRST to discover which databases are available.
   Returns: List of databases with paths, stats, and model info.

2. index_status()
   Check if the current index is ready for searching.
   Use this AFTER find_databases() to verify the database is accessible.
   Returns: Index status, stats, model info, and any error messages.

3. semantic_search(query, limit=10, compact=true, filter_path=null)
   Search the codebase using natural language queries.
   By default returns COMPACT results (path, line numbers, kind, signature, score only).
   Set compact=false to include full code content (use sparingly - high token cost).
   Use filter_path to narrow results to a specific directory (e.g., "src/api/").
   Query examples:
     - "where do we handle user authentication?"
     - "how is error logging implemented?"
     - "functions that process payment data"
   Returns: Array of matches with metadata. Use read tool to fetch actual code.

4. find_references(symbol, limit=50)
   Find all usages/call sites of a function, method, class, or type across the codebase.
   ⚠️  USE THIS instead of grep when you need to find where a symbol is used.
   Essential for refactoring — shows all locations that need to change.
   Examples:
     - find_references("authenticate") - Find all calls to authenticate()
     - find_references("UserService") - Find all usages of UserService
     - find_references("handleRequest") - Find all call sites
   Returns: Compact list of file paths, line numbers, kind, and score.

TOKEN-EFFICIENT WORKFLOW (IMPORTANT):

All tools return compact metadata by default to minimize token usage.
Use the read tool to fetch actual code content only for the specific
lines you need. NEVER use grep for finding symbol usages — use
find_references() instead.

RECOMMENDED WORKFLOW:

Step 1: Discover
  find_databases() → index_status()

Step 2: Search (compact — returns metadata only)
  semantic_search("authentication handler")

Step 3: Find related code (compact — returns locations only)
  find_references("authenticate")

Step 4: Read only what you need (targeted)
  Use read tool with exact file path + line numbers from steps 2-3

REFACTORING WORKFLOW:

1. semantic_search("the function to refactor") → find the definition
2. find_references("functionName") → find ALL call sites
3. Read each call site with read tool → understand usage patterns
4. Make changes to definition + all call sites

⚠️  NEVER use grep to find symbol references. Always use find_references().
    grep is only for exact string matching in non-indexed files.

USAGE PATTERNS:

Understanding a New Codebase:
  1. find_databases() → index_status()
  2. semantic_search("main application entry point")
  3. semantic_search("error handling strategy")

Finding Implementation Patterns:
  - semantic_search("how are API endpoints defined?")
  - semantic_search("database model definitions")

Debugging and Analysis:
  - semantic_search("error handling for database operations")
  - find_references("handleError") → find all error handling sites

BEST PRACTICES:

✓ Always call find_databases() first to discover available indexes
✓ Check index_status() before searching to verify the database is ready
✓ Use natural language queries describing concepts, not exact terms
✓ Use find_references() for refactoring — NOT grep
✓ Use filter_path to narrow searches to specific directories
✓ Let compact mode save tokens — read specific lines only when needed
✓ Start with broader queries, then narrow down

✗ Never attempt to index from this MCP server - use CLI instead
✗ Never use grep to find symbol usages — use find_references() instead
✗ Avoid short, vague queries like "auth" or "db"
✗ Don't use compact=false unless you specifically need full code content
✗ Don't search in subfolders expecting a separate index - indexes are project-wide

DATABASE LOCATIONS:

Priority order for database selection:
1. Current directory (.codesearch.db/)
2. Parent directories (up to 5 levels)
3. Globally tracked repositories (~/.codesearch/repos.json)

Current project: {project}
Current database: {db}
Database exists: {exists}
Current directory: {cwd}

ERROR HANDLING:

If you get "No index found" errors:
1. Call find_databases() to see what's available
2. Check if you're in the right directory
3. Verify the user has run 'codesearch index'

If search returns poor results:
1. The index may be stale - ask user to re-run 'codesearch index'
2. Try different query phrasing
3. Check index_status() for any errors

SETUP:

To create an index, the USER must run (not the agent):
  $ cd /path/to/project
  $ codesearch index

Indexing takes 30-60 seconds and cannot be done from the MCP server.

For detailed documentation, visit: https://github.com/flupkede/codesearch

Model: {model}
Dimensions: {dims}
"#,
                project = self.project_path.display(),
                db = self.db_path.display(),
                exists = if db_exists { "✅ Yes" } else { "❌ No" },
                cwd = current_dir.display(),
                model = self.model_type.short_name(),
                dims = self.dimensions
            )),
            ..Default::default()
        }
    }
}

// === Server Entry Point ===

/// Run the MCP server using stdio transport with file watching for live index updates.
///
/// # Multi-instance Support
///
/// When another instance is already running with write access to the same database,
/// this server will automatically start in **readonly mode**:
/// - Searches work normally
/// - No file watching (index won't auto-update)
/// - No incremental refresh
///
/// This allows multiple terminal windows to use codesearch simultaneously.
pub async fn run_mcp_server(
    path: Option<PathBuf>,
    create_index: bool,
    log_level: crate::logger::LogLevel,
    quiet: bool,
    cancel_token: CancellationToken,
) -> Result<()> {
    use rmcp::{transport::stdio, ServiceExt};

    // Set FASTEMBED_CACHE_DIR early (before any embedding work) to ensure fastembed
    // downloads and caches models to ~/.codesearch/models instead of creating
     // .fastembed_cache in the current working directory.
    match crate::constants::get_global_models_cache_dir() {
        Ok(models_dir) => {
            std::env::set_var("FASTEMBED_CACHE_DIR", &models_dir);
        }
        Err(e) => {
            tracing::warn!("Could not set FASTEMBED_CACHE_DIR: {}", e);
        }
    }

    tracing::info!("🚀 Starting codesearch MCP server");

    // Use database discovery to find the best database
    let db_info = find_best_database(path.as_deref())?;

    let (project_path, db_path) = if let Some(info) = db_info {
        (info.project_path, info.db_path)
    } else {
        // No database found
        if !create_index {
            return Err(anyhow::anyhow!(
                "No database found in current directory, parent directories, or globally tracked repositories. \
                 Run 'codesearch index' first to index the codebase, or use --create-index=true flag to automatically create it."
            ));
        }

        // Create minimal database structure to allow server to start immediately
        let effective_path = path
            .as_ref()
            .cloned()
            .unwrap_or(std::env::current_dir()?);

        // Use git root detection to place database in the correct location
        let db_root = crate::index::find_git_root(&effective_path)?
            .unwrap_or_else(|| effective_path.clone());
        let db_path = db_root.join(".codesearch.db");

        tracing::info!("📁 Creating minimal database structure at {}", db_path.display());

        // Create directory
        std::fs::create_dir_all(&db_path)?;

        // Get model info
        let model_type = ModelType::default();
        let model_short_name = model_type.short_name().to_string();
        let model_name = format!("{:?}", model_type);
        let dimensions = model_type.dimensions();

        // Create minimal metadata.json (matching format used by build_index)
        let metadata_path = db_path.join("metadata.json");
        let metadata = serde_json::json!({
            "model_short_name": model_short_name,
            "model_name": model_name,
            "dimensions": dimensions,
            "indexed_at": chrono::Utc::now().to_rfc3339()
        });
        tokio::fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?).await?;

        // Create minimal file_meta.json (matching FileMetaStore format)
        let file_meta = crate::cache::FileMetaStore::new(model_short_name.clone(), dimensions);
        file_meta.save(&db_path)?;

        // Create FTS directory
        let fts_path = db_path.join("fts");
        std::fs::create_dir_all(&fts_path)?;

        // Create LMDB file by opening VectorStore (creates minimal structure)
        let _store = crate::vectordb::VectorStore::new(&db_path, dimensions)?;

        tracing::info!("✅ Minimal database created successfully");
        tracing::info!("🔄 Background indexing will begin shortly via incremental refresh");

        (effective_path, db_path)
    };

    // Initialize file logger now that db_path is known (works for both existing and auto-created DB)
    // NOTE: For MCP, tracing is NOT initialized in main.rs — this is the only init call
    if let Err(e) = crate::logger::init_logger(&db_path, log_level, quiet) {
        tracing::warn!("Failed to initialize file logger: {}", e);
    }

    tracing::info!("📂 Project: {}", project_path.display());
    tracing::info!("💾 Database: {}", db_path.display());

    // Read model metadata to get dimensions (fallback to 384 if missing/corrupt)
    let metadata_path = db_path.join("metadata.json");
    let dimensions = if metadata_path.exists() {
        match std::fs::read_to_string(&metadata_path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|j| j.get("dimensions").and_then(|v| v.as_u64()))
        {
            Some(d) => d as usize,
            None => {
                tracing::warn!("⚠️  Could not parse dimensions from metadata.json, using default 384");
                384
            }
        }
    } else {
        tracing::warn!("⚠️  metadata.json not found, using default dimensions 384");
        384
    };

    // Create shared stores - try write mode first, fall back to readonly if locked
    // This enables multiple terminal windows to use the same database
    tracing::info!("📦 Creating shared stores...");
    let (shared_stores, is_readonly) = SharedStores::new_or_readonly(&db_path, dimensions)?;
    let shared_stores = Arc::new(shared_stores);

    if is_readonly {
        tracing::warn!("🔒 Running in READONLY mode (another instance has write access)");
        tracing::warn!("   ↳ Searches work normally, but index won't auto-update");
        tracing::warn!("   ↳ Close the other instance to enable write mode");
    }

    // Create MCP service with shared stores (ready immediately)
    let service = CodesearchService::new_with_stores(
        Some(project_path.clone()),
        Some(shared_stores.clone()),
    )?;

    tracing::info!("🧠 Model: {}", service.model_type.name());

    // START MCP SERVER NOW - fixes timeout!
    tracing::info!(
        "🚀 Starting MCP server{}...",
        if is_readonly { " (readonly)" } else { "" }
    );
    let server = service.serve(stdio()).await?;

    tracing::info!("MCP server ready. Waiting for requests...");

    // Only run background tasks if we have write access
    if !is_readonly {
        // Create IndexManager with shared stores (skip initial refresh - do in background)
        tracing::info!("🔍 Initializing index manager...");
        let index_manager =
            IndexManager::new_without_refresh(&project_path, shared_stores.clone()).await?;

        // Background: refresh FIRST, then file watcher (sequential, not concurrent)
        // Both write to SharedStores, so they must not run concurrently
        let project_path_clone = project_path.clone();
        let db_path_clone = db_path.clone();
        let shared_stores_clone = shared_stores.clone();
        let index_manager_arc = Arc::new(index_manager);
        let bg_cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            // Step 0: Pre-start FSW to collect file change events during refresh
            // This ensures changes made while the refresh is running are not missed
            if let Err(e) = index_manager_arc.start_watching().await {
                tracing::warn!("⚠️ Could not pre-start file watcher: {}", e);
            }

            // Step 1: Run initial refresh (writes to stores)
            tracing::info!("🔄 Starting background incremental refresh...");
            match IndexManager::perform_incremental_refresh_with_stores(
                &project_path_clone,
                &db_path_clone,
                &shared_stores_clone,
            )
            .await
            {
                Ok(_) => {
                    tracing::info!("✅ Background incremental refresh completed");

                    // Check if shutdown was requested during refresh
                    if bg_cancel_token.is_cancelled() {
                        tracing::info!("🛑 Shutdown requested, skipping file watcher startup");
                        return;
                    }

                    // Step 2: AFTER refresh completes, start file watcher (also writes to stores)
                    tracing::info!("👀 Starting file watcher...");
                    if let Err(e) = index_manager_arc.start_file_watcher(bg_cancel_token).await {
                        tracing::error!("❌ Failed to start file watcher: {}", e);
                    } else {
                        tracing::info!(
                            "✅ File watcher active - index will auto-update on file changes"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!("❌ Background incremental refresh failed: {}", e);
                }
            }
        });

        // Start periodic log cleanup task
        let db_path_for_cleanup = db_path.clone();
        let cleanup_cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            use crate::logger::{cleanup_old_logs, LogRotationConfig};

            // Run initial cleanup on startup
            let rotation_config = LogRotationConfig::from_env();
            tracing::info!("🧹 Running initial log cleanup...");
            if let Err(e) = cleanup_old_logs(&db_path_for_cleanup, &rotation_config) {
                tracing::warn!("Initial log cleanup failed: {}", e);
            }

            // Start periodic cleanup task (every 24 hours by default)
            crate::logger::start_cleanup_task(
                db_path_for_cleanup.clone(),
                rotation_config,
                cleanup_cancel_token,
            );
        });
    } else {
        tracing::info!("📖 Readonly mode: skipping background refresh and file watcher");
    }

    // Wait for shutdown: either MCP transport closes or cancellation token fires
    tokio::select! {
        result = server.waiting() => {
            tracing::info!("MCP server transport closed");
            result?;
        }
        _ = cancel_token.cancelled() => {
            tracing::info!("🛑 Shutdown signal received, stopping MCP server...");
        }
    }

    tracing::info!("✅ MCP server shut down cleanly");
    Ok(())
}
