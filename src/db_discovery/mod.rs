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
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};

use crate::constants::DB_DIR_NAME;

pub mod repos;

use repos::ReposConfig;

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

/// Find the best database to use for a given directory
///
/// Priority order:
/// 1. Valid database in current directory
/// 2. Valid database in a direct child directory (1 level down — matches repo-anchored index)
/// 3. Valid database in nearest parent directory (up to 5 levels)
/// 4. First valid global database
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

    // 2. Check direct child directories (1 level down)
    //    Matches find_git_root Phase 2: index may be at git root inside a child dir
    //    e.g. /workspace/.codesearch.db doesn't exist, but /workspace/frontend/.codesearch.db does
    if let Ok(entries) = std::fs::read_dir(&canonical) {
        for entry in entries.flatten() {
            let child = entry.path();
            if !child.is_dir() {
                continue;
            }
            // Skip hidden dirs (except the target itself) and known non-project dirs
            let name = child.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            let child_db = child.join(DB_DIR_NAME);
            if child_db.exists() && is_valid_database(&child_db) {
                return Ok(Some(DatabaseInfo {
                    project_path: child,
                    db_path: child_db,
                    is_current: false,
                    depth: 1,
                    is_global: false,
                }));
            }
        }
    }

    // 3. Check parent directories
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

    // 4. Check global databases
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
    let config = ReposConfig::load()?;

    let mut databases = Vec::new();
    for path in config.repos.into_values() {
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
    let mut config = ReposConfig::load()?;
    config.register(project_path.to_path_buf());
    config.save()?;

    Ok(())
}

/// Unregister a repository from global tracking
pub fn unregister_repository(project_path: &Path) -> Result<()> {
    let mut config = ReposConfig::load()?;
    if config.unregister_path(project_path) {
        config.save()?;
    }

    Ok(())
}

/// Check whether a canonical project path is globally registered.
pub fn is_registered_repository(project_path: &Path) -> Result<bool> {
    let config = ReposConfig::load()?;
    Ok(config.alias_for_path(project_path).is_some())
}

/// List globally registered repositories as alias/path pairs.
#[allow(dead_code)] // Available for CLI and admin tooling
pub fn list_registered_repositories() -> Result<Vec<(String, PathBuf)>> {
    let config = ReposConfig::load()?;
    let mut repos: Vec<(String, PathBuf)> = config.repos.into_iter().collect();
    repos.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(repos)
}

/// Load full repositories configuration (repos + groups).
pub fn load_repos_config() -> Result<ReposConfig> {
    ReposConfig::load()
}

/// Save full repositories configuration (repos + groups).
#[allow(dead_code)] // Available for CLI and admin tooling
pub fn save_repos_config(config: &ReposConfig) -> Result<()> {
    config.save()?;
    
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
            eprintln!(
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
    use tempfile::tempdir;

    /// Create a fake valid database at the given path
    fn create_fake_db(db_path: &Path) {
        fs::create_dir_all(db_path).unwrap();
        fs::write(db_path.join("metadata.json"), "{}").unwrap();
        fs::write(db_path.join("data.mdb"), "fake").unwrap();
        fs::create_dir_all(db_path.join("fts")).unwrap();
    }

    #[test]
    fn test_is_valid_database() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join(DB_DIR_NAME);

        // Empty dir is not valid
        fs::create_dir_all(&db_path).unwrap();
        assert!(!is_valid_database(&db_path));

        // Add all required files
        create_fake_db(&db_path);
        assert!(is_valid_database(&db_path));
    }

    #[test]
    fn test_find_best_database_current_dir() {
        let dir = tempdir().unwrap();
        create_fake_db(&dir.path().join(DB_DIR_NAME));

        let result = find_best_database(Some(dir.path())).unwrap();
        assert!(result.is_some());
        let info = result.unwrap();
        assert!(info.is_current);
        assert_eq!(info.depth, 0);
    }

    #[test]
    fn test_find_best_database_child_dir() {
        let dir = tempdir().unwrap();
        // No DB at root, but DB in a child directory (repo-anchored index)
        let child = dir.path().join("frontend");
        fs::create_dir_all(&child).unwrap();
        create_fake_db(&child.join(DB_DIR_NAME));

        let result = find_best_database(Some(dir.path())).unwrap();
        assert!(result.is_some(), "Should find DB in child directory");
        let info = result.unwrap();
        assert!(!info.is_current);
        assert_eq!(info.depth, 1);
        assert!(info.project_path.ends_with("frontend"));
    }

    #[test]
    fn test_find_best_database_child_skips_hidden_dirs() {
        let dir = tempdir().unwrap();
        // DB inside a hidden child dir should be skipped
        let hidden = dir.path().join(".hidden_repo");
        fs::create_dir_all(&hidden).unwrap();
        create_fake_db(&hidden.join(DB_DIR_NAME));

        let result = find_best_database(Some(dir.path())).unwrap();
        assert!(
            result.is_none(),
            "Should not find DB in hidden child directory"
        );
    }

    #[test]
    fn test_find_best_database_child_skips_target_dir() {
        let dir = tempdir().unwrap();
        // DB inside target/ should be skipped
        let target = dir.path().join("target");
        fs::create_dir_all(&target).unwrap();
        create_fake_db(&target.join(DB_DIR_NAME));

        let result = find_best_database(Some(dir.path())).unwrap();
        assert!(result.is_none(), "Should not find DB in target/ directory");
    }

    #[test]
    fn test_find_best_database_prefers_current_over_child() {
        let dir = tempdir().unwrap();
        // DB at both root and child — root should win
        create_fake_db(&dir.path().join(DB_DIR_NAME));
        let child = dir.path().join("frontend");
        fs::create_dir_all(&child).unwrap();
        create_fake_db(&child.join(DB_DIR_NAME));

        let result = find_best_database(Some(dir.path())).unwrap();
        assert!(result.is_some());
        let info = result.unwrap();
        assert!(info.is_current, "Should prefer current dir over child");
    }

    #[test]
    fn test_find_best_database_none_when_empty() {
        let dir = tempdir().unwrap();
        let result = find_best_database(Some(dir.path())).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_find_best_database_invalid_child_db_skipped() {
        let dir = tempdir().unwrap();
        // Incomplete DB in child (missing data.mdb)
        let child = dir.path().join("myrepo");
        let db_path = child.join(DB_DIR_NAME);
        fs::create_dir_all(&db_path).unwrap();
        fs::write(db_path.join("metadata.json"), "{}").unwrap();
        // No data.mdb, no fts/ → invalid

        let result = find_best_database(Some(dir.path())).unwrap();
        assert!(result.is_none(), "Should not find incomplete DB");
    }
}
