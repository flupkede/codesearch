use anyhow::Result;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::cache::{normalize_path, safe_canonicalize, FileMetaStore};
use crate::chunker::SemanticChunker;
use crate::db_discovery::{
    find_best_database, is_registered_repository, register_repository, unregister_repository,
};
use crate::embed::{EmbeddingService, ModelType};
use crate::file::FileWalker;
use crate::fts::FtsStore;
use crate::vectordb::{merge_metadata_atomic, VectorStore};

// Index manager module
mod manager;
pub use manager::{
    is_database_locked, CSharpRebuildNotifier, IndexManager, IndexingStatusCallback, SharedStores,
};

/// Ensure the HNSW vector index is built if it was never built in a previous
/// (possibly cancelled) run.
///
/// Returns `true` if the index was rebuilt, `false` if nothing needed to be
/// done (already indexed, or no chunks present). Returns an error only if the
/// database cannot be opened or `build_index` fails; a failure reading stats is
/// treated as a warning and returns `Ok(false)`.
pub(crate) fn ensure_hnsw_index_if_needed(
    db_path: &Path,
    dimensions: usize,
) -> anyhow::Result<bool> {
    let mut vs = VectorStore::new(db_path, dimensions)?;
    match vs.stats() {
        Ok(s) if s.total_chunks > 0 && !s.indexed => {
            vs.build_index()?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(e) => {
            tracing::warn!("could not check vector index status: {}", e);
            Ok(false)
        }
    }
}

/// Update metadata.json with current chunk/file counts so that `status(projects)`
/// can report accurate numbers without opening LMDB.
/// Uses atomic read-modify-write (temp+rename) so a crash never leaves an empty file.
pub(crate) fn update_metadata_stats(db_path: &Path, total_chunks: usize, total_files: usize) {
    if let Err(e) = merge_metadata_atomic(db_path, |obj| {
        obj.insert(
            "total_chunks".to_string(),
            serde_json::Value::Number(total_chunks.into()),
        );
        obj.insert(
            "total_files".to_string(),
            serde_json::Value::Number(total_files.into()),
        );
    }) {
        tracing::warn!("Failed to update metadata stats: {}", e);
    }
}

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

    // Canonicalize and strip any Windows UNC prefix (\\?\) via the central helper.
    let canonical_path =
        safe_canonicalize(project_path).unwrap_or_else(|_| PathBuf::from(project_path));

    // Step 1: Handle --force flag — delete databases
    if force {
        // 1a. First, check for a database directly in the project directory.
        //     This catches incomplete/corrupt databases that find_best_database
        //     would skip (it only returns valid databases).
        //     We always delete the local DB when --force is used from its own directory.
        let local_db = canonical_path.join(crate::constants::DB_DIR_NAME);
        if local_db.is_dir() {
            println!(
                "{}",
                format!(
                    "🗑️  Force rebuild: deleting existing database at {}",
                    local_db.display()
                )
                .yellow()
            );
            std::fs::remove_dir_all(&local_db).map_err(|e| {
                // On Windows, OS error 32 = "file in use by another process".
                // This typically means 'codesearch serve' has the LMDB memory-mapped.
                // The serve's /repos/{alias}/reindex endpoint should have been used instead.
                #[cfg(target_os = "windows")]
                if e.raw_os_error() == Some(32) {
                    return anyhow::anyhow!(
                        "Cannot delete database at {} — it is held open by another process \
                        (likely 'codesearch serve').\n\
                        Hint: 'codesearch index -f' automatically delegates to serve when it is \
                        running. The delegation may have failed because this repo is not registered \
                        in repos.json or the alias could not be resolved.\n\
                        Run 'codesearch serve --register .' to register this repo, then retry.",
                        local_db.display()
                    );
                }
                anyhow::anyhow!("Failed to delete database at {}: {}", local_db.display(), e)
            })?;
            // Wait for Windows to fully release file handles (memory-mapped files
            // from LMDB/tantivy may not be immediately released after deletion)
            std::thread::sleep(std::time::Duration::from_millis(1000));
            println!("✅ Existing database deleted");
        } else {
            // 1b. No local DB — check if find_best_database found one elsewhere.
            //     This handles --global or cases where the DB is in a parent dir.
            let existing_db = find_best_database(target)?;
            if let Some(ref db_info) = existing_db {
                // Safety check: only delete a database that belongs to this project.
                let db_project_normalized = normalize_path(&db_info.project_path);
                let canonical_path_str = normalize_path(&canonical_path);
                let db_is_for_this_project = db_project_normalized == canonical_path_str
                    || canonical_path
                        .as_path()
                        .starts_with(Path::new(&*db_project_normalized));

                if !db_is_for_this_project {
                    anyhow::bail!(
                        "Found database at {} for project '{}', but you are indexing '{}'. \
                         Cowardly refusing to delete another project's database. \
                         If the database is stale, delete it manually or run from the correct directory.",
                        db_info.db_path.display(),
                        db_project_normalized,
                        canonical_path_str
                    );
                }

                println!(
                    "{}",
                    format!(
                        "🗑️  Force rebuild: deleting existing database at {}",
                        db_info.db_path.display()
                    )
                    .yellow()
                );
                std::fs::remove_dir_all(&db_info.db_path)?;
                std::thread::sleep(std::time::Duration::from_millis(1000));
                println!("✅ Existing database deleted");
            }
        }
        // After deletion, continue to create new database
    }

    // Step 2: Check if there's an existing database (for non-force paths)
    let existing_db = if force {
        // Already handled above — DB was deleted, no existing DB to find
        None
    } else {
        find_best_database(target)?
    };

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
    // Skip when --force: old location may be wrong (e.g. not at git root).
    // Let Step 5 (find_git_root) determine the correct location.
    if !force {
        if let Some(db_info) = existing_db.as_ref() {
            // Use existing database (local or global)
            if !db_info.is_current {
                let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let relative_path = if let Ok(rel) = current_dir.strip_prefix(&db_info.project_path)
                {
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
    }

    // Step 5: No existing database - SAFETY CHECK before creating
    // Detect if we're in a subdirectory of a git repository
    // Propagate errors (e.g. multiple child .git dirs found)
    let git_root = find_git_root(&canonical_path)?;

    if let Some(root) = git_root {
        if root != canonical_path {
            // We're in a subdirectory of a git repository!
            crate::output::print_info(format_args!(
                "{}",
                format!(
                    "⚠️  You are in a subdirectory: {}\n   Git repository root detected at: {}",
                    canonical_path.display(),
                    root.display()
                )
                .yellow()
            ));
            crate::output::print_info(format_args!(
                "{}",
                "   Creating database at repository root to avoid duplicate indexes.".yellow()
            ));
            let db_path = root.join(".codesearch.db");
            return Ok((db_path, root));
        }
        // We're at the git root, so fall through to create local database
    }
    // Not in a git repository or at git root - create local database

    // Step 6: Create local database in current directory
    let db_path = canonical_path.join(".codesearch.db");
    Ok((db_path, canonical_path))
}

/// Find the git repository root by looking for .git directory.
/// Searches upward (unlimited), then one level down if nothing found upward.
/// Returns `Ok(None)` if not in a git repo. Returns `Err` if multiple child repos found.
pub(crate) fn find_git_root(start_path: &Path) -> Result<Option<PathBuf>> {
    let mut current = safe_canonicalize(start_path)
        .map_err(|e| anyhow::anyhow!("Failed to canonicalize path: {}", e))?;

    // Search up the directory tree (unlimited levels)
    loop {
        let git_path = current.join(".git");

        if git_path.exists() {
            // Found a .git directory

            // Check if it's a git worktree file or a directory
            if git_path.is_file() {
                // Git worktree: the .git file points into the main repo's
                // .git/worktrees/<n> directory, but the worktree checkout
                // is its own independent working tree and deserves its own
                // codesearch index.  Return the directory that contains the
                // .git file (i.e. the worktree root) as the project root so
                // codesearch never confuses it with the main repo or resolves
                // to .git/worktrees (the old, buggy behaviour).
                return Ok(Some(current.to_path_buf()));
            } else {
                // Normal git repository - return immediately
                return Ok(Some(current.to_path_buf()));
            }
        }

        // Move to parent directory
        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        } else {
            break;
        }
    }

    // Search down one level to check for multiple child .git dirs
    // Only do this if we didn't find any .git directory in the upward search
    if let Ok(entries) = std::fs::read_dir(start_path) {
        let mut child_git_dirs = Vec::new();

        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                let child_git = entry_path.join(".git");
                if child_git.exists() {
                    child_git_dirs.push(entry_path);
                }
            }
        }

        match child_git_dirs.len() {
            0 => {}
            1 => return Ok(Some(child_git_dirs.into_iter().next().unwrap())),
            _ => {
                return Err(anyhow::anyhow!(
                    "❌ Multiple git repositories found in subdirectories:\n  {}\n\n\
                     Cannot create a single index spanning multiple repos.\n\
                     Run 'codesearch index' inside each repository separately.",
                    child_git_dirs
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n  ")
                ));
            }
        }
    }

    // Not in a git repository - return None
    Ok(None)
}

