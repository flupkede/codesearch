use anyhow::Result;
use axum::{
    extract::{Json, State},
    http::StatusCode,
    routing::{get, post},
    Router,
};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;

use crate::cache::FileMetaStore;
use crate::chunker::SemanticChunker;
use crate::db_discovery::find_best_database;
use crate::embed::{EmbeddingService, ModelType};
use crate::file::FileWalker;
use crate::output::set_quiet;
use crate::vectordb::VectorStore;
use crate::watch::{FileEvent, FileWatcher};

/// Shared server state
struct ServerState {
    store: RwLock<VectorStore>,
    embedding_service: Mutex<EmbeddingService>,
    chunker: Mutex<SemanticChunker>,
    file_meta: RwLock<FileMetaStore>,
    root: PathBuf,
    db_path: PathBuf,
}

/// Search request body
#[derive(Debug, Deserialize)]
struct SearchRequest {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    path: Option<String>,
}

fn default_limit() -> usize {
    25
}

/// Search response
#[derive(Debug, Serialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
    query: String,
    took_ms: u64,
}

#[derive(Debug, Serialize)]
struct SearchResult {
    path: String,
    content: String,
    start_line: usize,
    end_line: usize,
    kind: String,
    score: f32,
}

/// Health check response
#[derive(Debug, Serialize)]
struct HealthResponse {
    status: String,
    indexed_files: usize,
    indexed_chunks: usize,
    model: String,
}

/// Index status response
#[derive(Debug, Serialize)]
struct StatusResponse {
    files: usize,
    chunks: usize,
    indexed: bool,
    model: String,
    dimensions: usize,
}

/// Run the background server with live file watching
///
/// Improvements over osgrep:
/// 1. Native Rust HTTP server (axum) - faster than Node.js
/// 2. Built-in file watching with native notify crate
/// 3. Two-level change detection (mtime + hash)
/// 4. Tracks chunk IDs for efficient incremental updates
pub async fn serve(port: u16, path: Option<PathBuf>) -> Result<()> {
    // Find the best database to use
    let db_info = find_best_database(path.as_deref())?;

    if db_info.is_none() {
        return Err(anyhow::anyhow!(
            "No database found in current directory, parent directories, or globally tracked repositories. \
             Run 'codesearch index' first to index the codebase."
        ));
    }

    let db_info = db_info.unwrap();
    let db_path = db_info.db_path;
    let root = db_info.project_path;

    println!("{}", "🚀 Codesearch Server".bright_cyan().bold());
    println!("{}", "=".repeat(60));
    println!("📂 Root: {}", root.display());
    println!("💾 Database: {}", db_path.display());
    println!("🌐 Port: {}", port);

    if db_info.is_global {
        println!("   {}", "(Global index)".dimmed());
    } else if !db_info.is_current {
        println!("   {}", "(Parent directory index)".dimmed());
    }

    // STEP 1: Perform incremental index refresh
    println!("\n🔍 Performing incremental index refresh...");
    crate::index::index_quiet(Some(root.clone()), false).await?;
    println!("✅ Index refresh completed");

    // Initialize embedding service
    let model_type = ModelType::default();
    println!("\n🔄 Loading embedding model...");
    let cache_dir = crate::constants::get_global_models_cache_dir()?;
    let embedding_service = EmbeddingService::with_cache_dir(model_type, Some(&cache_dir))?;
    let dimensions = embedding_service.dimensions();

    // Load or create file metadata store
    let file_meta = FileMetaStore::load_or_create(&db_path, model_type.short_name(), dimensions)?;

    // Open or create vector store
    let store = VectorStore::new(&db_path, dimensions)?;
    let stats = store.stats()?;

    // If database is empty, do initial index
    if stats.total_chunks == 0 {
        println!(
            "\n{}",
            "📦 Database empty, performing initial index...".yellow()
        );
        let (store, file_meta) = initial_index(root.clone(), db_path.clone(), model_type).await?;

        let state = Arc::new(ServerState {
            store: RwLock::new(store),
            embedding_service: Mutex::new(EmbeddingService::with_cache_dir(
                model_type,
                Some(&crate::constants::get_global_models_cache_dir()?),
            )?),
            chunker: Mutex::new(SemanticChunker::new(100, 2000, 10)),
            file_meta: RwLock::new(file_meta),
            root: root.clone(),
            db_path: db_path.clone(),
        });

        // STEP 2: Start background file watcher
        start_server(state, port, root).await
    } else {
        println!(
            "✅ Database loaded: {} chunks from {} files",
            stats.total_chunks, stats.total_files
        );

        let state = Arc::new(ServerState {
            store: RwLock::new(store),
            embedding_service: Mutex::new(embedding_service),
            chunker: Mutex::new(SemanticChunker::new(100, 2000, 10)),
            file_meta: RwLock::new(file_meta),
            root: root.clone(),
            db_path,
        });

        // STEP 2: Start background file watcher
        start_server(state, port, root).await
    }
}

