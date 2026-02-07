//! Database discovery utilities for finding codesearch indexes
//!
//! Provides functions to find .codesearch.db directories in:
//! - Current directory
//! - Parent directories (upwards tree)
//! - Global list of indexed repositories
//!
//! # Database Validation
//!
//! A database is considered valid if it contains:
//! - `metadata.json` (required)
//! - `data.mdb` file (LMDB vector store) - directly in db folder
//! - `fts/` directory (full-text search)
//!
//! Invalid/incomplete databases are skipped during discovery.

use anyhow::Result;
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::constants::{CONFIG_DIR_NAME, DB_DIR_NAME, REPOS_CONFIG_FILE};

/// Information about a discovered database
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseInfo {
    /// Path to the project root (directory containing DB_DIR_NAME)
    pub project_path: PathBuf,
    /// Path to the database directory
    pub db_path: PathBuf,
    /// Whether this is the current working directory
    pub is_current: bool,
    /// Depth from current directory (0 = current, 1 = parent, etc.)
    pub depth: usize,
    /// Whether this is a global database (in GLOBAL_DB_DIR_NAME/)
    pub is_global: bool,
}

/// Check if a database directory is valid and complete
///
/// A valid database must contain:
/// - metadata.json (model info, dimensions)
/// - data.mdb file (LMDB vector embeddings) - directly in db folder
/// - fts/ directory (full-text search index)
///
/// Returns `true` if the database appears valid, `false` otherwise.
pub fn is_valid_database(db_path: &Path) -> bool {
    if !db_path.exists() || !db_path.is_dir() {
        return false;
    }

    let metadata_exists = db_path.join("metadata.json").exists();
    let lmdb_exists = db_path.join("data.mdb").exists(); // LMDB creates data.mdb directly in db folder
    let fts_exists = db_path.join("fts").is_dir();

    // All three components must exist
    metadata_exists && lmdb_exists && fts_exists
}

/// Check if a database directory exists but is incomplete/corrupt
///
/// Returns `Some(reason)` if the database is incomplete, `None` if valid or doesn't exist
pub fn check_database_integrity(db_path: &Path) -> Option<String> {
    if !db_path.exists() {
        return None; // Doesn't exist, not a corruption issue
    }

    if !db_path.is_dir() {
        return Some("exists but is not a directory".to_string());
    }

    let mut missing = Vec::new();

    if !db_path.join("metadata.json").exists() {
        missing.push("metadata.json");
    }
    if !db_path.join("data.mdb").exists() {
        missing.push("data.mdb");
    }
    if !db_path.join("fts").is_dir() {
        missing.push("fts/");
    }

    if missing.is_empty() {
        None // Valid
    } else {
        Some(format!("missing: {}", missing.join(", ")))
    }
}

/// Find databases in current directory and parent directories
///
/// Only returns databases that pass validation (have metadata.json, data.mdb, fts/).
/// Incomplete/corrupt databases are logged and skipped.
pub fn find_databases() -> Result<Vec<DatabaseInfo>> {
    let mut databases = Vec::new();

    // 1. Check current directory
    let current_dir = std::env::current_dir()?;
    let current_db = current_dir.join(DB_DIR_NAME);

    if current_db.exists() {
        if is_valid_database(&current_db) {
            databases.push(DatabaseInfo {
                project_path: current_dir.clone(),
                db_path: current_db,
                is_current: true,
                depth: 0,
                is_global: false,
            });
        } else if let Some(reason) = check_database_integrity(&current_db) {
            eprintln!(
                "{}",
                format!(
                    "⚠️  Skipping incomplete database at {}: {}",
                    current_db.display(),
                    reason
                )
                .yellow()
            );
        }
    }

    // 2. Check parent directories (up to 5 levels up)
    let mut parent_dir = current_dir.clone();
    for depth in 1..=5 {
        if let Some(parent) = parent_dir.parent() {
            parent_dir = parent.to_path_buf();
            let parent_db = parent_dir.join(DB_DIR_NAME);

            if parent_db.exists() {
                if is_valid_database(&parent_db) {
                    databases.push(DatabaseInfo {
                        project_path: parent_dir.clone(),
                        db_path: parent_db,
                        is_current: false,
                        depth,
                        is_global: false,
                    });
                } else if let Some(reason) = check_database_integrity(&parent_db) {
                    eprintln!(
                        "{}",
                        format!(
                            "⚠️  Skipping incomplete database at {}: {}",
                            parent_db.display(),
                            reason
                        )
                        .yellow()
                    );
                }
            }
        } else {
            break; // Reached filesystem root
        }
    }

    // 3. Check globally tracked repositories
    if let Ok(global_dbs) = find_global_databases() {
        databases.extend(global_dbs);
    }

    Ok(databases)
}