/// Find the project root by looking for version control directories
/// Returns the directory containing .git, .hg, .svn, or Cargo.toml/package.json
#[allow(dead_code)]
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
    let canonical_path = safe_canonicalize(&project_path)?;

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
    // Always try to delegate to a running serve instance via HTTP.
    // This avoids file-lock conflicts between CLI and serve holding the same LMDB.
    if !dry_run {
        // Try to delegate; if serve is unresponsive (warming up), wait and retry.
        let delegate_result = serve_delegate_with_warmup_wait(|| {
            let path = path.clone();
            async move { try_delegate_reindex_to_serve(&path, force).await }
        })
        .await;

        match delegate_result {
            Ok((alias, project_path)) => {
                println!(
                    "{}",
                    format!(
                        "🔄 Delegated reindex to running serve instance (alias: '{}', path: {})",
                        alias,
                        project_path.display()
                    )
                    .bright_cyan()
                );
                return Ok(());
            }
            Err(DelegateError::ServeDown) => {
                // Serve is not running — index locally. Connection-refused is
                // detected immediately, so this fast path is not slowed down.
                debug!("serve not running; indexing locally");
            }
            Err(DelegateError::ServeUnresponsive) => {
                // Serve was reachable but never became ready within the wait budget.
                // Refusing to create a local duplicate — the user must decide.
                return Err(anyhow::anyhow!(
                    "codesearch serve is running but did not become ready within the wait \
                     budget (~2 min). Stop serve first to index locally, or wait for it \
                     to finish warming up."
                ));
            }
            Err(DelegateError::Failed(reason)) => {
                eprintln!(
                    "{}",
                    format!(
                        "⚠️  codesearch serve is running but could not delegate: {}",
                        reason
                    )
                    .yellow()
                );
                eprintln!(
                    "{}",
                    "   Running locally — LMDB file-lock conflicts are possible.".yellow()
                );
            }
        }
    }
    index_with_options(path, dry_run, force, global, model, false, cancel_token).await
}

