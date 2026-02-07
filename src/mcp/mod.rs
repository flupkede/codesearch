//! MCP (Model Context Protocol) server for Claude Code integration
//!
//! Exposes codesearch's semantic search capabilities via the MCP protocol,
//! allowing AI assistants like Claude to search codebases during conversations.

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
use crate::embed::{EmbeddingService, ModelType};
use crate::fts::FtsStore;
use crate::index::{IndexManager, SharedStores};
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
            let mt = ModelType::from_str(model_name).unwrap_or_default();
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
        let results = if let Some(ref stores) = self.shared_stores {
            // Use shared store with read lock
            let store = stores.vector_store.read().await;
            match store.search(&query_embedding, limit) {
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
            match store.search(&query_embedding, limit) {
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

        tracing::debug!("MCP: Found {} results", results.len());

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
                    let normalized_path = r.path.trim_start_matches("./");
                    let normalized_filter = fp.trim_start_matches("./").trim_end_matches('/');
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
        description = "Get all indexed chunks from a specific file. Returns compact metadata by default (path, line numbers, kind, signature). Useful for understanding file structure before using the read tool for specific sections."
    )]
    async fn get_file_chunks(
        &self,
        Parameters(request): Parameters<GetFileChunksRequest>,
    ) -> Result<CallToolResult, McpError> {
        let compact = request.compact.unwrap_or(true);
        // Ensure database exists
        if let Err(e) = self.ensure_database_exists() {
            return Ok(CallToolResult::success(vec![Content::text(e)]));
        }

        // Get chunks using shared stores if available
        let file_chunks = if let Some(ref stores) = self.shared_stores {
            let store = stores.vector_store.read().await;
            let stats = match store.stats() {
                Ok(s) => s,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error getting stats: {}",
                        e
                    ))]));
                }
            };

            // Collect chunks for the requested file
            let mut file_chunks: Vec<SearchResultItem> = Vec::new();
            for id in 0..stats.total_chunks as u32 {
                if let Ok(Some(chunk)) = store.get_chunk(id) {
                    // Normalize paths for comparison
                    let chunk_path = chunk.path.trim_start_matches("./");
                    let req_path = request.path.trim_start_matches("./");

                    if chunk_path == req_path || chunk.path == request.path {
                        file_chunks.push(SearchResultItem {
                            path: chunk.path,
                            start_line: chunk.start_line,
                            end_line: chunk.end_line,
                            kind: chunk.kind,
                            score: 1.0,
                            signature: chunk.signature,
                            content: if compact { None } else { Some(chunk.content) },
                            context_prev: if compact { None } else { chunk.context_prev },
                            context_next: if compact { None } else { chunk.context_next },
                        });
                    }
                }
            }
            file_chunks
        } else {
            // Fallback: open a new store (standalone mode)
            let store = match VectorStore::new(&self.db_path, self.dimensions) {
                Ok(s) => s,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error opening database: {}",
                        e
                    ))]));
                }
            };

            let stats = match store.stats() {
                Ok(s) => s,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error getting stats: {}",
                        e
                    ))]));
                }
            };

            // Collect chunks for the requested file
            let mut file_chunks: Vec<SearchResultItem> = Vec::new();
            for id in 0..stats.total_chunks as u32 {
                if let Ok(Some(chunk)) = store.get_chunk(id) {
                    // Normalize paths for comparison
                    let chunk_path = chunk.path.trim_start_matches("./");
                    let req_path = request.path.trim_start_matches("./");

                    if chunk_path == req_path || chunk.path == request.path {
                        file_chunks.push(SearchResultItem {
                            path: chunk.path,
                            start_line: chunk.start_line,
                            end_line: chunk.end_line,
                            kind: chunk.kind,
                            score: 1.0,
                            signature: chunk.signature,
                            content: if compact { None } else { Some(chunk.content) },
                            context_prev: if compact { None } else { chunk.context_prev },
                            context_next: if compact { None } else { chunk.context_next },
                        });
                    }
                }
            }
            file_chunks
        };

        // Sort by start line
        let mut file_chunks = file_chunks;
        file_chunks.sort_by_key(|c| c.start_line);

        if file_chunks.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No chunks found for file: {}. The file may not be indexed or the path may be incorrect.",
                request.path
            ))]));
        }

        let json = serde_json::to_string(&file_chunks).unwrap_or_else(|_| "[]".to_string());
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
        let fts_results = match fts_store.search(&request.symbol, limit * 2) {
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
                total_chunks: 0,
                total_files: 0,
                model: "none".to_string(),
                dimensions: 0,
                db_path: self.db_path.display().to_string(),
                project_path: self.project_path.display().to_string(),
                error_message: Some(
                    "No index found. Run 'codesearch index' first to create the index.".to_string(),
                ),
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
                        total_chunks: 0,
                        total_files: 0,
                        model: self.model_type.short_name().to_string(),
                        dimensions: 0,
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
                        total_chunks: 0,
                        total_files: 0,
                        model: self.model_type.short_name().to_string(),
                        dimensions: 0,
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
                        total_chunks: 0,
                        total_files: 0,
                        model: self.model_type.short_name().to_string(),
                        dimensions: 0,
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

        let response = IndexStatusResponse {
            indexed: stats.indexed,
            total_chunks: stats.total_chunks,
            total_files: stats.total_files,
            model: self.model_type.short_name().to_string(),
            dimensions: stats.dimensions,
            db_path: self.db_path.display().to_string(),
            project_path: self.project_path.display().to_string(),
            error_message: None,
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Find all available codesearch databases in the current directory, parent directories, and globally tracked repositories. Use this to discover which databases are available for searching."
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

5. get_file_chunks(path, compact=true)
   Get all indexed chunks from a specific file.
   Useful for understanding the structure of a file (functions, classes, methods).
   By default returns COMPACT metadata only. Set compact=false for full content.
   Returns: Chunks with metadata. Use read tool to fetch actual code.

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
  4. get_file_chunks("src/main.rs") → see file structure

Finding Implementation Patterns:
  - semantic_search("how are API endpoints defined?")
  - semantic_search("database model definitions")
  - get_file_chunks("src/models/user.rs") → see structure, read for details

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
pub async fn run_mcp_server(path: Option<PathBuf>, cancel_token: CancellationToken) -> Result<()> {
    use rmcp::{transport::stdio, ServiceExt};

    tracing::info!("🚀 Starting codesearch MCP server");

    // Use database discovery to find the best database
    let db_info = find_best_database(path.as_deref())?;

    if db_info.is_none() {
        return Err(anyhow::anyhow!(
            "No database found in current directory, parent directories, or globally tracked repositories. \
             Run 'codesearch index' first to index the codebase."
        ));
    }

    let db_info = db_info.unwrap();
    let project_path = db_info.project_path.clone();
    let db_path = db_info.db_path.clone();

    tracing::info!("📂 Project: {}", project_path.display());
    tracing::info!("💾 Database: {}", db_path.display());

    // Read model metadata to get dimensions
    let metadata_path = db_path.join("metadata.json");
    let dimensions = if metadata_path.exists() {
        let content = std::fs::read_to_string(&metadata_path)?;
        let json: serde_json::Value = serde_json::from_str(&content)?;
        json.get("dimensions")
            .and_then(|v| v.as_u64())
            .unwrap_or(384) as usize
    } else {
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
                    if let Err(e) = index_manager_arc
                        .start_file_watcher(bg_cancel_token)
                        .await
                    {
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
