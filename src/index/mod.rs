use anyhow::Result;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::cache::FileMetaStore;
use crate::chunker::SemanticChunker;
use crate::db_discovery::{find_best_database, register_repository, unregister_repository};
use crate::embed::{EmbeddingService, ModelType};
use crate::file::FileWalker;
use crate::fts::FtsStore;
use crate::vectordb::VectorStore;

// Index manager module
mod manager;
pub use manager::{IndexManager, SharedStores};

/// Get the database path and project path for a given directory
/// Uses automatic database discovery to find indexes in parent/global directories
fn get_db_path(path: Option<PathBuf>) -> Result<(PathBuf, PathBuf)> {
    use crate::db_discovery::resolve_database_with_message;
    resolve_database_with_message(path.as_deref(), "indexing")
}

/// Smart database path resolution that handles global/local/force scenarios
/// Ensures only ONE database per repository (local or global, never both)
///
/// # Safety Checks
/// - Detects git/hg/svn roots to prevent indexing subdirs
/// - Warns if trying to create a db in a non-root directory
fn get_db_path_smart(
    path: Option<PathBuf>,
    global: bool,
    force: bool,
) -> Result<(PathBuf, PathBuf)> {
    let target = path.as_deref();
    let project_path = path.as_deref().unwrap_or(Path::new("."));

    // Try to canonicalize, but fall back to original path if it fails
    let canonical_path = project_path
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(project_path));

    // Step 1: Check if there's an existing database (local or global)
    let existing_db = find_best_database(target)?;

    // Step 2: Handle --force flag
    if force {
        if let Some(ref db_info) = existing_db {
            // Delete existing database (local or global)
            println!(
                "{}",
                format!(
                    "🗑️  Force rebuild: deleting existing database at {}",
                    db_info.db_path.display()
                )
                .yellow()
            );
            std::fs::remove_dir_all(&db_info.db_path)?;
            println!("✅ Existing database deleted");
        }
        // After deletion, continue to create new database
    }

    // Step 3: Handle --global flag
    if global {
        // User explicitly wants global database
        if let Some(ref db_info) = existing_db {
            if !force && db_info.is_global {
                // Global database already exists, use it
                println!(
                    "{}",
                    format!(
                        "🌍 Using existing global database: {}",
                        db_info.db_path.display()
                    )
                    .dimmed()
                );
                return Ok((db_info.db_path.clone(), db_info.project_path.clone()));
            } else if !force && !db_info.is_global {
                // Local database exists but user wants global
                println!(
                    "{}",
                    format!(
                        "⚠️  Local database exists at {}\n   Moving to global database...",
                        db_info.db_path.display()
                    )
                    .yellow()
                );
                // Delete local database
                std::fs::remove_dir_all(&db_info.db_path)?;
                println!("✅ Local database removed");
            }
        }
        // Create or use global database
        return get_global_db_path(path);
    }

    // Step 4: Use automatic discovery (default behavior)
    if let Some(ref db_info) = existing_db {
        // Use existing database (local or global)
        if !db_info.is_current {
            let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let relative_path = if let Ok(rel) = current_dir.strip_prefix(&db_info.project_path) {
                format!("./{}", rel.display())
            } else {
                db_info.project_path.display().to_string()
            };
            println!(
                "{}",
                format!(
                    "📂 Using database from: {}\n   (indexing from subfolder, project root: {})",
                    db_info.db_path.display(),
                    relative_path
                )
                .dimmed()
            );
        }
        return Ok((db_info.db_path.clone(), db_info.project_path.clone()));
    }

    // Step 5: No existing database - SAFETY CHECK before creating
    // Detect if we're in a subdirectory of a project (git/hg/svn root detection)
    let project_root = find_project_root(&canonical_path);

    if let Some(root) = project_root {
        if root != canonical_path {
            // We're in a subdirectory of a project!
            println!(
                "{}",
                format!(
                    "⚠️  You are in a subdirectory: {}\n   Project root detected at: {}",
                    canonical_path.display(),
                    root.display()
                )
                .yellow()
            );
            println!(
                "{}",
                "   Creating database at project root to avoid duplicate indexes.".yellow()
            );
            let db_path = root.join(".codesearch.db");
            return Ok((db_path, root));
        }
    } else {
        // No project markers found - warn the user
        println!(
            "{}",
            format!(
                "ℹ️  No project root detected (no .git, Cargo.toml, package.json, etc.)\n   Creating database in: {}",
                canonical_path.display()
            ).dimmed()
        );
        println!(
            "{}",
            "   Tip: If this is a subdirectory, run 'codesearch index' from the project root."
                .dimmed()
        );
    }

    // Step 6: Create local database in current directory
    let db_path = canonical_path.join(".codesearch.db");
    Ok((db_path, canonical_path))
}