/// Index a repository with quiet mode option (for server/MCP use)
pub async fn index_quiet(
    path: Option<PathBuf>,
    force: bool,
    global: bool,
    cancel_token: CancellationToken,
) -> Result<()> {
    index_with_options(path, false, force, global, None, true, cancel_token).await
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
            model_type.short_name(),
            model_type.dimensions(),
        )?)
    } else {
        None
    };

    if is_incremental {
        let file_meta_store = file_meta_store.as_mut().unwrap();

        // B1 safety guard: if FileMetaStore is empty but VectorStore has chunks,
        // the metadata was lost/reset. Re-indexing without clearing would create
        // duplicate chunks. Detect and clear before proceeding.
        if file_meta_store.is_empty() {
            let mut vs = VectorStore::new(&db_path, model_type.dimensions())?;
            let existing_chunks = vs.stats().map(|s| s.total_chunks).unwrap_or(0);
            if existing_chunks > 0 {
                log_print!(
                    "{}",
                    format!(
                        "⚠️  FileMetaStore is empty but VectorStore has {} chunks — \
                         clearing to prevent duplicates (metadata was likely lost/reset)",
                        existing_chunks
                    )
                    .yellow()
                );
                vs.clear()?;
                drop(vs);
                // Also clear FTS
                let mut fts = FtsStore::new_with_writer(&db_path)?;
                fts.clear()?;
                drop(fts);
            } else {
                drop(vs);
            }
        }

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
            // Safety net: if a previous run was cancelled/interrupted mid-way,
            // the HNSW vector index may never have been built. Detect this and
            // rebuild now so the database is usable without requiring --force.
            match ensure_hnsw_index_if_needed(&db_path, model_type.dimensions()) {
                Ok(true) => {
                    log_print!(
                        "\n{}",
                        "🔨 Vector index not built from previous run — rebuilt successfully."
                            .yellow()
                    );
                }
                Ok(false) => {} // already indexed or no chunks — all good
                Err(e) => {
                    log_print!(
                        "{}",
                        format!("⚠️  Could not rebuild vector index: {}", e).yellow()
                    );
                }
            }

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
        // Note: database deletion for --force is handled in get_db_path_smart()
        // (including the delay for Windows file handle release). This else branch
        // only runs when not in incremental mode, i.e., fresh index creation.
    }

    // Phase 2: Semantic Chunking + Embedding + Storage (Streaming)
    // We process files one at a time to keep memory usage low
    log_print!(
        "\n{}",
        "Phase 2: Semantic Chunking, Embedding & Storage".bright_cyan()
    );
    log_print!("{}", "-".repeat(60));

    let chunking_start = Instant::now();
    let mut chunker = SemanticChunker::new(100, 2000, 10);
    let mut total_chunks = 0;

    let pb = if quiet {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(files.len() as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {msg}")
                .unwrap()
                .progress_chars("█▓▒░ "),
        );
        pb
    };

    // Initialize embedding model (uses global models cache)
    let cache_dir = crate::constants::get_global_models_cache_dir()?;
    let mut embedding_service =
        EmbeddingService::with_cache_dir(model_type, Some(cache_dir.as_path()))?;

    // Check for shutdown after model loading (can take 5-10 seconds)
    if crate::constants::check_shutdown(&cancel_token) {
        log_print!(
            "\n{}",
            "⚠️  Indexing cancelled during model loading".yellow()
        );
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
    let mut skipped_files: Vec<String> = Vec::new();
    let mut cancelled = false;
    for file in &files {
        // Check for cancellation before processing each file
        // Uses BOTH global AtomicBool (set by ctrlc OS handler) AND CancellationToken (for programmatic cancel)
        if crate::constants::check_shutdown(&cancel_token) {
            cancelled = true;
            break;
        }

        pb.set_message(format!(
            "{}",
            file.path.file_name().unwrap().to_string_lossy()
        ));

        debug!("📄 Processing file: {}", file.path.display());

        // Read file content with encoding fallback (UTF-8 → lossy)
        let source_code = match std::fs::read_to_string(&file.path) {
            Ok(content) => {
                // UTF-8 succeeded
                content
            }
            Err(utf8_err) if utf8_err.kind() == std::io::ErrorKind::InvalidData => {
                // UTF-8 failed — try lossy decode (handles ISO-8859-1, Windows-1252, etc.)
                match std::fs::read(&file.path) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                    Err(read_err) => {
                        skipped_files.push(format!(
                            "{} (read failed: {})",
                            file.path.display(),
                            read_err
                        ));
                        pb.inc(1);
                        continue;
                    }
                }
            }
            Err(e) => {
                // Other error (permission denied, file not found, etc.)
                skipped_files.push(format!("{} ({})", file.path.display(), e));
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
            // Still track this file so we don't re-process it every run.
            // A file with 0 chunks (e.g. minified JS, empty file) is "processed
            // but unchunkable" — record it with an empty chunk list so check_file()
            // returns (unchanged) on future runs and doctor doesn't flag it.
            let path_str = file.path.to_string_lossy().to_string();
            file_chunks.insert(path_str, vec![]);
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
        if crate::constants::check_shutdown(&cancel_token) {
            cancelled = true;
            break;
        }

        // Phase 2c: Extract lightweight FTS data before handing ownership to vector store.
        // We capture just the strings needed for FTS (content, path, signature, kind)
        // so we can pass full EmbeddedChunks to the vector store without cloning.
        let fts_data: Vec<(String, String, Option<String>, String)> = embedded_chunks
            .iter()
            .map(|ec| {
                (
                    ec.chunk.content.clone(),
                    ec.chunk.path.clone(),
                    ec.chunk.signature.clone(),
                    format!("{:?}", ec.chunk.kind),
                )
            })
            .collect();

        // Phase 2d: Insert into vector store (takes ownership, no clone needed)
        let chunk_ids = store.insert_chunks_with_ids(embedded_chunks)?;

        // Phase 2e: Insert into FTS with real chunk IDs from vector store.
        // FTS failures are non-fatal: vector search is the primary search method,
        // FTS (BM25) is supplementary for hybrid search. If tantivy encounters
        // I/O errors (common on Windows due to antivirus interference), we log
        // a warning and continue rather than aborting the entire indexing run.
        for ((content, path, signature, kind), &chunk_id) in fts_data.iter().zip(chunk_ids.iter()) {
            if let Err(e) = fts_store.add_chunk(chunk_id, content, path, signature.as_deref(), kind)
            {
                tracing::warn!(
                    "FTS add_chunk failed in {}: {} (continuing without FTS for this chunk)",
                    file.path.display(),
                    e
                );
            }
        }

        // Track chunk IDs per file for metadata (only paths and IDs, not chunk content)
        let file_path = file.path.to_string_lossy().to_string();
        file_chunks.insert(file_path, chunk_ids.clone());

        total_chunks += chunk_count;
        pb.inc(1);

        // Periodic FTS commit to flush the in-memory segment to disk in a controlled
        // way. Non-fatal: if commit fails, we log and continue. Some FTS data may
        // be lost but vector search (primary) is unaffected.
        if total_chunks % 1000 == 0 && total_chunks > 0 {
            if let Err(e) = fts_store.commit() {
                tracing::warn!(
                    "Periodic FTS commit failed at {} chunks: {} (continuing, some FTS data may be lost)",
                    total_chunks,
                    e
                );
            }
        }

        // Memory is freed here - chunks/embeddings dropped before next file
    }

    // Handle cancellation: still finalize the index properly so the database
    // remains usable. Skipping build_index() was the old behaviour — it left
    // the database in a broken state that a subsequent incremental run could
    // not recover from (no changed files → early return → index never built).
    if cancelled {
        pb.finish_with_message("Cancelled!");
        log_print!(
            "\n{}",
            "⚠️  Indexing cancelled — finalising partial index...".yellow()
        );

        // Free ONNX model memory before build_index (releases hundreds of MB)
        drop(embedding_service);
        drop(chunker);

        // Commit FTS
        if total_chunks > 0 {
            if let Err(e) = fts_store.commit() {
                log_print!("{}   FTS commit warning: {}", "⚠️ ".yellow(), e);
            }
        }
        drop(fts_store);

        // Build vector index from the chunks that were successfully inserted
        if total_chunks > 0 {
            log_print!(
                "   Building vector index for {} partial chunks...",
                total_chunks
            );
            store.build_index()?;
            log_print!("   ✅ Vector index built");
        }

        // Save metadata — best-effort: log and continue on failure so the
        // partial chunks we already built are still searchable.
        // Uses read-modify-write so existing stats (total_chunks/total_files) are preserved.
        if let Err(e) = merge_metadata_atomic(&db_path, |obj| {
            obj.insert(
                "model_short_name".to_string(),
                serde_json::Value::String(model_type.short_name().to_string()),
            );
            obj.insert(
                "model_name".to_string(),
                serde_json::Value::String(model_type.name().to_string()),
            );
            obj.insert(
                "dimensions".to_string(),
                serde_json::Value::Number(model_type.dimensions().into()),
            );
            obj.insert(
                "indexed_at".to_string(),
                serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
            );
            obj.insert("partial".to_string(), serde_json::Value::Bool(true));
        }) {
            log_print!("{}   metadata.json write warning: {}", "⚠️ ".yellow(), e);
        }

        // Update FileMetaStore with the files that were actually processed.
        // Also best-effort: a failed save means the next incremental run will
        // re-process those files, which is acceptable for a cancelled index.
        if !file_chunks.is_empty() {
            let save_result = if is_incremental {
                let mut meta = file_meta_store.take().unwrap();
                for (file_path, chunk_ids) in file_chunks {
                    if let Err(e) = meta.update_file(Path::new(&file_path), chunk_ids) {
                        log_print!(
                            "{}   file-meta update warning for '{}': {}",
                            "⚠️ ".yellow(),
                            file_path,
                            e
                        );
                    }
                }
                meta.save(&db_path)
            } else {
                let mut meta = FileMetaStore::new(
                    model_type.short_name().to_string(),
                    model_type.dimensions(),
                );
                for (file_path, chunk_ids) in file_chunks {
                    if let Err(e) = meta.update_file(Path::new(&file_path), chunk_ids) {
                        log_print!(
                            "{}   file-meta update warning for '{}': {}",
                            "⚠️ ".yellow(),
                            file_path,
                            e
                        );
                    }
                }
                meta.save(&db_path)
            };
            if let Err(e) = save_result {
                log_print!("{}   file-meta save warning: {}", "⚠️ ".yellow(), e);
            }
        }

        // Persist stats — best-effort, only for display; failures are non-fatal.
        match store.stats() {
            Ok(db_stats) => {
                update_metadata_stats(&db_path, db_stats.total_chunks, db_stats.total_files);
                log_print!(
                    "   Partial index finalised: {} chunks, {} files",
                    db_stats.total_chunks,
                    db_stats.total_files
                );
            }
            Err(e) => {
                log_print!("{}   Could not read final stats: {}", "⚠️ ".yellow(), e);
            }
        }
        log_print!(
            "{}   Run {} to index the remaining files",
            "💡 ".cyan(),
            "codesearch index".bright_cyan()
        );

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

    // Commit FTS store (non-fatal: vector search works without FTS)
    if let Err(e) = fts_store.commit() {
        tracing::warn!(
            "Final FTS commit failed: {} (vector search will work, but hybrid/BM25 search may have gaps)",
            e
        );
    }

    if !skipped_files.is_empty() {
        log_print!(
            "   ⚠️  Skipped {} files (failed to read):",
            skipped_files.len()
        );
        for path in &skipped_files {
            log_print!("      - {}", path);
        }
    }

    pb.finish_with_message("Done!");
    let chunking_duration = chunking_start.elapsed();

    log_print!(
        "✅ Created and indexed {} chunks in {:?}",
        total_chunks,
        chunking_duration
    );

    if total_chunks == 0 {
        // Still save file metadata for files that were processed but produced 0 chunks
        // (e.g. minified JS, binary-like files). Without this, those files would be
        // detected as "changed" on every subsequent run and never stabilise.
        if !file_chunks.is_empty() {
            if is_incremental {
                let mut store = file_meta_store.take().unwrap();
                let file_count = file_chunks.len();
                for (file_path, chunk_ids) in file_chunks {
                    store.update_file(Path::new(&file_path), chunk_ids)?;
                }
                store.save(&db_path)?;
                log_print!(
                    "✅ Updated metadata for {} unchunkable files (0 chunks produced)",
                    file_count
                );
            } else {
                let mut store = FileMetaStore::new(
                    model_type.short_name().to_string(),
                    model_type.dimensions(),
                );
                for (file_path, chunk_ids) in file_chunks {
                    store.update_file(Path::new(&file_path), chunk_ids)?;
                }
                store.save(&db_path)?;
            }
        }
        log_print!("\n{}", "No chunks created!".yellow());
        return Ok(());
    }

    // Capture FTS stats before dropping the store to free memory
    let _fts_stats = fts_store.stats()?;

    // Drop FTS store before build_index() to free tantivy memory.
    // FTS is already committed above — keeping the store open during
    // build_index() wastes memory on tantivy's segment readers and buffers.
    drop(fts_store);

    // Build vector index (now that all chunks are inserted)
    let storage_start = Instant::now();
    store.build_index()?;
    let _storage_duration = storage_start.elapsed();

    // Save model metadata (read-modify-write so existing stats are preserved).
    // `partial: false` is explicit so the schema matches the cancel path
    // (which writes `partial: true`); readers can always check the field
    // regardless of how indexing completed.
    merge_metadata_atomic(&db_path, |obj| {
        obj.insert(
            "model_short_name".to_string(),
            serde_json::Value::String(model_short_name.to_string()),
        );
        obj.insert(
            "model_name".to_string(),
            serde_json::Value::String(model_name.to_string()),
        );
        obj.insert(
            "dimensions".to_string(),
            serde_json::Value::Number(model_dimensions.into()),
        );
        obj.insert(
            "indexed_at".to_string(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
        obj.insert("partial".to_string(), serde_json::Value::Bool(false));
    })?;

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
            FileMetaStore::new(model_type.short_name().to_string(), model_type.dimensions());

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

    // Persist chunk/file counts in metadata.json for status(projects)
    update_metadata_stats(&db_path, db_stats.total_chunks, db_stats.total_files);

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
#[allow(dead_code)]
pub async fn list() -> Result<()> {
    use crate::db_discovery::repos::ReposConfig;

    println!("{}", "📚 Indexed Repositories".bright_cyan().bold());
    println!("{}", "=".repeat(60));

    let config = ReposConfig::load().unwrap_or_default();

    if config.repos.is_empty() {
        println!("\n  No repositories registered.");
    } else {
        let mut entries: Vec<_> = config.repos.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));

        for (alias, project_path) in &entries {
            println!();
            println!("  {}", alias.bright_green());
            let db_path = project_path.join(".codesearch.db");
            print_repo_stats(project_path, &db_path)?;
        }

        println!();
        println!("  {} repositories registered.", entries.len());
    }

    // Show a loose local DB if the user is standing in one that is not registered
    let current_dir = std::env::current_dir()?;
    let current_db = current_dir.join(".codesearch.db");
    let current_alias = config.alias_for_path(&current_dir);

    if current_db.exists() && current_alias.is_none() {
        println!();
        println!("{}", "Local (unregistered):".bright_yellow());
        print_repo_stats(&current_dir, &current_db)?;
    }

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
/// Remove stale entries from `repos.json`.
///
/// For each registered repo whose path no longer exists on disk (e.g. its
/// folder was renamed/moved), a best-effort git-identity relocation is tried
/// first; only entries that cannot be relocated are unregistered. Prints a
/// summary of what was relocated/removed.
pub async fn prune_index() -> Result<()> {
    use crate::db_discovery::repos::ReposConfig;

    let mut config = ReposConfig::load()?;
    let (relocated, removed) = config.prune_stale();

    if relocated.is_empty() && removed.is_empty() {
        println!("✅ No stale repositories found — repos.json is clean.");
        return Ok(());
    }

    config.save()?;

    for (alias, path) in &relocated {
        println!("📍 relocated '{}' → {}", alias, path.display());
    }
    for alias in &removed {
        println!("🗑️  removed stale entry '{}'", alias);
    }
    println!(
        "✅ Prune complete: {} relocated, {} removed.",
        relocated.len(),
        removed.len()
    );

    Ok(())
}

pub async fn add_to_index(
    path: Option<PathBuf>,
    global: bool,
    model: Option<ModelType>,
    cancel_token: CancellationToken,
) -> Result<()> {
    let project_path = path.as_deref().unwrap_or_else(|| Path::new("."));
    let canonical_path = safe_canonicalize(project_path)?;

    println!("{}", "➕ Add to Index".bright_green().bold());
    println!("{}", "=".repeat(60));
    println!("📂 Project: {}", canonical_path.display());

    // Try delegating to a running serve instance first.
    // Serve handles: register in repos.json + create index + warmup.
    let add_delegate = serve_delegate_with_warmup_wait(|| {
        let path = path.clone();
        // Alias is always derived from the directory name; the CLI no longer
        // lets the user set it. Pass None so serve derives it consistently.
        async move { try_delegate_add_to_serve(&path, &None, global, &model).await }
    })
    .await;

    match add_delegate {
        Ok((assigned_alias, _)) => {
            println!("\n{}", "✅ Delegated to running serve instance.".green());
            println!("   Registered as '{}'.", assigned_alias);
            println!("   Index creation running in background on the server.");
            return Ok(());
        }
        Err(DelegateError::ServeDown) => {
            // Serve not running — fall through to local add (fast: refused is instant).
            tracing::debug!("add_to_index: serve not running, adding locally");
        }
        Err(DelegateError::ServeUnresponsive) => {
            return Err(anyhow::anyhow!(
                "codesearch serve is running but did not become ready within the wait \
                 budget (~2 min). Stop serve first to add locally, or wait for it to \
                 finish warming up."
            ));
        }
        Err(DelegateError::Failed(reason)) => {
            eprintln!(
                "{}",
                format!("⚠️  serve is running but could not delegate: {}", reason).yellow()
            );
            eprintln!("   Adding locally instead.");
        }
    }

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

        // If this is a local DB in the current dir, ensure it is registered in
        // repos.json (for legacy DBs that predate auto-registration). The alias
        // is always derived from the directory name.
        if db.is_current && !db.is_global {
            let mut config = crate::db_discovery::repos::ReposConfig::load().unwrap_or_default();
            if let Some(existing) = config.alias_for_path(&canonical_path) {
                println!("   Already registered as '{}'.", existing);
            } else {
                let assigned = config.register(canonical_path.clone());
                if let Err(e) = config.save() {
                    eprintln!("⚠️ Failed to save repos config: {}", e);
                } else {
                    println!("   ✅ Registered as '{}'.", assigned);
                }
            }
            return Ok(());
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

    let has_global = is_registered_repository(&canonical_path)?;

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
        index(
            Some(canonical_path.clone()),
            false,
            false,
            true,
            model,
            cancel_token.clone(),
        )
        .await?;
        println!("\n{}", "✅ Global index created!".green());
        eprintln!("⚠️ Global indexes are not auto-registered. Use 'index add' without --global for serve discovery.");
    } else {
        println!("\n{}", "Creating local index...".cyan());
        index(
            Some(canonical_path.clone()),
            false,
            false,
            false,
            model,
            cancel_token,
        )
        .await?;
        println!("\n{}", "✅ Local index created!".green());

        // Auto-register in repos.json for serve discovery
        let config_path = match crate::db_discovery::repos::ReposConfig::path() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("⚠️ Could not determine repos config path: {}", e);
                return Ok(());
            }
        };

        let mut config = crate::db_discovery::repos::ReposConfig::load().unwrap_or_default();

        if let Some(existing) = config.alias_for_path(&canonical_path) {
            eprintln!("ℹ️ Already registered as '{}'.", existing);
        } else {
            let assigned = config.register(canonical_path.clone());
            if let Err(e) = config.save() {
                eprintln!("⚠️ Index created, but failed to save repos config: {}", e);
                eprintln!("   Config path: {}", config_path.display());
            } else {
                eprintln!("✅ Registered as '{}'.", assigned);
            }
        }
    }

    Ok(())
}

/// Remove the index (local or global, auto-detected)
pub async fn remove_from_index(path: Option<PathBuf>, keep_config: bool) -> Result<()> {
    // If the argument names a registered alias, resolve it to that repo's path;
    // otherwise treat it as a filesystem path (existing behavior). This lets
    // `codesearch index rm <alias>` work by alias in addition to by path.
    let effective_path: Option<PathBuf> = match &path {
        Some(p) => {
            let raw = p.to_string_lossy();
            match crate::db_discovery::repos::ReposConfig::load() {
                Ok(cfg) => match cfg.resolve(&raw) {
                    Some(resolved) => {
                        println!(
                            "{}",
                            format!("🏷️  Resolved alias '{}' → {}", raw, resolved.display()).cyan()
                        );
                        Some(resolved)
                    }
                    None => path.clone(),
                },
                Err(_) => path.clone(),
            }
        }
        None => path.clone(),
    };

    let project_path = effective_path.clone().unwrap_or_else(|| PathBuf::from("."));
    let canonical_path = safe_canonicalize(&project_path)?;

    println!("{}", "➖ Remove Index".bright_red().bold());
    println!("{}", "=".repeat(60));
    println!("📂 Project: {}", canonical_path.display());

    // Try delegating to a running serve instance first (unless --keep-config,
    // which the serve endpoint doesn't support — serve always unregisters).
    if !keep_config {
        match try_delegate_rm_to_serve(&effective_path).await {
            Ok((alias, _)) => {
                println!("\n{}", "✅ Delegated to running serve instance.".green());
                println!("   Removed alias '{}'.", alias);
                println!("   FSW stopped, repo evicted from memory, DB deleted.");
                return Ok(());
            }
            Err(reason) => {
                // Serve not running or delegation failed — fall through to local operation.
                tracing::debug!("remove_from_index: delegation skipped ({})", reason);
            }
        }
    }

    // Check what exists
    let local_db = canonical_path.join(".codesearch.db");
    let has_local = local_db.exists();

    let has_global = is_registered_repository(&canonical_path)?;

    if !has_local && !has_global {
        println!("\n{}", "⚠️  No index found for this project.".yellow());
        return Ok(());
    }

    // Auto-unregister from repos.json unless --keep-config
    if !keep_config {
        let mut config = crate::db_discovery::repos::ReposConfig::load().unwrap_or_default();
        if config.unregister_path(&canonical_path) {
            if let Err(e) = config.save() {
                eprintln!("⚠️ Failed to update repos config: {}", e);
            } else {
                println!("{}", "🗑️  Unregistered from repos.json".green());
            }
        }
    } else {
        println!("{}", "ℹ️ Config entry preserved.".cyan());
    }

    // If both exist (shouldn't happen), remove local with warning
    if has_local && has_global {
        println!(
            "\n{}",
            "⚠️  Warning: Both local and global indexes exist!".yellow()
        );
        println!("   Removing local index...");
        if let Err(e) = fs::remove_dir_all(&local_db) {
            eprintln!(
                "⚠️ Database files may be locked by a running codesearch serve. Stop it and retry."
            );
            return Err(anyhow::anyhow!("Failed to remove local index: {}", e));
        }
        println!("   {}", "✅ Local index removed".green());
        println!("   (Global index remains)");
        return Ok(());
    }

    // Remove whichever exists
    if has_local {
        println!("\n{}", "Removing local index...".cyan());
        if let Err(e) = fs::remove_dir_all(&local_db) {
            eprintln!(
                "⚠️ Database files may be locked by a running codesearch serve. Stop it and retry."
            );
            return Err(anyhow::anyhow!("Failed to remove local index: {}", e));
        }
        println!("{}", "✅ Local index removed!".green());
    } else if has_global {
        println!("\n{}", "Removing global index...".cyan());
        unregister_repository(&canonical_path)?;
        println!("{}", "✅ Global index removed!".green());
    }

    Ok(())
}

/// Show index status — lists all repos registered in repos.json plus any
/// loose local DB in the current directory that is not yet registered.
pub async fn list_index_status() -> Result<()> {
    use crate::db_discovery::repos::ReposConfig;

    println!("{}", "📚 Indexed Repositories".bright_cyan().bold());
    println!("{}", "=".repeat(60));

    let config = ReposConfig::load().unwrap_or_default();

    if config.repos.is_empty() {
        println!("\n  No repositories registered.");
        println!("\n  Register one with:");
        println!("    codesearch index add          # register current directory");
        println!("    codesearch index add <path>   # register a specific path");
    } else {
        let mut entries: Vec<_> = config.repos.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));

        for (alias, project_path) in &entries {
            println!();
            println!("  {}", alias.bright_green());
            let db_path = project_path.join(".codesearch.db");
            print_repo_stats(project_path, &db_path)?;
        }

        println!();
        println!("  {} repositories registered.", entries.len());
    }

    // Show a loose local DB if the user is standing in one that is not registered
    let current_dir = std::env::current_dir()?;
    let current_db = current_dir.join(".codesearch.db");
    let current_alias = config.alias_for_path(&current_dir);

    if current_db.exists() && current_alias.is_none() {
        println!();
        println!("{}", "Local (unregistered):".bright_yellow());
        print_repo_stats(&current_dir, &current_db)?;
        println!("  Register with: codesearch index add");
    }

    Ok(())
}
#[allow(dead_code)]
async fn get_db_stats(db_path: &Path) -> Result<DbStats> {
    use crate::vectordb::VectorStore;

    if !db_path.exists() {
        return Ok(DbStats {
            chunk_count: 0,
            size_mb: 0.0,
            bloat_ratio: None,
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

    // Calculate bloat ratio from database file size and chunk count
    // Bloat ratio = (total_size / chunk_count) * 100 - indicates storage efficiency
    let bloat_ratio = if stats.total_chunks > 0 {
        Some((total_size as f64 / stats.total_chunks as f64) * 100.0)
    } else {
        None
    };

    Ok(DbStats {
        chunk_count: stats.total_chunks,
        size_mb: total_size as f64 / (1024.0 * 1024.0),
        bloat_ratio: Some(bloat_ratio.unwrap_or(0.0)),
    })
}

#[allow(dead_code)]
struct DbStats {
    chunk_count: usize,
    size_mb: f64,
    bloat_ratio: Option<f64>,
}

/// Run a serve-delegation closure, waiting patiently if serve is reachable but
/// still warming up (e.g. opening LMDB handles for 15+ repos at startup).
///
/// - `Ok(result)` immediately on success.
/// - `Err(ServeDown)` immediately when nothing is listening (fast path preserved).
/// - On `ServeUnresponsive`: print progress and retry up to [`SERVE_WARMUP_RETRIES`]
///   times with [`SERVE_WARMUP_RETRY_SLEEP`] between attempts. Returns
///   `Err(ServeUnresponsive)` only after exhausting the retry budget.
/// - `Err(Failed(_))` propagated immediately.
async fn serve_delegate_with_warmup_wait<F, Fut, T>(
    mut f: F,
) -> std::result::Result<T, DelegateError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, DelegateError>>,
{
    match f().await {
        Ok(r) => return Ok(r),
        Err(DelegateError::ServeDown) => return Err(DelegateError::ServeDown),
        Err(DelegateError::Failed(e)) => return Err(DelegateError::Failed(e)),
        Err(DelegateError::ServeUnresponsive) => {
            // First encounter — print a friendly message and start waiting.
            eprintln!(
                "{}",
                "⏳ serve is starting up, waiting for it to become ready...".yellow()
            );
        }
    }

    for attempt in 1..=SERVE_WARMUP_RETRIES {
        tokio::time::sleep(SERVE_WARMUP_RETRY_SLEEP).await;
        match f().await {
            Ok(r) => {
                eprintln!("{}", "✅ serve is ready, delegating...".green());
                return Ok(r);
            }
            Err(DelegateError::ServeDown) => return Err(DelegateError::ServeDown),
            Err(DelegateError::Failed(e)) => return Err(DelegateError::Failed(e)),
            Err(DelegateError::ServeUnresponsive) => {
                eprintln!(
                    "{}",
                    format!("⏳ still warming up ({attempt}/{SERVE_WARMUP_RETRIES})...").yellow()
                );
            }
        }
    }

    Err(DelegateError::ServeUnresponsive)
}

/// Outcome of probing the serve `/health` endpoint.
enum ServeProbe {
    /// Serve answered — safe to delegate.
    Up,
    /// Nothing is listening (connection refused / cannot connect). Serve is not
    /// running, so local indexing is appropriate. Detected immediately — the
    /// HTTP timeout never elapses for a refused connection, so the common
    /// "no serve → index locally" path stays fast.
    Down,
    /// A socket is listening but did not answer in time (serve is busy, e.g.
    /// warming up many repos at startup). Callers MUST NOT create a local
    /// duplicate index in this case — that is the bug this distinction prevents.
    Unresponsive,
}

/// Why delegation to a running serve did not complete.
pub(crate) enum DelegateError {
    /// Serve is not running. Local indexing is the right fallback.
    ServeDown,
    /// Serve is running but did not respond to `/health` in time (busy/warming).
    /// Callers must surface this loudly and must NOT create a local duplicate.
    ServeUnresponsive,
    /// Serve responded but a later delegation step failed.
    Failed(String),
}

impl From<String> for DelegateError {
    fn from(s: String) -> Self {
        DelegateError::Failed(s)
    }
}

impl std::fmt::Display for DelegateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DelegateError::ServeDown => write!(f, "serve is not running"),
            DelegateError::ServeUnresponsive => {
                write!(f, "serve is running but did not respond in time (busy)")
            }
            DelegateError::Failed(reason) => write!(f, "{}", reason),
        }
    }
}