/// Find the best database to use for a given directory
///
/// Priority order:
/// 1. Valid database in current directory
/// 2. Valid database in nearest parent directory
/// 3. First valid global database
///
/// Incomplete/corrupt databases are skipped with a warning.
pub fn find_best_database(target_dir: Option<&Path>) -> Result<Option<DatabaseInfo>> {
    let target = target_dir.unwrap_or_else(|| Path::new("."));

    // Canonicalize the target path
    let canonical = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir()?.join(target)
    };

    // Try to canonicalize, but handle errors gracefully
    let canonical = match canonical.canonicalize() {
        Ok(path) => path,
        Err(_) => return Ok(None), // Path doesn't exist, return None
    };

    // 1. Check current directory
    let current_db = canonical.join(DB_DIR_NAME);
    if current_db.exists() {
        if is_valid_database(&current_db) {
            return Ok(Some(DatabaseInfo {
                project_path: canonical.clone(),
                db_path: current_db,
                is_current: true,
                depth: 0,
                is_global: false,
            }));
        } else if let Some(reason) = check_database_integrity(&current_db) {
            eprintln!(
                "{}",
                format!(
                    "⚠️  Found incomplete database at {}: {}",
                    current_db.display(),
                    reason
                )
                .yellow()
            );
            eprintln!(
                "{}",
                "   Run 'codesearch index --force' to rebuild it.".yellow()
            );
        }
    }

    // 2. Check parent directories
    let mut parent_dir = canonical.clone();
    for depth in 1..=5 {
        if let Some(parent) = parent_dir.parent() {
            parent_dir = parent.to_path_buf();
            let parent_db = parent_dir.join(DB_DIR_NAME);

            if parent_db.exists() {
                if is_valid_database(&parent_db) {
                    return Ok(Some(DatabaseInfo {
                        project_path: parent_dir.clone(),
                        db_path: parent_db,
                        is_current: false,
                        depth,
                        is_global: false,
                    }));
                } else if let Some(reason) = check_database_integrity(&parent_db) {
                    eprintln!(
                        "{}",
                        format!(
                            "⚠️  Found incomplete database at {}: {}",
                            parent_db.display(),
                            reason
                        )
                        .yellow()
                    );
                }
            }
        } else {
            break;
        }
    }

    // 3. Check global databases
    let global_dbs = find_global_databases()?;
    if !global_dbs.is_empty() {
        return Ok(Some(global_dbs.into_iter().next().unwrap()));
    }

    Ok(None)
}

/// Find globally tracked repositories
///
/// Only returns databases that pass validation.
fn find_global_databases() -> Result<Vec<DatabaseInfo>> {
    let home_dir = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory found"))?;
    let config_dir = home_dir.join(CONFIG_DIR_NAME);
    let config_path = config_dir.join(REPOS_CONFIG_FILE);

    if !config_path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&config_path)?;
    let repos_map: HashMap<String, serde_json::Value> = serde_json::from_str(&content)?;

    let mut databases = Vec::new();
    for (project_path, _meta) in repos_map {
        let path = PathBuf::from(&project_path);
        let db_path = path.join(DB_DIR_NAME);

        if is_valid_database(&db_path) {
            databases.push(DatabaseInfo {
                project_path: path,
                db_path,
                is_current: false,
                depth: usize::MAX, // Global, not in parent hierarchy
                is_global: true,
            });
        }
        // Note: We don't warn about incomplete global databases here
        // to avoid spam when there are many registered repos
    }

    Ok(databases)
}