/// Find the project root by looking for version control directories
/// Returns the directory containing .git, .hg, .svn, or Cargo.toml/package.json
fn find_project_root(start_path: &Path) -> Option<PathBuf> {
    // Project markers in order of priority
    let markers = [
        ".git",           // Git repository
        ".hg",            // Mercurial repository
        ".svn",           // Subversion repository
        "Cargo.toml",     // Rust project
        "package.json",   // Node.js project
        "pyproject.toml", // Python project
        "go.mod",         // Go project
        ".sln",           // .NET solution (check for any .sln file)
    ];

    let mut current = start_path.to_path_buf();

    loop {
        // Check for project markers
        for marker in &markers {
            let marker_path = current.join(marker);
            if marker_path.exists() {
                return Some(current);
            }
        }

        // Also check for .sln files (glob pattern)
        if let Ok(entries) = std::fs::read_dir(&current) {
            for entry in entries.flatten() {
                if let Some(ext) = entry.path().extension() {
                    if ext == "sln" {
                        return Some(current);
                    }
                }
            }
        }

        // Move to parent directory
        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        } else {
            break;
        }
    }

    None
}

/// Get the global database path for a given directory
/// Uses ~/.codesearch.dbs/<project_name>/ for storage
fn get_global_db_path(path: Option<PathBuf>) -> Result<(PathBuf, PathBuf)> {
    use dirs::home_dir;

    let project_path = path.unwrap_or_else(|| PathBuf::from("."));
    let canonical_path = project_path.canonicalize()?;

    // Create a unique name for the project based on its path
    // Use the directory name as the project identifier
    let project_name = canonical_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    // Create global database directory
    let home = home_dir().ok_or_else(|| anyhow::anyhow!("No home directory found"))?;
    let global_db_dir = home.join(".codesearch.dbs").join(project_name);
    let db_path = global_db_dir.join(".codesearch.db");

    // Register this repository in the global tracking
    register_repository(&canonical_path)?;

    println!(
        "{}",
        format!(
            "🌍 Using global database: {}\n   (project: {})",
            db_path.display(),
            project_name
        )
        .dimmed()
    );

    Ok((db_path, canonical_path))
}

/// Index a repository
///
/// # Arguments
/// * `path` - Path to index (defaults to current directory)
/// * `dry_run` - Preview what would be indexed without indexing
/// * `force` - Delete existing index and rebuild from scratch
/// * `global` - Create global index instead of local
/// * `model` - Override embedding model
/// * `quiet` - Suppress verbose output (for server/MCP mode)
pub async fn index(
    path: Option<PathBuf>,
    dry_run: bool,
    force: bool,
    global: bool,
    model: Option<ModelType>,
    cancel_token: CancellationToken,
) -> Result<()> {
    index_with_options(path, dry_run, force, global, model, false, cancel_token).await
}

/// Index a repository with quiet mode option (for server/MCP use)
pub async fn index_quiet(path: Option<PathBuf>, force: bool, cancel_token: CancellationToken) -> Result<()> {
    index_with_options(path, false, force, false, None, true, cancel_token).await
}