/// How many times to retry the serve `/health` probe on *timeout* (serve is
/// listening but busy, e.g. warming up repos at startup). Connection-refused is
/// never retried, so the "no serve → index locally" path stays fast.
const SERVE_HEALTH_RETRIES: u32 = 3;

/// When serve is reachable but still warming up, how many times to retry the
/// full delegation before giving up. Each iteration waits SERVE_WARMUP_RETRY_SLEEP
/// then probes health + attempts delegation again.
/// Budget: ~6 × (14s probe + 8s sleep) ≈ 2 min — covers a 15-repo warmup.
const SERVE_WARMUP_RETRIES: u32 = 6;
/// Sleep between warmup retries. Long enough for serve to finish warming one repo.
const SERVE_WARMUP_RETRY_SLEEP: std::time::Duration = std::time::Duration::from_secs(8);
/// Sleep between serve `/health` timeout retries.
const SERVE_HEALTH_RETRY_SLEEP: std::time::Duration = std::time::Duration::from_millis(600);

/// Build a `reqwest::Client` configured for talking to a running `codesearch serve`
/// instance.
///
/// If `CODESEARCH_SERVE_API_KEY` is set in the environment (and non-empty), the
/// key is attached as `Authorization: Bearer <key>` to *every* request the client
/// makes. This is required:
/// - when serve is bound to a non-localhost address (the `require_auth_for_network`
///   middleware guards ALL endpoints, including `/health`), and
/// - for management endpoints (`POST /repos`, `DELETE /repos/:alias`,
///   `POST /repos/:alias/reindex`, `POST /reload`) when serve is bound to
///   localhost with the key set.
///
/// Without this, delegation to a network-bound serve returns 401 and falls back
/// to local indexing, risking LMDB file-lock conflicts. See issue #132.
fn build_serve_client(
    timeout: std::time::Duration,
) -> std::result::Result<reqwest::Client, String> {
    let key = std::env::var(crate::constants::SERVE_API_KEY_ENV)
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty());
    build_serve_client_with_key(timeout, key.as_deref())
}

