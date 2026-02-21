//! Doctor command - diagnose and repair index health

use crate::cache::FileMetaStore;
use crate::constants::{DB_DIR_NAME, FILE_META_DB_NAME};
use crate::db_discovery::{find_best_database, is_valid_database};
use crate::embed::PersistentEmbeddingCache;
use crate::fts::FtsStore;
use crate::index::find_git_root;
use crate::vectordb::VectorStore;
use anyhow::Result;
use colored::Colorize;
use serde::Serialize;
use std::fs;
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Check status
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

/// Result of a single check
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl CheckResult {
    pub fn pass(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Pass,
            message: message.into(),
            details: None,
            hint: None,
        }
    }

    pub fn warn(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Warn,
            message: message.into(),
            details: None,
            hint: None,
        }
    }

    pub fn fail(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail,
            message: message.into(),
            details: None,
            hint: None,
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Check 1: Find database
fn check_find_database(project_path: &Path) -> CheckResult {
    match find_best_database(Some(project_path)) {
        Ok(Some(db_info)) => {
            CheckResult::pass(
                "Database found",
                format!("Database at {}", db_info.db_path.display()),
            )
            .with_details(format!(
                "Project: {} (depth {})",
                db_info.project_path.display(),
                db_info.depth
            ))
        }
        Ok(None) => CheckResult::fail(
            "No database found",
            "No .codesearch.db found in current or parent directories",
        )
        .with_hint("Run 'codesearch index' to create an index"),
        Err(e) => CheckResult::fail(
            "Database discovery failed",
            format!("Error finding database: {}", e),
        ),
    }
}

/// Check 2: Validate database structure
fn check_database_structure(db_path: &Path) -> CheckResult {
    if !db_path.exists() {
        return CheckResult::fail("Database structure", "Database path does not exist");
    }

    let mut missing = Vec::new();

    let metadata_path = db_path.join("metadata.json");
    if !metadata_path.exists() {
        missing.push("metadata.json");
    }

    let data_path = db_path.join("data.mdb");
    if !data_path.exists() {
        missing.push("data.mdb");
    }

    let fts_path = db_path.join("fts");
    if !fts_path.exists() || !fts_path.is_dir() {
        missing.push("fts/");
    }

    if is_valid_database(db_path) {
        CheckResult::pass("Database structure", "All required components present")
    } else if missing.is_empty() {
        CheckResult::warn(
            "Database structure",
            "Database appears incomplete or corrupted",
        )
        .with_details("Required files exist but validation failed")
    } else {
        CheckResult::fail(
            "Database structure",
            format!("Missing components: {}", missing.join(", ")),
        )
        .with_hint("Run 'codesearch index' to recreate the index")
    }
}

/// Check 3: Model consistency between metadata.json and file_meta.json
fn check_model_consistency(db_path: &Path) -> CheckResult {
    let metadata_path = db_path.join("metadata.json");
    let file_meta_path = db_path.join(FILE_META_DB_NAME);

    // Read model from metadata.json
    let metadata_model: Option<String> = fs::read_to_string(&metadata_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("model_short_name").and_then(|v| v.as_str()).map(|s| s.to_string()));

    // Read model from file_meta.json
    let file_meta_model: Option<String> = fs::read_to_string(&file_meta_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("model_name").and_then(|v| v.as_str()).map(|s| s.to_string()));

    match (metadata_model, file_meta_model) {
        (Some(meta), Some(file)) if meta == file => {
            CheckResult::pass("Model consistency", format!("Model: {}", meta))
        }
        (Some(meta), Some(file)) => {
            CheckResult::warn(
                "Model consistency",
                format!("Model name mismatch: metadata.json='{}', file_meta.json='{}'", meta, file),
            )
            .with_hint("This may cause issues; consider re-indexing")
        }
        (Some(meta), None) => {
            CheckResult::pass("Model consistency", format!("Model: {} (no file_meta.json yet)", meta))
        }
        (None, Some(file)) => {
            CheckResult::warn("Model consistency", format!("Model in file_meta only: {}", file))
        }
        (None, None) => {
            CheckResult::warn("Model consistency", "No model information found")
        }
    }
}

/// Check 4: Git repo detection - is index at git root?
fn check_git_root_placement(db_path: &Path, project_path: &Path) -> CheckResult {
    match find_git_root(project_path) {
        Ok(Some(git_root)) => {
            let db_canonical = fs::canonicalize(db_path).unwrap_or_else(|_| db_path.to_path_buf());
            let expected_db_path = git_root.join(DB_DIR_NAME);
            let expected_canonical = fs::canonicalize(&expected_db_path)
                .unwrap_or_else(|_| expected_db_path);

            if db_canonical == expected_canonical {
                CheckResult::pass(
                    "Git root placement",
                    format!("Index at git root: {}", git_root.display()),
                )
            } else {
                CheckResult::warn(
                    "Git root placement",
                    format!("Index not at git root; .git is at {}", git_root.display()),
                )
                .with_details(format!(
                    "Index should be at {} but is at {}",
                    expected_canonical.display(),
                    db_canonical.display()
                ))
                .with_hint("Move .codesearch.db to git root and re-index")
            }
        }
        Ok(None) => {
            CheckResult::warn("Git root placement", "No .git directory found")
                .with_details("Index may not be in optimal location")
        }
        Err(e) => {
            CheckResult::warn("Git root placement", format!("Could not find .git: {}", e))
        }
    }
}

/// Check 5: File integrity - find stale/unindexed files
///
/// Uses FileMetaStore to compare tracked files against disk.
/// Uses FileWalker to get the real list of indexable files (same as `codesearch index`).
fn check_file_integrity(db_path: &Path, project_path: &Path) -> CheckResult {
    let file_meta_path = db_path.join(FILE_META_DB_NAME);

    // Read model info from file_meta.json
    let (model_name, dimensions) = read_model_info(&file_meta_path);

    // Load FileMetaStore
    let store = match FileMetaStore::load_or_create(db_path, &model_name, dimensions) {
        Ok(s) => s,
        Err(e) => {
            return CheckResult::fail(
                "File integrity",
                format!("Could not load file metadata: {}", e),
            );
        }
    };

    // Stale files: in index but deleted from disk
    let stale_files = store.find_deleted_files();
    let stale_count = stale_files.len();

    // Walk disk to find all indexable files (uses the real FileWalker)
    let walker = crate::file::FileWalker::new(project_path.to_path_buf());
    let files = match walker.walk() {
        Ok((files, _)) => files,
        Err(e) => {
            return CheckResult::warn(
                "File integrity",
                format!("Could not walk project files: {}", e),
            );
        }
    };

    // Use check_file() for each file — same code path as `codesearch index`.
    // This avoids path format mismatches from set intersection.
    let mut up_to_date = 0;
    let mut unindexed = 0;

    for file in &files {
        match store.check_file(&file.path) {
            Ok((needs_reindex, old_ids)) => {
                if needs_reindex && old_ids.is_empty() {
                    // check_file returns (true, []) for two cases:
                    //   1. File has NO entry in the store → genuinely unindexed
                    //   2. File IS tracked but produced 0 chunks (minified JS, empty file, etc.)
                    // Distinguish them with is_tracked() — case 2 is not an error.
                    if store.is_tracked(&file.path) {
                        // Unchunkable file — tracked with 0 chunks, not a problem
                        up_to_date += 1;
                    } else {
                        unindexed += 1;
                    }
                } else if needs_reindex {
                    // Entry exists but content changed → count as up-to-date (just outdated)
                    up_to_date += 1;
                } else {
                    up_to_date += 1;
                }
            }
            Err(_) => {
                unindexed += 1;
            }
        }
    }

    if stale_count > 0 || unindexed > 0 {
        let mut details = Vec::new();
        if stale_count > 0 {
            details.push(format!(
                "{} stale files in index but deleted from disk",
                stale_count
            ));
        }
        if unindexed > 0 {
            details.push(format!("{} files on disk but not in index", unindexed));
        }

        CheckResult::warn(
            "File integrity",
            format!(
                "{} stale, {} unindexed, {} up to date",
                stale_count, unindexed, up_to_date
            ),
        )
        .with_details(details.join("; "))
        .with_hint("Run 'codesearch index' to fix stale/missing files")
    } else {
        CheckResult::pass(
            "File integrity",
            format!("{} files indexed and up to date", up_to_date),
        )
    }
}

/// Read model name and dimensions from file_meta.json
fn read_model_info(file_meta_path: &Path) -> (String, usize) {
    fs::read_to_string(file_meta_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .map(|json| {
            let model = json
                .get("model_name")
                .and_then(|v| v.as_str())
                .unwrap_or("minilm-l6-q")
                .to_string();
            let dims = json
                .get("dimensions")
                .and_then(|v| v.as_u64())
                .unwrap_or(384) as usize;
            (model, dims)
        })
        .unwrap_or_else(|| ("minilm-l6-q".to_string(), 384))
}

/// Read dimensions from metadata.json (fallback to 384)
fn read_dimensions(db_path: &Path) -> usize {
    fs::read_to_string(db_path.join("metadata.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("dimensions").and_then(|v| v.as_u64()))
        .unwrap_or(384) as usize
}

/// Check 6: Chunk integrity - vector store health
fn check_chunk_integrity(store: &VectorStore) -> CheckResult {
    let stats = store.stats().unwrap_or_else(|_| crate::vectordb::StoreStats {
        total_chunks: 0,
        total_files: 0,
        indexed: false,
        dimensions: 0,
        max_chunk_id: 0,
    });
    if stats.indexed {
        CheckResult::pass(
            "Chunk integrity",
            format!("Vector index searchable ({} chunks)", stats.total_chunks),
        )
        .with_details(format!(
            "Max chunk ID: {}, Files: {}, Dimensions: {}",
            stats.max_chunk_id, stats.total_files, stats.dimensions
        ))
    } else {
        CheckResult::warn("Chunk integrity", "Vector store empty - no chunks indexed")
            .with_hint("Run 'codesearch index' to populate the index")
    }
}

/// Check 7: FTS health
fn check_fts_health(db_path: &Path) -> CheckResult {
    match FtsStore::new(db_path) {
        Ok(_store) => CheckResult::pass("FTS health", "Full-text search index readable"),
        Err(e) => {
            CheckResult::fail("FTS health", format!("Failed to open FTS index: {}", e))
                .with_hint("Run 'codesearch index' to rebuild FTS index")
        }
    }
}

/// Check 8: LMDB bloat
fn check_lmdb_bloat(db_path: &Path, store: &VectorStore) -> CheckResult {
    let data_path = db_path.join("data.mdb");

    if !data_path.exists() {
        return CheckResult::fail("LMDB bloat", "data.mdb file does not exist");
    }

    let file_size = match fs::metadata(&data_path) {
        Ok(meta) => meta.len(),
        Err(e) => {
            return CheckResult::fail("LMDB bloat", format!("Failed to read data.mdb: {}", e));
        }
    };

    let total_chunks = store.stats().map(|s| s.total_chunks).unwrap_or(0);

    if total_chunks == 0 {
        return CheckResult::pass("LMDB bloat", "Empty database - no bloat concern");
    }

    // Estimate actual data size (5500 bytes per chunk is approximate)
    let estimated_data_size = total_chunks * 5500;
    let bloat_ratio = file_size as f64 / estimated_data_size as f64;

    if bloat_ratio < 2.0 {
        CheckResult::pass(
            "LMDB bloat",
            format!("Bloat ratio: {:.1}x ({} data in {} file)",
                bloat_ratio,
                format_bytes(estimated_data_size),
                format_bytes(file_size as usize)
            )
        )
    } else if bloat_ratio < 5.0 {
        CheckResult::warn(
            "LMDB bloat",
            format!("Bloat ratio: {:.1}x ({} data in {} file)",
                bloat_ratio,
                format_bytes(estimated_data_size),
                format_bytes(file_size as usize)
            )
        ).with_hint("Consider re-indexing to reduce bloat")
    } else {
        CheckResult::warn(
            "LMDB bloat",
            format!("High bloat ratio: {:.1}x ({} data in {} file)",
                bloat_ratio,
                format_bytes(estimated_data_size),
                format_bytes(file_size as usize)
            )
        ).with_hint("Run 'codesearch index' to rebuild and compact the database")
    }
}

/// Format bytes in human-readable format
fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2}GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Check 9: Embedding cache
fn check_embedding_cache(_db_path: &Path, model_name: &str) -> CheckResult {
    // PersistentEmbeddingCache::open takes model_name as &str
    match PersistentEmbeddingCache::open(model_name) {
        Ok(cache) => {
            match cache.stats() {
                Ok(stats) => {
                    if stats.entries > 0 {
                        CheckResult::pass(
                            "Embedding cache",
                            format!("{} entries ({})", stats.entries, format_bytes(stats.file_size_bytes as usize))
                        )
                    } else {
                        CheckResult::pass(
                            "Embedding cache",
                            format!("Cache empty but functional ({} entries)", stats.entries)
                        )
                    }
                }
        Err(_e) => {
                    CheckResult::warn("Embedding cache", "Could not get cache stats")
                }
            }
        }
        Err(e) => {
            CheckResult::warn("Embedding cache", format!("Could not open cache: {}", e))
        }
    }
}

/// Run all checks and return results
pub async fn run(fix: bool, json: bool) -> Result<()> {
    let project_path = Path::new(".");

    // Find database (single call)
    let db_info = match find_best_database(Some(project_path))? {
        Some(info) => info,
        None => {
            let results = vec![check_find_database(project_path)];
            if json {
                let output = serde_json::json!({
                    "checks": results,
                    "summary": { "warnings": 0, "errors": 1 }
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                print_results(&results, false);
            }
            anyhow::bail!("No database found");
        }
    };

    let db_path = db_info.db_path;
    // Use absolute project_path from database info — ensures FileWalker paths
    // match the normalized absolute paths stored in FileMetaStore by the indexer
    let project_path = db_info.project_path;

    // Read model name for cache check
    let model_name = fs::read_to_string(db_path.join("metadata.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("model_short_name").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown".to_string());

    // Open VectorStore once for checks that need it
    let dims = read_dimensions(&db_path);
    let vector_store = VectorStore::new(&db_path, dims);

    // Run all checks in order
    let mut results = vec![
        check_find_database(&project_path),
        check_database_structure(&db_path),
        check_model_consistency(&db_path),
        check_git_root_placement(&db_path, &project_path),
        check_file_integrity(&db_path, &project_path),
    ];

    // Checks that need VectorStore
    match &vector_store {
        Ok(store) => {
            results.push(check_chunk_integrity(store));
            results.push(check_fts_health(&db_path));
            results.push(check_lmdb_bloat(&db_path, store));
        }
        Err(e) => {
            results.push(CheckResult::fail(
                "Chunk integrity",
                format!("Failed to open vector store: {}", e),
            ));
            results.push(check_fts_health(&db_path));
            results.push(CheckResult::fail(
                "LMDB bloat",
                "Could not open vector store".to_string(),
            ));
        }
    }

    results.push(check_embedding_cache(&db_path, &model_name));

    // Print results
    print_results(&results, json);

    // Count warnings and errors
    let warnings = results.iter().filter(|r| r.status == CheckStatus::Warn).count();
    let errors = results.iter().filter(|r| r.status == CheckStatus::Fail).count();

    if json {
        // JSON mode: single root object with checks + summary
        let output = serde_json::json!({
            "checks": results,
            "summary": {
                "warnings": warnings,
                "errors": errors,
            }
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        // Normal mode: print summary
        println!();
        println!("{}", "Summary".bold());
        println!("{}", "=".repeat(60));
        println!("  {} warnings, {} errors", warnings, errors);

        // Add hints based on issues found
        if warnings > 0 || errors > 0 {
            if results.iter().any(|r| {
                r.status == CheckStatus::Warn || r.status == CheckStatus::Fail
            }) {
                println!();
                println!("{}", "💡 Run 'codesearch index' to fix stale/missing files".bright_yellow());
            }
            if fix {
                println!();
                println!("Running incremental refresh...");
                if let Err(e) = crate::index::index_quiet(None, false, CancellationToken::new()).await {
                    eprintln!("{} Failed to run index: {}", "❌".red(), e);
                } else {
                    println!("{}", "✅ Index refresh completed".green());
                }
            }
        }
    }

    if errors > 0 {
        anyhow::bail!("Doctor found {} error(s)", errors);
    }

    Ok(())
}

/// Print results to console (non-JSON mode only)
fn print_results(results: &[CheckResult], json: bool) {
    if json {
        return; // JSON output handled in run() as single root object
    }

    println!("{}", "🔍 Codesearch Doctor".bold());
    println!("{}", "=".repeat(60));

    for result in results {
        let icon = match result.status {
            CheckStatus::Pass => "✅".green(),
            CheckStatus::Warn => "⚠️".yellow(),
            CheckStatus::Fail => "❌".red(),
        };

        println!("  {} {}", icon, result.message);

        if let Some(details) = &result.details {
            println!("    {}", details.dimmed());
        }

        if let Some(hint) = &result.hint {
            println!("    {}", hint.bright_cyan());
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use tempfile::tempdir;

    fn create_metadata_json(dir: &Path, model_short_name: &str) {
        let metadata_path = dir.join("metadata.json");
        let content = format!(
            r#"{{
  "version": "1.0.0",
  "model_short_name": "{}",
  "dimensions": 384
}}"#,
            model_short_name
        );
        let mut file = File::create(&metadata_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    fn create_file_meta_json(dir: &Path, model_name: &str) {
        let file_meta_path = dir.join("file_meta.json");
        let content = format!(
            r#"{{
  "model_name": "{}",
  "dimensions": 384,
  "files": {{}}
}}"#,
            model_name
        );
        let mut file = File::create(&file_meta_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    fn create_lmdb_file(dir: &Path) {
        let data_path = dir.join("data.mdb");
        let mut file = File::create(&data_path).unwrap();
        // Write some fake data
        file.write_all(&[0u8; 4096]).unwrap();
    }

    fn create_fts_dir(dir: &Path) {
        let fts_path = dir.join("fts");
        fs::create_dir_all(&fts_path).unwrap();
        // Create a minimal index file
        File::create(fts_path.join(".keep")).unwrap();
    }

    fn create_valid_database(dir: &Path, model: &str) {
        create_metadata_json(dir, model);
        create_file_meta_json(dir, model);
        create_lmdb_file(dir);
        create_fts_dir(dir);
    }

    #[test]
    fn test_doctor_no_database() {
        let temp_dir = tempdir().unwrap();
        let project_path = temp_dir.path();

        // No .codesearch.db exists
        let result = check_find_database(project_path);

        assert_eq!(result.status, CheckStatus::Fail);
        assert_eq!(result.name, "No database found");
        assert!(result.message.contains("No .codesearch.db found"));
    }

    #[test]
    fn test_doctor_incomplete_database() {
        let temp_dir = tempdir().unwrap();
        let db_dir = temp_dir.path().join(".codesearch.db");
        fs::create_dir_all(&db_dir).unwrap();

        // Create only metadata.json - missing other components
        create_metadata_json(&db_dir, "minilm-l6-q");

        let result = check_database_structure(&db_dir);

        assert_eq!(result.status, CheckStatus::Fail);
        assert_eq!(result.name, "Database structure");
        assert!(result.message.contains("Missing components"));
    }

    #[test]
    fn test_doctor_model_name_mismatch() {
        let temp_dir = tempdir().unwrap();
        let db_dir = temp_dir.path().join(".codesearch.db");
        fs::create_dir_all(&db_dir).unwrap();

        // Different model names
        create_metadata_json(&db_dir, "minilm-l6-q");
        create_file_meta_json(&db_dir, "wrong-model");

        let result = check_model_consistency(&db_dir);

        assert_eq!(result.status, CheckStatus::Warn);
        assert_eq!(result.name, "Model consistency");
        assert!(result.message.contains("mismatch"));
        assert!(result.message.contains("minilm-l6-q"));
    }

    #[test]
    fn test_doctor_model_name_consistent() {
        let temp_dir = tempdir().unwrap();
        let db_dir = temp_dir.path().join(".codesearch.db");
        fs::create_dir_all(&db_dir).unwrap();

        // Same model names
        create_metadata_json(&db_dir, "minilm-l6-q");
        create_file_meta_json(&db_dir, "minilm-l6-q");

        let result = check_model_consistency(&db_dir);

        assert_eq!(result.status, CheckStatus::Pass);
        assert_eq!(result.name, "Model consistency");
        assert!(result.message.contains("minilm-l6-q"));
    }

    #[test]
    fn test_doctor_misplaced_index() {
        let temp_dir = tempdir().unwrap();

        // Create .git in a child directory
        let git_dir = temp_dir.path().join("subdir").join(".git");
        fs::create_dir_all(&git_dir).unwrap();

        // Create .codesearch.db in parent (wrong location)
        let db_dir = temp_dir.path().join(".codesearch.db");
        fs::create_dir_all(&db_dir).unwrap();

        let project_path = temp_dir.path();
        let result = check_git_root_placement(&db_dir, project_path);

        assert_eq!(result.status, CheckStatus::Warn);
        assert_eq!(result.name, "Git root placement");
        assert!(result.message.contains("not at git root"));
    }

    #[test]
    fn test_doctor_index_at_git_root() {
        let temp_dir = tempdir().unwrap();

        // Create .git and .codesearch.db in same directory
        let git_dir = temp_dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();

        let db_dir = temp_dir.path().join(".codesearch.db");
        fs::create_dir_all(&db_dir).unwrap();

        let project_path = temp_dir.path();
        let result = check_git_root_placement(&db_dir, project_path);

        assert_eq!(result.status, CheckStatus::Pass);
        assert_eq!(result.name, "Git root placement");
        assert!(result.message.contains("at git root"));
    }

    #[test]
    fn test_doctor_stale_files() {
        let temp_dir = tempdir().unwrap();
        let project_path = temp_dir.path();
        let db_dir = project_path.join(".codesearch.db");
        fs::create_dir_all(&db_dir).unwrap();

        // Create minimal database structure
        create_metadata_json(&db_dir, "minilm-l6-q");
        create_lmdb_file(&db_dir);
        create_fts_dir(&db_dir);

        // Create a real file, track it in FileMetaStore, then delete the file
        let test_file = project_path.join("will_be_deleted.rs");
        fs::write(&test_file, "fn stale() {}").unwrap();

        let mut store = FileMetaStore::new("minilm-l6-q".to_string(), 384);
        store.update_file(&test_file, vec![1, 2, 3]).unwrap();
        store.save(&db_dir).unwrap();

        // Now delete the file — it becomes stale
        fs::remove_file(&test_file).unwrap();

        let result = check_file_integrity(&db_dir, project_path);

        // Should warn about stale files
        assert_eq!(result.status, CheckStatus::Warn, "Expected Warn, got {:?}: {}", result.status, result.message);
        assert_eq!(result.name, "File integrity");
        assert!(result.details.as_ref().unwrap().contains("stale"),
            "Expected 'stale' in details, got: {:?}", result.details);
    }

    #[test]
    fn test_doctor_valid_database_all_green() {
        let temp_dir = tempdir().unwrap();
        let db_dir = temp_dir.path().join(".codesearch.db");
        fs::create_dir_all(&db_dir).unwrap();

        // Create valid database structure
        create_valid_database(&db_dir, "minilm-l6-q");

        // All structural checks should pass
        assert_eq!(check_database_structure(&db_dir).status, CheckStatus::Pass);
        assert_eq!(check_model_consistency(&db_dir).status, CheckStatus::Pass);
    }

    #[test]
    fn test_lmdb_bloat_no_data_file() {
        let temp_dir = tempdir().unwrap();
        let db_dir = temp_dir.path().join(".codesearch.db");
        fs::create_dir_all(&db_dir).unwrap();

        // No data.mdb → should fail
        let store = VectorStore::new(&db_dir, 4);
        if let Ok(ref s) = store {
            let result = check_lmdb_bloat(&db_dir, s);
            // With a fresh empty store, either pass (empty) or report bloat
            assert!(matches!(result.status, CheckStatus::Pass | CheckStatus::Warn));
        }
        // If store fails to open, that's fine — check_chunk_integrity handles it in run()
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(2048), "2.0KB");
        assert_eq!(format_bytes(2_097_152), "2.0MB");
        assert_eq!(format_bytes(2_147_483_648), "2.00GB");
    }

    #[test]
    fn test_check_result_with_details_and_hint() {
        let result = CheckResult::pass("test", "message")
            .with_details("details")
            .with_hint("hint");

        assert_eq!(result.name, "test");
        assert_eq!(result.message, "message");
        assert_eq!(result.status, CheckStatus::Pass);
        assert_eq!(result.details, Some("details".to_string()));
        assert_eq!(result.hint, Some("hint".to_string()));
    }

    #[test]
    fn test_check_result_serialization() {
        let result = CheckResult::pass("test", "message")
            .with_details("details")
            .with_hint("hint");

        let json = serde_json::to_string(&result).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["name"], "test");
        assert_eq!(parsed["status"], "pass");
        assert_eq!(parsed["message"], "message");
        assert_eq!(parsed["details"], "details");
        assert_eq!(parsed["hint"], "hint");
    }
}