async fn initial_index(
    root: PathBuf,
    db_path: PathBuf,
    model_type: ModelType,
) -> Result<(VectorStore, FileMetaStore)> {
    // Clear existing database if any
    if db_path.exists() {
        std::fs::remove_dir_all(&db_path)?;
    }

    // File discovery
    let walker = FileWalker::new(root.clone());
    let (files, _stats) = walker.walk()?;
    println!("  Found {} files", files.len());

    if files.is_empty() {
        let store = VectorStore::new(&db_path, model_type.dimensions())?;
        let file_meta =
            FileMetaStore::new(model_type.short_name().to_string(), model_type.dimensions());
        return Ok((store, file_meta));
    }

    // Chunking
    let mut chunker = SemanticChunker::new(100, 2000, 10);
    let mut all_chunks = Vec::new();
    let mut file_chunks: HashMap<String, Vec<crate::chunker::Chunk>> = HashMap::new();

    for file in &files {
        let source_code = match std::fs::read_to_string(&file.path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let chunks = chunker.chunk_semantic(file.language, &file.path, &source_code)?;
        let path_str = file.path.to_string_lossy().to_string();
        file_chunks.insert(path_str, chunks.clone());
        all_chunks.extend(chunks);
    }
    println!("  Created {} chunks", all_chunks.len());

    // Embedding
    let cache_dir = crate::constants::get_global_models_cache_dir()?;
    let mut embedding_service = EmbeddingService::with_cache_dir(model_type, Some(&cache_dir))?;
    let embedded_chunks = embedding_service.embed_chunks(all_chunks)?;
    println!("  Generated {} embeddings", embedded_chunks.len());

    // Storage
    let mut store = VectorStore::new(&db_path, model_type.dimensions())?;
    let chunk_ids = store.insert_chunks_with_ids(embedded_chunks)?;
    store.build_index()?;

    // Build file metadata
    let mut file_meta =
        FileMetaStore::new(model_type.short_name().to_string(), model_type.dimensions());

    let mut chunk_id_iter = chunk_ids.iter();
    for file in &files {
        let path_str = file.path.to_string_lossy().to_string();
        if let Some(chunks) = file_chunks.get(&path_str) {
            let ids: Vec<u32> = chunk_id_iter.by_ref().take(chunks.len()).copied().collect();
            file_meta.update_file(&file.path, ids)?;
        }
    }
    file_meta.mark_full_index();
    file_meta.save(&db_path)?;

    println!("  ✅ Initial index complete");

    Ok((store, file_meta))
}

async fn start_server(state: Arc<ServerState>, port: u16, root: PathBuf) -> Result<()> {
    // Start file watcher in background
    let watcher_state = state.clone();
    let watcher_root = root.clone();
    tokio::spawn(async move {
        if let Err(e) = run_file_watcher(watcher_state, watcher_root).await {
            eprintln!("File watcher error: {}", e);
        }
    });

    // Build HTTP router
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/status", get(status_handler))
        .route("/search", post(search_handler))
        .with_state(state);

    let addr = format!("127.0.0.1:{}", port);
    println!("\n{}", "🌐 Server ready!".bright_green().bold());
    println!("  Health: http://{}/health", addr);
    println!("  Search: POST http://{}/search", addr);
    println!("\n{}", "👀 Watching for file changes...".dimmed());

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn run_file_watcher(state: Arc<ServerState>, root: PathBuf) -> Result<()> {
    let mut watcher = FileWatcher::new(root);
    watcher.start(300)?; // 300ms debounce

    loop {
        let events = watcher.wait_for_events(Duration::from_secs(1));

        if events.is_empty() {
            continue;
        }

        println!("\n📁 {} file change(s) detected", events.len());

        // Enable quiet mode during FSW indexing to suppress verbose output
        set_quiet(true);

        for event in events {
            match event {
                FileEvent::Modified(path) => {
                    if let Err(e) = handle_file_modified(&state, &path).await {
                        eprintln!("  ❌ Error processing {}: {}", path.display(), e);
                    }
                }
                FileEvent::Deleted(path) => {
                    if let Err(e) = handle_file_deleted(&state, &path).await {
                        eprintln!("  ❌ Error processing deletion {}: {}", path.display(), e);
                    }
                }
                FileEvent::Renamed(from, to) => {
                    // Treat as delete + create
                    let _ = handle_file_deleted(&state, &from).await;
                    let _ = handle_file_modified(&state, &to).await;
                }
            }
        }

        // Rebuild index after changes
        let mut store = state.store.write().await;
        if !store.is_indexed() {
            store.build_index()?;
        }

        // Save metadata
        let file_meta = state.file_meta.read().await;
        file_meta.save(&state.db_path)?;

        // Disable quiet mode after FSW indexing is complete
        set_quiet(false);
    }
}

async fn handle_file_modified(state: &ServerState, path: &PathBuf) -> Result<()> {
    // Check if file needs re-indexing
    let file_meta = state.file_meta.read().await;
    let (needs_reindex, old_chunk_ids) = file_meta.check_file(path)?;
    drop(file_meta);

    if !needs_reindex {
        return Ok(());
    }

    println!("  📝 Re-indexing: {}", path.display());

    // Delete old chunks if any
    if !old_chunk_ids.is_empty() {
        let mut store = state.store.write().await;
        store.delete_chunks(&old_chunk_ids)?;
    }

    // Read and chunk file
    let source_code = std::fs::read_to_string(path)?;
    let language = crate::file::Language::from_path(path);

    let chunks = {
        let mut chunker = state
            .chunker
            .lock()
            .map_err(|e| anyhow::anyhow!("Chunker mutex poisoned: {}", e))?;
        chunker.chunk_semantic(language, path, &source_code)?
    };

    if chunks.is_empty() {
        // Update metadata with no chunks
        let mut file_meta = state.file_meta.write().await;
        file_meta.update_file(path, vec![])?;
        return Ok(());
    }

    // Embed chunks
    let embedded_chunks = {
        let mut embedding_service = state
            .embedding_service
            .lock()
            .map_err(|e| anyhow::anyhow!("Embedding service mutex poisoned: {}", e))?;
        embedding_service.embed_chunks(chunks)?
    };

    // Insert into store
    let chunk_ids = {
        let mut store = state.store.write().await;
        store.insert_chunks_with_ids(embedded_chunks)?
    };

    // Update metadata
    let mut file_meta = state.file_meta.write().await;
    file_meta.update_file(path, chunk_ids)?;

    Ok(())
}

async fn handle_file_deleted(state: &ServerState, path: &Path) -> Result<()> {
    let mut file_meta = state.file_meta.write().await;

    if let Some(meta) = file_meta.remove_file(path) {
        if !meta.chunk_ids.is_empty() {
            println!(
                "  🗑️  Removing: {} ({} chunks)",
                path.display(),
                meta.chunk_ids.len()
            );
            let mut store = state.store.write().await;
            store.delete_chunks(&meta.chunk_ids)?;
        }
    }

    Ok(())
}

// HTTP Handlers

async fn health_handler(State(state): State<Arc<ServerState>>) -> Json<HealthResponse> {
    let store = state.store.read().await;
    let stats = store.stats().unwrap_or(crate::vectordb::StoreStats {
        total_chunks: 0,
        total_files: 0,
        indexed: false,
        dimensions: 384,
    });

    let file_meta = state.file_meta.read().await;

    Json(HealthResponse {
        status: "ready".to_string(),
        indexed_files: stats.total_files,
        indexed_chunks: stats.total_chunks,
        model: file_meta.model_name.clone(),
    })
}

async fn status_handler(State(state): State<Arc<ServerState>>) -> Json<StatusResponse> {
    let store = state.store.read().await;
    let stats = store.stats().unwrap_or(crate::vectordb::StoreStats {
        total_chunks: 0,
        total_files: 0,
        indexed: false,
        dimensions: 384,
    });

    let file_meta = state.file_meta.read().await;

    Json(StatusResponse {
        files: stats.total_files,
        chunks: stats.total_chunks,
        indexed: stats.indexed,
        model: file_meta.model_name.clone(),
        dimensions: file_meta.dimensions,
    })
}

async fn search_handler(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, (StatusCode, String)> {
    let start = std::time::Instant::now();

    // Embed query
    let query_embedding = {
        let mut embedding_service = state.embedding_service.lock().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Mutex poisoned: {}", e),
            )
        })?;
        embedding_service
            .embed_query(&req.query)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    // Search
    let store = state.store.read().await;
    let results = store
        .search(&query_embedding, req.limit)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Convert to response format
    let search_results: Vec<SearchResult> = results
        .into_iter()
        .filter(|r| {
            // Filter by path if specified
            if let Some(ref path_filter) = req.path {
                r.path.contains(path_filter)
            } else {
                true
            }
        })
        .map(|r| {
            // Make path relative to root
            let rel_path = r
                .path
                .strip_prefix(state.root.to_str().unwrap_or(""))
                .unwrap_or(&r.path)
                .trim_start_matches('/')
                .to_string();

            SearchResult {
                path: rel_path,
                content: truncate_content(&r.content, 200),
                start_line: r.start_line,
                end_line: r.end_line,
                kind: r.kind,
                score: r.score,
            }
        })
        .collect();

    let took_ms = start.elapsed().as_millis() as u64;

    Ok(Json(SearchResponse {
        results: search_results,
        query: req.query,
        took_ms,
    }))
}

fn truncate_content(content: &str, max_len: usize) -> String {
    if content.len() <= max_len {
        content.to_string()
    } else {
        format!("{}...", &content[..max_len])
    }
}