/// Inner, testable form of [`build_serve_client`]: build a client that attaches
/// `Authorization: Bearer <key>` (when `key` is `Some`) as a default header, so
/// every request — health probe, POST, DELETE — carries it automatically.
pub(crate) fn build_serve_client_with_key(
    timeout: std::time::Duration,
    key: Option<&str>,
) -> std::result::Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().timeout(timeout);

    if let Some(k) = key {
        let mut headers = reqwest::header::HeaderMap::new();
        let auth_value =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", k)).map_err(|e| {
                format!(
                    "invalid {} value: {}",
                    crate::constants::SERVE_API_KEY_ENV,
                    e
                )
            })?;
        headers.insert(reqwest::header::AUTHORIZATION, auth_value);
        builder = builder.default_headers(headers);
    }

    builder
        .build()
        .map_err(|e| format!("failed to build HTTP client: {}", e))
}

/// When the server returns 401, produce a friendly, actionable error pointing at
/// the missing client-side API key instead of a bare "401 Unauthorized". Returns
/// `None` for any other status so callers can format their own error.
fn auth_failure_hint(status: reqwest::StatusCode) -> Option<String> {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        Some(format!(
            "serve returned 401 Unauthorized. The server requires an API key — \
             set the {} environment variable on the client to the same value as the server.",
            crate::constants::SERVE_API_KEY_ENV
        ))
    } else {
        None
    }
}