/// Register a repository in the global tracking file
pub fn register_repository(project_path: &Path) -> Result<()> {
    let home_dir = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory found"))?;
    let config_dir = home_dir.join(CONFIG_DIR_NAME);
    let config_path = config_dir.join(REPOS_CONFIG_FILE);

    // Create config directory if it doesn't exist
    fs::create_dir_all(&config_dir)?;

    let mut repos_map: HashMap<String, serde_json::Value> = if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        HashMap::new()
    };

    // Add or update repository entry
    let canonical_path = project_path.canonicalize()?;
    let path_str = canonical_path.to_string_lossy().to_string();
    repos_map.insert(
        path_str.clone(),
        serde_json::json!({
            "indexed_at": chrono::Utc::now().to_rfc3339(),
        }),
    );

    // Write back
    fs::write(&config_path, serde_json::to_string_pretty(&repos_map)?)?;

    Ok(())
}

/// Unregister a repository from global tracking
pub fn unregister_repository(project_path: &Path) -> Result<()> {
    let home_dir = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory found"))?;
    let config_dir = home_dir.join(CONFIG_DIR_NAME);
    let config_path = config_dir.join(REPOS_CONFIG_FILE);

    if !config_path.exists() {
        return Ok(()); // Nothing to remove
    }

    let content = fs::read_to_string(&config_path)?;
    let mut repos_map: HashMap<String, serde_json::Value> = serde_json::from_str(&content)?;

    let canonical_path = project_path.canonicalize()?;
    let path_str = canonical_path.to_string_lossy().to_string();
    repos_map.remove(&path_str);

    fs::write(&config_path, serde_json::to_string_pretty(&repos_map)?)?;

    Ok(())
}

/// Resolve database path with user-friendly messaging
///
/// This is a shared utility used by both search and index commands.
/// It finds the best database and prints appropriate messages when using
/// a database from a parent directory or global location.
///
/// # Arguments
/// * `path` - Optional target path (defaults to current directory)
/// * `action` - Action verb for messaging (e.g., "searching", "indexing")
///
/// # Returns
/// * `Ok((db_path, project_path))` - Tuple of database path and project root path
/// * `Err(...)` - If path resolution fails
pub fn resolve_database_with_message(
    path: Option<&Path>,
    action: &str,
) -> Result<(PathBuf, PathBuf)> {
    let target = path.unwrap_or(Path::new("."));

    // Try to find best database using discovery
    if let Some(db_info) = find_best_database(Some(target))? {
        // If database is not in current directory, show a message
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        if !db_info.is_current {
            let relative_path = if let Ok(rel) = current_dir.strip_prefix(&db_info.project_path) {
                format!("./{}", rel.display())
            } else {
                db_info.project_path.display().to_string()
            };
            println!(
                "{}",
                format!(
                    "📂 Using database from: {}\n   ({} from subfolder, project root: {})",
                    db_info.db_path.display(),
                    action,
                    relative_path
                )
                .dimmed()
            );
        }
        return Ok((db_info.db_path, db_info.project_path));
    }

    // Fallback to current directory for backward compatibility
    let project_path = if let Some(p) = path {
        p.to_path_buf()
    } else {
        PathBuf::from(".")
    };

    // Try to canonicalize, but fall back to original path if it fails
    let canonical_path = project_path.canonicalize().unwrap_or(project_path.clone());
    let db_path = canonical_path.join(".codesearch.db");
    Ok((db_path, canonical_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_databases() {
        let databases = find_databases();
        assert!(databases.is_ok());
        let dbs = databases.unwrap();
        println!("Found {} databases", dbs.len());
    }
}