/// Internal index function with all options
async fn index_with_options(
    path: Option<PathBuf>,
    dry_run: bool,
    force: bool,
    global: bool,
    model: Option<ModelType>,
    quiet: bool,
    cancel_token: CancellationToken,
) -> Result<()> {
    let (db_path, project_path) = get_db_path_smart(path, global, force)?;
    let model_type = model.unwrap_or_default();

    // Macro to conditionally print
    macro_rules! log_print {
        ($($arg:tt)*) => {
            if !quiet {
                println!($($arg)*);
            }
        };
    }

    log_print!("{}", "🚀 Codesearch Indexer".bright_cyan().bold());
    log_print!("{}", "=".repeat(60));
    log_print!("📂 Project: {}", project_path.display());
    log_print!("💾 Database: {}", db_path.display());
    log_print!(
        "🧠 Model: {} ({} dims)",
        model_type.name(),
        model_type.dimensions()
    );

    if dry_run {
        log_print!("\n{}", "🔍 DRY RUN MODE".bright_yellow());
    }

    // Phase 1: File Discovery
    log_print!("\n{}", "Phase 1: File Discovery".bright_cyan());
    log_print!("{}", "-".repeat(60));

    let start = Instant::now();
    let walker = FileWalker::new(project_path.clone());
    let (mut files, stats) = walker.walk()?;
    let discovery_duration = start.elapsed();

    log_print!(
        "✅ Found {} indexable files in {:?}",
        files.len(),
        discovery_duration
    );
    log_print!("   Total files scanned: {}", stats.total_files);
    log_print!("   Binary/skipped: {}", stats.skipped_binary);
    log_print!("   Total size: {:.2} MB", stats.total_size_mb());

    if files.is_empty() {
        log_print!("\n{}", "No files to index!".yellow());
        return Ok(());
    }

    if dry_run {
        log_print!("\n{}", "Dry run complete!".green());
        return Ok(());
    }

    let is_incremental = db_path.exists() && !force;

    // Load FileMetaStore for incremental indexing (will be used later to update metadata)
    let mut file_meta_store = if is_incremental {
        log_print!("\n{}", "📊 Incremental Indexing".bright_cyan());
        log_print!("{}", "-".repeat(60));

        Some(FileMetaStore::load_or_create(
            &db_path,
            model_type.name(),
            model_type.dimensions(),
        )?)
    } else {
        None
    };

    if is_incremental {
        let file_meta_store = file_meta_store.as_mut().unwrap();

        // Find changed and deleted files
        let mut changed_files = Vec::new();
        let mut unchanged_files = 0;

        for file in &files {
            let (needs_reindex, _old_chunk_ids) = file_meta_store.check_file(&file.path)?;

            if needs_reindex {
                changed_files.push(file.clone());
                debug!("📝 File changed (needs reindex): {}", file.path.display());
            } else {
                unchanged_files += 1;
                debug!("✅ File unchanged: {}", file.path.display());
            }
        }

        // Find deleted files (in metadata but not on disk)
        let deleted_files = file_meta_store.find_deleted_files();

        for (file_path, _chunk_ids) in &deleted_files {
            debug!("🗑️  File deleted from disk: {}", file_path);
        }

        log_print!("   Unchanged files: {}", unchanged_files);
        log_print!("   Changed files: {}", changed_files.len());
        log_print!("   Deleted files: {}", deleted_files.len());

        // If no changes and no deleted files, we're done
        if changed_files.is_empty() && deleted_files.is_empty() {
            log_print!("\n{}", "✅ Database is up to date!".green());
            return Ok(());
        }

        // Delete chunks for changed and deleted files
        let mut total_chunks_to_delete = 0u32;
        for (_, chunk_ids) in deleted_files.iter() {
            total_chunks_to_delete += chunk_ids.len() as u32;
        }
        for file in &changed_files {
            let (_, chunk_ids) = file_meta_store.check_file(&file.path)?;
            total_chunks_to_delete += chunk_ids.len() as u32;
        }

        if total_chunks_to_delete > 0 {
            log_print!("\n🔄 Deleting {} old chunks...", total_chunks_to_delete);

            let mut store = VectorStore::new(&db_path, 384)?; // Will load dimensions from DB
            let mut fts_store = FtsStore::new_with_writer(&db_path)?;

            // Delete deleted files' metadata and chunks
            for (file_path, chunk_ids) in deleted_files {
                if !chunk_ids.is_empty() {
                    info!(
                        "🗑️  Deleting {} chunks for deleted file: {}",
                        chunk_ids.len(),
                        file_path
                    );
                    debug!("   File path: {}", file_path);
                    store.delete_chunks(&chunk_ids)?;
                    for chunk_id in &chunk_ids {
                        fts_store.delete_chunk(*chunk_id)?;
                    }
                }
                file_meta_store.remove_file(Path::new(&file_path));
            }

            // Delete changed files' old chunks
            for file in &changed_files {
                let (_, old_chunk_ids) = file_meta_store.check_file(&file.path)?;
                if !old_chunk_ids.is_empty() {
                    let file_path_str = file.path.to_string_lossy().to_string();
                    info!(
                        "🔄 Deleting {} old chunks for changed file: {}",
                        old_chunk_ids.len(),
                        file_path_str
                    );
                    debug!("   File path: {}", file.path.display());
                    store.delete_chunks(&old_chunk_ids)?;
                    for chunk_id in &old_chunk_ids {
                        fts_store.delete_chunk(*chunk_id)?;
                    }
                }
            }

            fts_store.commit()?;

            // Rebuild vector index after deletions - critical for ANN search correctness
            log_print!("🔨 Rebuilding vector index after deletions...");
            store.build_index()?;

            log_print!("✅ Deleted {} chunks", total_chunks_to_delete);

            // Explicitly drop stores to release LMDB memory map before Phase 2
            drop(store);
            drop(fts_store);
        }

        // Only process changed files
        log_print!("\n🔄 Processing {} changed files...", changed_files.len());
        files = changed_files;
    } else {
        // Clear existing database if forcing
        if db_path.exists() && force {
            log_print!("\n{}", "🗑️  Clearing existing database...".yellow());
            std::fs::remove_dir_all(&db_path)?;
        }
    }

    // Phase 2: Semantic Chunking + Embedding + Storage (Streaming)
    // We process files one at a time to keep memory usage low
    log_print!("\n{}", "Phase 2: Semantic Chunking, Embedding & Storage".bright_cyan());
    log_print!("{}", "-".repeat(60));

    let chunking_start = Instant::now();
    let mut chunker = SemanticChunker::new(100, 2000, 10);
    let mut total_chunks = 0;

    let pb = ProgressBar::new(files.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("█▓▒░ "),
    );

    // Initialize embedding model (uses global models cache)
    let cache_dir = crate::constants::get_global_models_cache_dir()?;
    let mut embedding_service =
        EmbeddingService::with_cache_dir(model_type, Some(cache_dir.as_path()))?;

    // Check for shutdown after model loading (can take 5-10 seconds)
    if crate::constants::is_shutdown_requested() || cancel_token.is_cancelled() {
        log_print!("\n{}", "⚠️  Indexing cancelled during model loading".yellow());
        return Ok(());
    }

    // Initialize vector store
    let mut store = VectorStore::new(&db_path, embedding_service.dimensions())?;

    // Initialize FTS store
    let mut fts_store = FtsStore::new_with_writer(&db_path)?;

    // Track chunk IDs per file for metadata (memory efficient: only file paths, not chunk contents)
    let mut file_chunks: std::collections::HashMap<String, Vec<u32>> =
        std::collections::HashMap::new();

    // Arena reset interval: periodically recreate the ONNX session to free
    // arena allocator memory that grows monotonically. Model is on disk, so
    // recreation is fast (~1-2s). Cache is preserved across resets.
    let arena_reset_interval: usize = std::env::var("CODESEARCH_ARENA_RESET_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(crate::constants::DEFAULT_ARENA_RESET_INTERVAL);
    let mut files_since_reset: usize = 0;

    let mut skipped_files = 0;
    let mut cancelled = false;
    for file in &files {
        // Check for cancellation before processing each file
        // Uses BOTH global AtomicBool (set by ctrlc OS handler) AND CancellationToken (for programmatic cancel)
        if crate::constants::is_shutdown_requested() || cancel_token.is_cancelled() {
            cancelled = true;
            break;
        }

        pb.set_message(format!(
            "{}",
            file.path.file_name().unwrap().to_string_lossy()
        ));

        debug!("📄 Processing file: {}", file.path.display());

        // Skip files that aren't valid UTF-8
        let source_code = match std::fs::read_to_string(&file.path) {
            Ok(content) => content,
            Err(_) => {
                debug!("⚠️  Skipping file (invalid UTF-8): {}", file.path.display());
                skipped_files += 1;
                pb.inc(1);
                continue;
            }
        };

        // Phase 2a: Chunk this file only (memory efficient!)
        let chunks = chunker.chunk_semantic(file.language, &file.path, &source_code)?;
        let chunk_count = chunks.len();
        debug!(
            "   Created {} chunks for {}",
            chunk_count,
            file.path.display()
        );

        if chunks.is_empty() {
            pb.inc(1);
            continue;
        }

        // Phase 2b: Embed chunks for this file only (batched internally)
        // If embedding is interrupted by CTRL-C, catch it as cancellation (not error)
        let embedded_chunks = match embedding_service.embed_chunks(chunks) {
            Ok(chunks) => chunks,
            Err(_) if crate::constants::is_shutdown_requested() => {
                cancelled = true;
                break;
            }
            Err(e) => return Err(e),
        };

        // Check cancellation after embedding (most CPU-intensive step)
        if crate::constants::is_shutdown_requested() || cancel_token.is_cancelled() {
            cancelled = true;
            break;
        }

        // Phase 2c: Insert into vector store immediately
        let chunk_ids = store.insert_chunks_with_ids(embedded_chunks.clone())?;

        // Phase 2d: Insert into FTS store immediately
        for (chunk, chunk_id) in embedded_chunks.iter().zip(chunk_ids.iter()) {
            fts_store.add_chunk(
                *chunk_id,
                &chunk.chunk.content,
                &chunk.chunk.path,
                chunk.chunk.signature.as_deref(),
                &format!("{:?}", chunk.chunk.kind),
            )?;
        }

        // Track chunk IDs per file for metadata (only paths and IDs, not chunk content)
        let file_path = file.path.to_string_lossy().to_string();
        file_chunks.insert(file_path, chunk_ids.clone());

        total_chunks += chunk_count;
        files_since_reset += 1;
        pb.inc(1);

        // Periodically recreate ONNX session to free arena allocator memory.
        // Arena memory grows monotonically during inference; the only way to
        // reclaim it is to destroy the session. The embedding cache (Moka)
        // survives across resets, so cached embeddings are not lost.
        if arena_reset_interval > 0 && files_since_reset >= arena_reset_interval {
            debug!(
                "♻️  Resetting ONNX session after {} files to free arena memory",
                files_since_reset
            );
            embedding_service.reset_embedder(Some(cache_dir.as_path()))?;
            files_since_reset = 0;
        }

        // Memory is freed here - chunks/embeddings dropped before next file
    }

    // Handle cancellation: exit quickly without blocking on build_index
    if cancelled {
        pb.finish_with_message("Cancelled!");
        log_print!("\n{}", "⚠️  Indexing cancelled by user".yellow());

        // Free ONNX model memory immediately
        drop(embedding_service);
        drop(chunker);

        // Don't call build_index() — it blocks for 10-30 seconds on large datasets.
        // The database is in a partially written state, user can re-run with --force.
        // Just commit what we have in FTS for consistency.
        if total_chunks > 0 {
            let _ = fts_store.commit(); // best-effort, don't block on error
            log_print!(
                "   Partial progress: {} chunks written (re-run with --force for clean index)",
                total_chunks
            );
        }

        return Ok(());
    }

    // Capture model info before dropping the ONNX model
    let model_short_name = embedding_service.model_short_name().to_string();
    let model_name = embedding_service.model_name().to_string();
    let model_dimensions = embedding_service.dimensions();

    // Free ONNX model + arena allocator memory before final index operations
    // This releases hundreds of MB of inference buffers
    drop(embedding_service);
    drop(chunker);

    // Commit FTS store
    fts_store.commit()?;

    if skipped_files > 0 {
        log_print!("   ⚠️  Skipped {} files (invalid UTF-8)", skipped_files);
    }

    pb.finish_with_message("Done!");
    let chunking_duration = chunking_start.elapsed();

    log_print!(
        "✅ Created and indexed {} chunks in {:?}",
        total_chunks,
        chunking_duration
    );

    if total_chunks == 0 {
        log_print!("\n{}", "No chunks created!".yellow());
        return Ok(());
    }

    // Build vector index (now that all chunks are inserted)
    let storage_start = Instant::now();
    store.build_index()?;

    let _fts_stats = fts_store.stats()?;
    let _storage_duration = storage_start.elapsed();

    // Save model metadata
    let metadata = serde_json::json!({
        "model_short_name": model_short_name,
        "model_name": model_name,
        "dimensions": model_dimensions,
        "indexed_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        db_path.join("metadata.json"),
        serde_json::to_string_pretty(&metadata)?,
    )?;

    // Update FileMetaStore with new chunk IDs (incremental mode)
    if is_incremental {
        // IMPORTANT: Reuse the existing file_meta_store that already contains unchanged files!
        // Don't create a new one - that would lose all unchanged file metadata
        let mut file_meta_store = file_meta_store.take().unwrap();

        // Save FileMetaStore count before moving
        let file_count = file_chunks.len();

        // Update FileMetaStore with new/changed files (unchanged files are already preserved)
        for (file_path, chunk_ids) in file_chunks {
            file_meta_store.update_file(Path::new(&file_path), chunk_ids)?;
        }

        // Save FileMetaStore (includes both unchanged + updated files)
        file_meta_store.save(&db_path)?;

        log_print!(
            "✅ Updated metadata for {} changed files (unchanged files preserved)",
            file_count
        );
    } else {
        // In full index mode, create a fresh FileMetaStore
        let mut file_meta_store =
            FileMetaStore::new(model_type.name().to_string(), model_type.dimensions());

        // Update FileMetaStore
        for (file_path, chunk_ids) in file_chunks {
            file_meta_store.update_file(Path::new(&file_path), chunk_ids)?;
        }

        // Save FileMetaStore
        file_meta_store.save(&db_path)?;
    }

    // Show final stats
    let db_stats = store.stats()?;
    log_print!("\n{}", "📊 Final Statistics".bright_green().bold());
    log_print!("{}", "=".repeat(60));
    log_print!("   Total chunks: {}", db_stats.total_chunks);
    log_print!("   Total files: {}", db_stats.total_files);
    log_print!(
        "   Indexed: {}",
        if db_stats.indexed {
            "✅ Yes"
        } else {
            "❌ No"
        }
    );

    // Calculate database size
    let mut total_size = 0u64;
    for entry in std::fs::read_dir(&db_path)? {
        let entry = entry?;
        total_size += entry.metadata()?.len();
    }
    log_print!(
        "   Database size: {:.2} MB",
        total_size as f64 / (1024.0 * 1024.0)
    );

    log_print!("\n{}", "✨ Indexing complete".bright_green().bold());
    log_print!(
        "   Run {} to search your codebase",
        "codesearch search <query>".bright_cyan()
    );

    Ok(())
}

/// List all indexed repositories
#[allow(dead_code)] // Reserved for 'list' command implementation
pub async fn list() -> Result<()> {
    println!("{}", "📚 Indexed Repositories".bright_cyan().bold());
    println!("{}", "=".repeat(60));

    // TODO: Scan all repositories in ~/.codesearch/repos.json
    // For now just check current directory

    // Check current directory
    let current_dir = std::env::current_dir()?;
    let current_db = current_dir.join(".codesearch.db");

    if current_db.exists() {
        println!("\n{}", "Current Directory:".bright_green());
        print_repo_stats(&current_dir, &current_db)?;
    }

    // TODO: Track indexed repositories globally in ~/.codesearch/repos.json
    // For now, just show current directory

    Ok(())
}

/// Show statistics about the vector database
pub async fn stats(path: Option<PathBuf>) -> Result<()> {
    let (db_path, project_path) = get_db_path(path)?;

    if !db_path.exists() {
        println!("{}", "❌ No database found!".red());
        println!("   Run {} first", "codesearch index".bright_cyan());
        return Ok(());
    }

    println!("{}", "📊 Database Statistics".bright_cyan().bold());
    println!("{}", "=".repeat(60));
    println!("💾 Database: {}", db_path.display());
    println!("📂 Project: {}", project_path.display());

    let store = VectorStore::new(&db_path, 384)?; // We'll need to store dimensions in metadata
    let stats = store.stats()?;

    println!("\n{}", "Vector Store:".bright_green());
    println!("   Total chunks: {}", stats.total_chunks);
    println!("   Total files: {}", stats.total_files);
    println!(
        "   Indexed: {}",
        if stats.indexed { "✅ Yes" } else { "❌ No" }
    );
    println!("   Dimensions: {}", stats.dimensions);

    // Calculate database size
    let mut total_size = 0u64;
    for entry in std::fs::read_dir(&db_path)? {
        let entry = entry?;
        total_size += entry.metadata()?.len();
    }

    println!("\n{}", "Storage:".bright_green());
    println!(
        "   Database size: {:.2} MB",
        total_size as f64 / (1024.0 * 1024.0)
    );
    println!(
        "   Avg per chunk: {:.2} KB",
        (total_size as f64 / stats.total_chunks as f64) / 1024.0
    );

    Ok(())
}

/// Clear the vector database
pub async fn clear(path: Option<PathBuf>, yes: bool) -> Result<()> {
    let (db_path, project_path) = get_db_path(path)?;

    if !db_path.exists() {
        println!("{}", "❌ No database found!".red());
        return Ok(());
    }

    println!("{}", "🗑️  Clear Database".bright_yellow().bold());
    println!("{}", "=".repeat(60));
    println!("💾 Database: {}", db_path.display());
    println!("📂 Project: {}", project_path.display());

    if !yes {
        println!("\n{}", "⚠️  This will delete all indexed data!".yellow());
        print!("Are you sure? (y/N): ");
        use std::io::{self, Write};
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        if !input.trim().eq_ignore_ascii_case("y") {
            println!("{}", "Cancelled.".dimmed());
            return Ok(());
        }
    }

    println!("\n🔄 Removing database...");
    std::fs::remove_dir_all(&db_path)?;

    println!("{}", "✅ Database cleared!".green());

    Ok(())
}

/// Helper to print repository stats
#[allow(dead_code)] // Used by list() function
fn print_repo_stats(repo_path: &Path, db_path: &Path) -> Result<()> {
    println!("   📂 {}", repo_path.display());

    // Try to load stats
    match VectorStore::new(db_path, 384) {
        Ok(store) => match store.stats() {
            Ok(stats) => {
                println!(
                    "      {} chunks in {} files",
                    stats.total_chunks, stats.total_files
                );
            }
            Err(_) => {
                println!("      {}", "Could not load stats".dimmed());
            }
        },
        Err(_) => {
            println!("      {}", "Could not open database".dimmed());
        }
    }

    Ok(())
}

/// Add a repository to the index (creates local or global)
pub async fn add_to_index(path: Option<PathBuf>, global: bool, cancel_token: CancellationToken) -> Result<()> {
    let project_path = path.as_deref().unwrap_or_else(|| Path::new("."));
    let canonical_path = project_path.canonicalize()?;

    println!("{}", "➕ Add to Index".bright_green().bold());
    println!("{}", "=".repeat(60));
    println!("📂 Project: {}", canonical_path.display());

    // Check if ANY index exists (current directory OR parent directories OR global)
    let db_info = find_best_database(path.as_deref())?;

    if let Some(db) = db_info {
        println!("\n{}", "⚠️  An index already exists!".yellow());
        println!("\n{}", "Existing Index:".cyan());
        println!("   Path: {}", db.db_path.display());

        if db.is_global {
            println!("   Type: {}", "Global".bright_green());
        } else if !db.is_current {
            println!("   Type: {} (parent directory)", "Local".bright_green());
        } else {
            println!("   Type: {}", "Local".bright_green());
        }

        println!(
            "\n{}",
            "You cannot create a separate index for a subdirectory.".yellow()
        );
        println!(
            "{}",
            if db.is_global {
                "The global index will be used for all projects."
            } else if !db.is_current {
                "The parent directory index will be used for this subdirectory."
            } else {
                "An index already exists for this project."
            }
        );

        println!("\n{}", "To use the existing index, simply run:".cyan());
        println!("  codesearch index");

        return Err(anyhow::anyhow!(
            "Index already exists in parent or current directory"
        ));
    }

    // Check if any index already exists for THIS directory (not parent)
    let local_db = canonical_path.join(".codesearch.db");
    let has_local = local_db.exists();

    let repos_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .join(".codesearch")
        .join("repos.json");

    let has_global = if repos_path.exists() {
        let content = fs::read_to_string(&repos_path)?;
        if let Ok(repos) =
            serde_json::from_str::<std::collections::HashMap<String, serde_json::Value>>(&content)
        {
            repos.contains_key(canonical_path.to_str().unwrap_or(""))
        } else {
            false
        }
    } else {
        false
    };

    // Conflict checks
    if global && has_local {
        println!("\n{}", "❌ Error: Local index already exists!".red());
        println!("   A local index already exists at: {}", local_db.display());
        println!("   Remove it first with: codesearch index rm");
        return Err(anyhow::anyhow!("Local index exists"));
    }

    if has_local || has_global {
        println!(
            "\n{}",
            "⚠️  Index already exists for this project!".yellow()
        );
        println!("   Local: {}", if has_local { "✅" } else { "❌" });
        println!("   Global: {}", if has_global { "✅" } else { "❌" });
        return Ok(());
    }

    // Create the index
    if global {
        println!("\n{}", "Creating global index...".cyan());
        index(Some(canonical_path.clone()), false, false, true, None, cancel_token.clone()).await?;
        println!("\n{}", "✅ Global index created!".green());
    } else {
        println!("\n{}", "Creating local index...".cyan());
        index(Some(canonical_path.clone()), false, false, false, None, cancel_token).await?;
        println!("\n{}", "✅ Local index created!".green());
    }

    Ok(())
}

/// Remove the index (local or global, auto-detected)
pub async fn remove_from_index(path: Option<PathBuf>) -> Result<()> {
    let project_path = path.unwrap_or_else(|| PathBuf::from("."));
    let canonical_path = project_path.canonicalize()?;

    println!("{}", "➖ Remove Index".bright_red().bold());
    println!("{}", "=".repeat(60));
    println!("📂 Project: {}", canonical_path.display());

    // Check what exists
    let local_db = canonical_path.join(".codesearch.db");
    let has_local = local_db.exists();

    let repos_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .join(".codesearch")
        .join("repos.json");

    let has_global = if repos_path.exists() {
        let content = fs::read_to_string(&repos_path)?;
        if let Ok(repos) =
            serde_json::from_str::<std::collections::HashMap<String, serde_json::Value>>(&content)
        {
            repos.contains_key(canonical_path.to_str().unwrap_or(""))
        } else {
            false
        }
    } else {
        false
    };

    if !has_local && !has_global {
        println!("\n{}", "⚠️  No index found for this project.".yellow());
        return Ok(());
    }

    // If both exist (shouldn't happen), remove local with warning
    if has_local && has_global {
        println!(
            "\n{}",
            "⚠️  Warning: Both local and global indexes exist!".yellow()
        );
        println!("   Removing local index...");
        fs::remove_dir_all(&local_db)?;
        println!("   {}", "✅ Local index removed".green());
        println!("   (Global index remains)");
        return Ok(());
    }

    // Remove whichever exists
    if has_local {
        println!("\n{}", "Removing local index...".cyan());
        // Note: fastembed cache is inside .codesearch.db/fastembed_cache, so it's removed automatically
        fs::remove_dir_all(&local_db)?;
        println!("{}", "✅ Local index removed!".green());
    } else if has_global {
        println!("\n{}", "Removing global index...".cyan());
        unregister_repository(&canonical_path)?;
        println!("{}", "✅ Global index removed!".green());
    }

    Ok(())
}

/// Show index status (local or global)
pub async fn list_index_status() -> Result<()> {
    println!("{}", "📋 Index Status".bright_cyan().bold());
    println!("{}", "=".repeat(60));

    // Try to find the database
    let db_info = find_best_database(Some(Path::new(".")))?;

    if let Some(db) = db_info {
        println!("\n{}", "💾 Database:".cyan());
        println!("   Path: {}", db.db_path.display());

        if db.is_global {
            println!("   Type: {}", "Global".bright_green());
        } else {
            println!("   Type: {}", "Local".bright_green());
        }

        // Show if this is from a parent directory
        if !db.is_current && !db.is_global {
            println!("   {}", "(from parent directory)".dimmed());
        }

        // Get stats
        if let Ok(stats) = get_db_stats(&db.db_path).await {
            println!("   Status: {}", "✅ Indexed".green());
            println!("   Chunks: {}", stats.chunk_count);
            println!("   Size: {:.2} MB", stats.size_mb);
        } else {
            println!("   Status: {}", "⚠️  Could not read database".yellow());
        }
    } else {
        println!("\n{}", "No index found for this project.".dimmed());
        println!("\nCreate an index with:");
        println!("  codesearch index add          # Create local index");
        println!("  codesearch index add -g       # Create global index");
    }

    Ok(())
}

async fn get_db_stats(db_path: &Path) -> Result<DbStats> {
    use crate::vectordb::VectorStore;

    if !db_path.exists() {
        return Ok(DbStats {
            chunk_count: 0,
            size_mb: 0.0,
        });
    }

    // Try to get stats from vector store
    let store = VectorStore::new(db_path, 384)?;
    let stats = store.stats()?;

    // Calculate database size
    let mut total_size = 0u64;
    for entry in std::fs::read_dir(db_path)? {
        let entry = entry?;
        total_size += entry.metadata()?.len();
    }

    Ok(DbStats {
        chunk_count: stats.total_chunks,
        size_mb: total_size as f64 / (1024.0 * 1024.0),
    })
}

struct DbStats {
    chunk_count: usize,
    size_mb: f64,
}