/// Probe serve `/health`, distinguishing "not running" from "busy".
///
/// A *connection refused* (or any non-timeout connect error) returns
/// [`ServeProbe::Down`] immediately, so the common "no serve → index locally"
/// path is not slowed down. Only a *timeout* — a socket is listening but slow,
/// which is what happens while serve warms up many repos — triggers a small,
/// bounded set of retries before returning [`ServeProbe::Unresponsive`].
async fn probe_serve_health(
    client: &reqwest::Client,
    base_url: &str,
    max_timeout_retries: u32,
    retry_sleep: std::time::Duration,
) -> ServeProbe {
    let url = format!("{}/health", base_url);
    let mut attempt: u32 = 0;
    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => return ServeProbe::Up,
            // A socket answered (even non-2xx) — serve is up and reachable.
            Ok(_) => return ServeProbe::Up,
            Err(e) if e.is_timeout() => {
                if attempt >= max_timeout_retries {
                    return ServeProbe::Unresponsive;
                }
                attempt += 1;
                tokio::time::sleep(retry_sleep).await;
            }
            // Connection refused / cannot connect → nothing is listening.
            Err(_) => return ServeProbe::Down,
        }
    }
}

/// Try to delegate a reindex to a running serve instance.
///
/// Returns `Ok((alias, project_path))` if the serve accepted the reindex request.
/// Returns `Err(reason)` with a human-readable reason if delegation failed.
async fn try_delegate_reindex_to_serve(
    path: &Option<PathBuf>,
    force: bool,
) -> std::result::Result<(String, PathBuf), DelegateError> {
    use crate::constants::{resolve_serve_host, DEFAULT_SERVE_PORT, SERVE_PORT_ENV};

    let port: u16 = std::env::var(SERVE_PORT_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SERVE_PORT);
    let host = resolve_serve_host();

    let base_url = format!("http://{}:{}", host, port);

    // 1. Health check — is serve running (and responsive)?
    // build_serve_client() attaches the CODESEARCH_SERVE_API_KEY header (if set)
    // so the health probe and all subsequent POSTs authenticate to a network-bound
    // serve. See issue #132.
    let client = build_serve_client(std::time::Duration::from_secs(3))?;

    match probe_serve_health(
        &client,
        &base_url,
        SERVE_HEALTH_RETRIES,
        SERVE_HEALTH_RETRY_SLEEP,
    )
    .await
    {
        ServeProbe::Up => {}
        ServeProbe::Down => return Err(DelegateError::ServeDown),
        ServeProbe::Unresponsive => return Err(DelegateError::ServeUnresponsive),
    }

    // 2. Resolve the project path to an alias by loading repos config
    let raw_project_path = path
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    // Canonicalize and strip UNC prefix (\\?\) for reliable path operations.
    let project_path =
        safe_canonicalize(&raw_project_path).unwrap_or_else(|_| raw_project_path.clone());

    let config = crate::db_discovery::repos::ReposConfig::load()
        .map_err(|e| format!("cannot load repos.json: {}", e))?;

    /// Normalize a path for alias comparison: canonicalize (resolves symlinks,
    /// relative components), then normalize via `cache::normalize_path` (strips
    /// Windows UNC prefix, converts backslashes) and lowercases for case-insensitive match.
    fn normalize_for_cmp(p: &std::path::Path) -> String {
        let canonical = safe_canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        crate::cache::normalize_path(&canonical).to_lowercase()
    }

    let project_norm = normalize_for_cmp(&project_path);

    // Find the alias for this path
    let alias = config
        .repos
        .iter()
        .find(|(_, p)| normalize_for_cmp(p) == project_norm)
        .map(|(a, _)| a.clone())
        .unwrap_or_else(|| {
            // Fallback: use the directory name as alias
            tracing::debug!(
                "try_delegate_reindex: path '{}' not found in repos.json, using dir name as alias",
                project_norm
            );
            project_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string()
        });

    // 3. POST /repos/{alias}/reindex[?force=true]
    let url = if force {
        format!("{}/repos/{}/reindex?force=true", base_url, alias)
    } else {
        format!("{}/repos/{}/reindex", base_url, alias)
    };
    let reindex_resp = client
        .post(&url)
        .send()
        .await
        .map_err(|e| format!("reindex POST failed: {}", e))?;

    if reindex_resp.status().is_success() {
        return Ok((alias, project_path));
    }

    let status = reindex_resp.status();
    let body = reindex_resp.text().await.unwrap_or_default();

    // 401 means the client is missing the API key the server requires. Surface a
    // friendly hint rather than falling through to the 404/500 recovery paths
    // (those would also 401 on the auto-register POST). See issue #132.
    if let Some(hint) = auth_failure_hint(status) {
        return Err(DelegateError::Failed(hint));
    }

    // If 404, the alias is unknown to serve — auto-register via POST /repos, then retry reindex.
    if status == reqwest::StatusCode::NOT_FOUND {
        tracing::info!(
            "alias '{}' not known to serve (404), auto-registering via POST /repos",
            alias
        );

        // Register the repo with serve
        let mut add_body = serde_json::json!({
            "path": project_path,
            "global": false,
        });
        // Use the resolved alias so the reindex retry targets the same name
        add_body["alias"] = serde_json::Value::String(alias.clone());

        let add_resp = client
            .post(format!("{}/repos", base_url))
            .json(&add_body)
            .send()
            .await
            .map_err(|e| format!("auto-register POST /repos failed: {}", e))?;

        if !add_resp.status().is_success() {
            let add_status = add_resp.status();
            let add_text = add_resp.text().await.unwrap_or_default();
            if let Some(hint) = auth_failure_hint(add_status) {
                return Err(DelegateError::Failed(hint));
            }
            return Err(DelegateError::Failed(format!(
                "auto-register returned {} for alias '{}': {}",
                add_status, alias, add_text
            )));
        }

        // Auto-register returns 202 Accepted — indexing runs in background.
        // No need to retry the reindex; the database will be created by the
        // spawned background task.
        tracing::info!(
            "auto-register accepted for alias '{}', indexing in background",
            alias
        );
        Ok((alias, project_path))
    } else if status == reqwest::StatusCode::INTERNAL_SERVER_ERROR
        && body.contains("Database not found")
    {
        // Serve knows the alias but its database was deleted externally (e.g. the
        // previous serve run was killed mid-index and the DB never fully formed).
        // Treat this the same as 404: auto-register so serve creates the DB fresh.
        tracing::info!(
            "alias '{}' has no database on serve (500 Database not found), \
             auto-registering via POST /repos to recreate",
            alias
        );

        let mut add_body = serde_json::json!({
            "path": project_path,
            "global": false,
        });
        add_body["alias"] = serde_json::Value::String(alias.clone());

        let add_resp = client
            .post(format!("{}/repos", base_url))
            .json(&add_body)
            .send()
            .await
            .map_err(|e| format!("auto-register POST /repos failed: {}", e))?;

        if !add_resp.status().is_success() {
            let add_status = add_resp.status();
            let add_text = add_resp.text().await.unwrap_or_default();

            // 409 means serve already has this alias registered (the alias is in
            // repos.json) but the DB directory is missing. POST /repos correctly
            // rejects the duplicate registration. The right recovery is a force
            // reindex, which uses allow_create=true and re-creates the DB.
            if add_status == reqwest::StatusCode::CONFLICT {
                tracing::info!(
                    "alias '{}' already registered on serve (409), retrying as force reindex \
                     to recreate missing database",
                    alias
                );
                let force_url = format!("{}/repos/{}/reindex?force=true", base_url, alias);
                let force_resp = client
                    .post(&force_url)
                    .send()
                    .await
                    .map_err(|e| format!("force reindex POST failed: {}", e))?;
                if force_resp.status().is_success() {
                    tracing::info!(
                        "force reindex accepted for '{}', DB will be recreated in background",
                        alias
                    );
                    return Ok((alias, project_path));
                }
                let force_status = force_resp.status();
                let force_text = force_resp.text().await.unwrap_or_default();
                if let Some(hint) = auth_failure_hint(force_status) {
                    return Err(DelegateError::Failed(hint));
                }
                return Err(DelegateError::Failed(format!(
                    "force reindex returned {} for alias '{}': {}",
                    force_status, alias, force_text
                )));
            }

            if let Some(hint) = auth_failure_hint(add_status) {
                return Err(DelegateError::Failed(hint));
            }
            return Err(DelegateError::Failed(format!(
                "auto-register returned {} for alias '{}': {}",
                add_status, alias, add_text
            )));
        }

        tracing::info!(
            "auto-register accepted for alias '{}' (DB recreate), indexing in background",
            alias
        );
        Ok((alias, project_path))
    } else {
        Err(DelegateError::Failed(format!(
            "serve returned {} for alias '{}': {}",
            status, alias, body
        )))
    }
}

/// Try to delegate `index add` to a running serve instance.
///
/// Returns `Ok((alias, project_path))` if the serve accepted the add request.
/// Returns `Err(reason)` with a human-readable reason if delegation failed
/// (e.g. serve not running, conflict).
pub(crate) async fn try_delegate_add_to_serve(
    path: &Option<PathBuf>,
    alias: &Option<String>,
    global: bool,
    model: &Option<ModelType>,
) -> std::result::Result<(String, PathBuf), DelegateError> {
    use crate::constants::{resolve_serve_host, DEFAULT_SERVE_PORT, SERVE_PORT_ENV};

    let port: u16 = std::env::var(SERVE_PORT_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SERVE_PORT);
    let host = resolve_serve_host();

    let base_url = format!("http://{}:{}", host, port);

    // 1. Health check — is serve running (and responsive)?
    // build_serve_client() attaches the CODESEARCH_SERVE_API_KEY header (if set)
    // so the health probe and the POST /repos authenticate to a network-bound
    // serve. See issue #132.
    let client = build_serve_client(std::time::Duration::from_secs(3))?;

    match probe_serve_health(
        &client,
        &base_url,
        SERVE_HEALTH_RETRIES,
        SERVE_HEALTH_RETRY_SLEEP,
    )
    .await
    {
        ServeProbe::Up => {}
        ServeProbe::Down => return Err(DelegateError::ServeDown),
        ServeProbe::Unresponsive => return Err(DelegateError::ServeUnresponsive),
    }

    // 2. Resolve path
    let raw_project_path = path
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let project_path =
        safe_canonicalize(&raw_project_path).unwrap_or_else(|_| raw_project_path.clone());

    // 3. Build request body
    let mut body = serde_json::json!({
        "path": project_path,
        "global": global,
    });
    if let Some(a) = alias {
        body["alias"] = serde_json::Value::String(a.clone());
    }
    if let Some(m) = model {
        body["model"] = serde_json::Value::String(m.short_name().to_string());
    }

    // 4. POST /repos
    let add_resp = client
        .post(format!("{}/repos", base_url))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("add POST failed: {}", e))?;

    if add_resp.status().is_success() {
        let resp_body: serde_json::Value = add_resp.json().await.unwrap_or_default();
        let assigned_alias = resp_body["alias"]
            .as_str()
            .unwrap_or_else(|| {
                project_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
            })
            .to_string();
        Ok((assigned_alias, project_path))
    } else {
        let status = add_resp.status();
        let text = add_resp.text().await.unwrap_or_default();
        // Surface a friendly hint on 401. See issue #132.
        if let Some(hint) = auth_failure_hint(status) {
            return Err(DelegateError::Failed(hint));
        }
        Err(DelegateError::Failed(format!(
            "serve returned {}: {}",
            status, text
        )))
    }
}

/// Try to delegate `index rm` to a running serve instance.
///
/// Returns `Ok((alias, project_path))` if the serve accepted the remove request.
/// Returns `Err(reason)` with a human-readable reason if delegation failed.
pub(crate) async fn try_delegate_rm_to_serve(
    path: &Option<PathBuf>,
) -> std::result::Result<(String, PathBuf), String> {
    use crate::constants::{resolve_serve_host, DEFAULT_SERVE_PORT, SERVE_PORT_ENV};

    let port: u16 = std::env::var(SERVE_PORT_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SERVE_PORT);
    let host = resolve_serve_host();

    let base_url = format!("http://{}:{}", host, port);

    // 1. Health check
    // build_serve_client() attaches the CODESEARCH_SERVE_API_KEY header (if set)
    // so the health probe and the DELETE authenticate to a network-bound serve.
    // See issue #132.
    let client = build_serve_client(std::time::Duration::from_secs(3))?;

    let health_resp = client
        .get(format!("{}/health", base_url))
        .send()
        .await
        .map_err(|e| {
            format!(
                "serve not reachable at {} ({}). Is 'codesearch serve' running?",
                base_url, e
            )
        })?;

    if !health_resp.status().is_success() {
        // Surface a friendly hint on 401 so users know to set the API key.
        if let Some(hint) = auth_failure_hint(health_resp.status()) {
            return Err(hint);
        }
        return Err(format!(
            "serve health check returned {}",
            health_resp.status()
        ));
    }

    // 2. Resolve the project path to an alias
    let raw_project_path = path
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let project_path =
        safe_canonicalize(&raw_project_path).unwrap_or_else(|_| raw_project_path.clone());

    fn normalize_for_cmp(p: &std::path::Path) -> String {
        let canonical = safe_canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        crate::cache::normalize_path(&canonical).to_lowercase()
    }

    let project_norm = normalize_for_cmp(&project_path);
    let config = crate::db_discovery::repos::ReposConfig::load()
        .map_err(|e| format!("cannot load repos.json: {}", e))?;

    let alias = config
        .repos
        .iter()
        .find(|(_, p)| normalize_for_cmp(p) == project_norm)
        .map(|(a, _)| a.clone())
        .ok_or_else(|| format!("path '{}' not found in repos.json", project_path.display()))?;

    // 3. DELETE /repos/:alias
    let delete_resp = client
        .delete(format!("{}/repos/{}", base_url, alias))
        .send()
        .await
        .map_err(|e| format!("delete failed: {}", e))?;

    if delete_resp.status().is_success() {
        Ok((alias, project_path))
    } else {
        let status = delete_resp.status();
        let text = delete_resp.text().await.unwrap_or_default();
        // Surface a friendly hint on 401. See issue #132.
        if let Some(hint) = auth_failure_hint(status) {
            return Err(hint);
        }
        Err(format!(
            "serve returned {} for alias '{}': {}",
            status, alias, text
        ))
    }
}

#[cfg(test)]
mod serve_probe_tests {
    use super::{auth_failure_hint, build_serve_client_with_key, probe_serve_health, ServeProbe};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Spawn a minimal HTTP server exposing `/health`. If `delay` is set, the
    /// handler sleeps that long before answering (to simulate a busy serve).
    async fn spawn_health_server(delay: Option<Duration>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(move || async move {
                if let Some(d) = delay {
                    tokio::time::sleep(d).await;
                }
                "ok"
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // Give the accept loop a moment to start.
        tokio::time::sleep(Duration::from_millis(50)).await;
        format!("http://{}", addr)
    }

    fn client(timeout: Duration) -> reqwest::Client {
        reqwest::Client::builder().timeout(timeout).build().unwrap()
    }

    /// A responsive `/health` → `Up` (delegation proceeds normally).
    #[tokio::test]
    async fn probe_reports_up_when_health_responds() {
        let base = spawn_health_server(None).await;
        let c = client(Duration::from_secs(2));
        assert!(
            matches!(
                probe_serve_health(&c, &base, 3, Duration::from_millis(10)).await,
                ServeProbe::Up
            ),
            "responsive /health must be Up"
        );
    }

    /// A socket that listens but never answers in time must be `Unresponsive`,
    /// NOT `Down` — this is the core of the fix: the caller treats `Unresponsive`
    /// as "serve is up but busy" and refuses to create a local duplicate index,
    /// instead of silently indexing locally. This is exactly the warmup scenario
    /// that produced a duplicate index.
    ///
    /// (The `Down` branch — a *non-timeout* connect error such as connection
    /// refused → `Down`, fast, no retries — is intentionally not unit-tested:
    /// reliably producing a "refused" socket is OS-dependent. On this Windows
    /// host a just-closed loopback port and the reserved port `:1` both *hang*
    /// instead of refusing, which would make such a test flaky.)
    #[tokio::test]
    async fn probe_reports_unresponsive_when_listening_but_slow() {
        let base = spawn_health_server(Some(Duration::from_secs(30))).await;
        // Tiny per-request timeout so each attempt times out quickly.
        let c = client(Duration::from_millis(150));
        assert!(
            matches!(
                probe_serve_health(&c, &base, 2, Duration::from_millis(10)).await,
                ServeProbe::Unresponsive
            ),
            "listening-but-slow serve must be Unresponsive, not Down"
        );
    }

    // ---- auth_failure_hint (issue #132) -------------------------------------

    /// 401 must produce a friendly hint that names the env var, so users know
    /// *what* to set instead of seeing a bare "401 Unauthorized".
    #[test]
    fn auth_failure_hint_for_401_names_env_var() {
        let hint =
            auth_failure_hint(reqwest::StatusCode::UNAUTHORIZED).expect("401 must yield a hint");
        assert!(
            hint.contains("401"),
            "hint should mention the status: {hint}"
        );
        assert!(
            hint.contains(crate::constants::SERVE_API_KEY_ENV),
            "hint should name the env var so users know what to set: {hint}"
        );
    }

    /// Any non-401 status returns `None` so callers format their own error.
    #[test]
    fn auth_failure_hint_none_for_other_statuses() {
        for status in [
            reqwest::StatusCode::FORBIDDEN,
            reqwest::StatusCode::NOT_FOUND,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            reqwest::StatusCode::OK,
        ] {
            assert!(
                auth_failure_hint(status).is_none(),
                "{status} must NOT trigger the auth-failure hint"
            );
        }
    }

    // ---- build_serve_client_with_key (issue #132) ---------------------------

    /// Spawn a server that records the `Authorization` header it receives on
    /// `/health` into the shared slot. Returns `(base_url, captured)`.
    async fn spawn_auth_echo_server() -> (String, Arc<Mutex<Option<String>>>) {
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(move |headers: axum::http::HeaderMap| async move {
                if let Some(v) = headers.get(reqwest::header::AUTHORIZATION) {
                    if let Ok(s) = v.to_str() {
                        *captured_clone.lock().unwrap() = Some(s.to_string());
                    }
                }
                "ok"
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        (format!("http://{}", addr), captured)
    }

    /// When a key is supplied, the client must send `Authorization: Bearer <key>`
    /// on every request — this is the core of the issue #132 fix.
    #[tokio::test]
    async fn build_serve_client_with_key_attaches_bearer_header() {
        let (base, captured) = spawn_auth_echo_server().await;
        let c = build_serve_client_with_key(Duration::from_secs(2), Some("hunter2"))
            .expect("client with valid key must build");

        // Send a GET — the client's default header must be applied automatically.
        c.get(format!("{base}/health"))
            .send()
            .await
            .expect("request must reach the server")
            .error_for_status()
            .expect("server must return 200");

        let seen = captured.lock().unwrap().clone();
        assert_eq!(
            seen.as_deref(),
            Some("Bearer hunter2"),
            "client built with a key must send Authorization: Bearer <key>"
        );
    }

    /// When no key is supplied (`None`), the client must NOT send any
    /// Authorization header — it should behave like a plain client.
    #[tokio::test]
    async fn build_serve_client_without_key_sends_no_auth_header() {
        let (base, captured) = spawn_auth_echo_server().await;
        let c = build_serve_client_with_key(Duration::from_secs(2), None)
            .expect("client without key must build");

        c.get(format!("{base}/health"))
            .send()
            .await
            .expect("request must reach the server")
            .error_for_status()
            .expect("server must return 200");

        let seen = captured.lock().unwrap().clone();
        assert!(
            seen.is_none(),
            "client built without a key must NOT send an Authorization header, got: {seen:?}"
        );
    }

    /// The inner builder treats any `Some(key)` literally — it does NOT trim or
    /// filter empties. That filtering is the responsibility of the outer
    /// `build_serve_client` (which reads the env var, `.trim()`s, and filters
    /// empties before calling this function). This test pins that contract so a
    /// future refactor doesn't silently move the filtering into the wrong layer.
    #[tokio::test]
    async fn inner_builder_attaches_header_for_any_some_value() {
        let (base, captured) = spawn_auth_echo_server().await;
        let c = build_serve_client_with_key(Duration::from_secs(2), Some("any-value"))
            .expect("Some value must build");
        c.get(format!("{base}/health"))
            .send()
            .await
            .expect("request must reach the server")
            .error_for_status()
            .expect("server must return 200");
        let seen = captured.lock().unwrap().clone();
        assert_eq!(
            seen.as_deref(),
            Some("Bearer any-value"),
            "inner builder attaches Bearer <key> verbatim; trimming is the caller's job"
        );
    }

    /// A key containing characters invalid in a header value (control chars,
    /// CR/LF) must fail fast with a message naming the env var, rather than
    /// panicking or silently dropping the header.
    #[test]
    fn build_serve_client_with_invalid_key_fails_with_env_var_name() {
        // CR/LF is rejected by HeaderValue::from_str (header injection guard).
        let err = build_serve_client_with_key(Duration::from_secs(2), Some("bad\r\nkey"))
            .expect_err("invalid header chars must produce an error");
        assert!(
            err.contains(crate::constants::SERVE_API_KEY_ENV),
            "error should name the env var so the user knows which var to fix: {err}"
        );
    }
}

#[cfg(test)]
mod index_quality_tests {
    use super::ensure_hnsw_index_if_needed;
    use crate::chunker::{Chunk, ChunkKind};
    use crate::embed::EmbeddedChunk;
    use crate::vectordb::VectorStore;
    use tempfile::tempdir;

    /// Helper: create a minimal EmbeddedChunk with `dims`-dimensional embedding.
    fn fake_chunk(path: &str, dims: usize) -> EmbeddedChunk {
        EmbeddedChunk::new(
            Chunk::new(
                "fn dummy() {}".to_string(),
                0,
                1,
                ChunkKind::Function,
                path.to_string(),
            ),
            vec![1.0_f32 / (dims as f32).sqrt(); dims],
        )
    }

    /// `ensure_hnsw_index_if_needed` returns `true` and leaves the DB indexed
    /// when there are chunks but the HNSW index was never built (simulates a
    /// prior cancellation that finished inserting chunks but never called
    /// `build_index`).
    #[test]
    fn rebuilds_unindexed_db_with_chunks() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        const DIMS: usize = 4;

        // Insert a chunk without calling build_index — simulate cancelled run.
        {
            let mut vs = VectorStore::new(&db_path, DIMS).unwrap();
            vs.insert_chunks(vec![fake_chunk("foo.rs", DIMS)]).unwrap();
            // Deliberately do NOT call vs.build_index()
            let s = vs.stats().unwrap();
            assert!(s.total_chunks > 0, "precondition: DB has chunks");
            assert!(!s.indexed, "precondition: index not yet built");
        }

        // Safety-net must detect and rebuild the index.
        let rebuilt = ensure_hnsw_index_if_needed(&db_path, DIMS)
            .expect("ensure_hnsw_index_if_needed should not error");
        assert!(
            rebuilt,
            "expected the function to report it rebuilt the index"
        );

        // Verify: DB is now indexed.
        let vs = VectorStore::new(&db_path, DIMS).unwrap();
        assert!(
            vs.is_indexed(),
            "VectorStore must be indexed after ensure_hnsw_index_if_needed"
        );
    }

    /// `ensure_hnsw_index_if_needed` returns `false` (no rebuild) when the DB
    /// is already indexed — repeated calls must be idempotent.
    #[test]
    fn no_rebuild_when_already_indexed() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        const DIMS: usize = 4;

        {
            let mut vs = VectorStore::new(&db_path, DIMS).unwrap();
            vs.insert_chunks(vec![fake_chunk("bar.rs", DIMS)]).unwrap();
            vs.build_index().unwrap();
            assert!(vs.is_indexed(), "precondition: already indexed");
        }

        let rebuilt = ensure_hnsw_index_if_needed(&db_path, DIMS)
            .expect("should succeed on already-indexed DB");
        assert!(
            !rebuilt,
            "should not report a rebuild on an already-indexed DB"
        );
    }

    /// `ensure_hnsw_index_if_needed` returns `false` (no rebuild) for an empty
    /// DB (no chunks, nothing to index).
    #[test]
    fn no_rebuild_for_empty_db() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        const DIMS: usize = 4;

        // Empty DB — no chunks inserted.
        {
            let _vs = VectorStore::new(&db_path, DIMS).unwrap();
        }

        let rebuilt =
            ensure_hnsw_index_if_needed(&db_path, DIMS).expect("should succeed on empty DB");
        assert!(!rebuilt, "empty DB needs no rebuild");
    }
}
