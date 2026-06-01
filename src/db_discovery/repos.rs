use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::constants::{CONFIG_DIR_NAME, REPOS_CONFIG_FILE};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReposConfig {
    pub repos: HashMap<String, PathBuf>,
    #[serde(default)]
    pub groups: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub repos_meta: HashMap<String, RepoMeta>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoMeta {
    /// Unix timestamp (seconds) of last observed repo change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_changed_unix: Option<i64>,
    /// Unix timestamp (seconds) of last successful SCIP index rebuild.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scip_indexed_unix: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct LegacyReposConfig(HashMap<String, serde_json::Value>);

impl ReposConfig {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        Self::load_from(&path).or_else(|e| {
            tracing::warn!("{}. Returning empty config.", e);
            Ok(Self::default())
        })
    }

    /// Load from an explicit path (useful in tests).
    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(path)?;

        // New format
        if let Ok(config) = serde_json::from_str::<Self>(&content) {
            return Ok(config);
        }

        // Legacy format: {"/abs/path": {...meta...}}
        if let Ok(legacy) = serde_json::from_str::<LegacyReposConfig>(&content) {
            let mut repos = HashMap::new();
            for (project_path, _meta) in legacy.0 {
                let path = PathBuf::from(&project_path);
                let alias = unique_alias_for_path(&repos, &path);
                repos.insert(alias, path);
            }

            return Ok(Self {
                repos,
                groups: HashMap::new(),
                repos_meta: HashMap::new(),
            });
        }

        // Both parses failed — file is corrupt
        Err(anyhow::anyhow!(
            "repos.json is corrupt or unrecognised at: {}",
            path.display()
        ))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        self.save_to(&path)
    }

    /// Save to an explicit path (useful in tests).
    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Return the path to the repos config file.
    pub fn path() -> Result<PathBuf> {
        config_path()
    }

    pub fn register(&mut self, path: PathBuf) -> String {
        let canonical = strip_unc(path.canonicalize().unwrap_or(path));

        if let Some((alias, _)) = self
            .repos
            .iter()
            .find(|(_, p)| normalize_path_for_compare(p) == normalize_path_for_compare(&canonical))
        {
            return alias.clone();
        }

        let alias = unique_alias_for_path(&self.repos, &canonical);
        self.repos.insert(alias.clone(), canonical);
        alias
    }

    pub fn register_with_alias(&mut self, path: PathBuf, alias: Option<String>) -> Result<String> {
        let canonical = strip_unc(path.canonicalize().unwrap_or(path));

        if let Some((existing_alias, _)) = self
            .repos
            .iter()
            .find(|(_, p)| normalize_path_for_compare(p) == normalize_path_for_compare(&canonical))
        {
            return Ok(existing_alias.clone());
        }

        let final_alias = match alias {
            Some(raw) => {
                let cleaned = sanitize_alias(&raw);
                if cleaned.is_empty() {
                    return Err(anyhow::anyhow!("Alias '{}' is invalid", raw));
                }
                if self.repos.contains_key(&cleaned) {
                    return Err(anyhow::anyhow!("Alias '{}' already exists", cleaned));
                }
                cleaned
            }
            None => unique_alias_for_path(&self.repos, &canonical),
        };

        self.repos.insert(final_alias.clone(), canonical);
        Ok(final_alias)
    }

    pub fn unregister_alias(&mut self, alias: &str) -> bool {
        if self.repos.remove(alias).is_none() {
            return false;
        }

        self.repos_meta.remove(alias);

        for aliases in self.groups.values_mut() {
            aliases.retain(|a| a != alias);
        }
        self.groups.retain(|_, aliases| !aliases.is_empty());
        true
    }

    /// Auto-discover repos when the config is empty.
    ///
    /// Scans the current working directory for a `.codesearch.db` database.
    /// If found and the repo list is empty, registers the CWD as a repo.
    /// Returns the number of newly discovered repos (0 or 1).
    pub fn auto_discover_from_cwd(&mut self) -> usize {
        if !self.repos.is_empty() {
            return 0;
        }

        let cwd = std::env::current_dir().unwrap_or_default();
        let db_path = cwd.join(crate::constants::DB_DIR_NAME);

        if crate::db_discovery::is_valid_database(&db_path) {
            let alias = self.register(cwd);
            tracing::info!("🔍 Auto-discovered repo '{}' from CWD", alias);
            return 1;
        }

        0
    }

    pub fn unregister_path(&mut self, path: &Path) -> bool {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let to_remove = self
            .repos
            .iter()
            .find(|(_, p)| normalize_path_for_compare(p) == normalize_path_for_compare(&canonical))
            .map(|(alias, _)| alias.clone());

        if let Some(alias) = to_remove {
            return self.unregister_alias(&alias);
        }

        false
    }

    pub fn resolve(&self, project: &str) -> Option<PathBuf> {
        self.repos.get(project).cloned()
    }

    /// Metadata for an alias. Returns default metadata when absent.
    pub fn meta(&self, alias: &str) -> RepoMeta {
        self.repos_meta.get(alias).cloned().unwrap_or_default()
    }

    /// Mutable metadata entry for an alias, creating it if needed.
    pub fn meta_mut(&mut self, alias: &str) -> &mut RepoMeta {
        self.repos_meta.entry(alias.to_string()).or_default()
    }

    /// Update `last_changed_unix` only when `ts` is newer.
    /// Returns true when metadata changed.
    pub fn touch_last_changed(&mut self, alias: &str, ts: i64) -> bool {
        let meta = self.meta_mut(alias);
        match meta.last_changed_unix {
            Some(existing) if ts <= existing => false,
            _ => {
                meta.last_changed_unix = Some(ts);
                true
            }
        }
    }

    /// Mark last successful SCIP rebuild timestamp.
    pub fn touch_last_scip(&mut self, alias: &str, ts: i64) {
        let meta = self.meta_mut(alias);
        meta.last_scip_indexed_unix = Some(ts);
    }

    #[allow(dead_code)] // Used in tests only — dead in bin targets
    pub fn resolve_group(&self, group: &str) -> Vec<(String, PathBuf)> {
        let Some(aliases) = self.groups.get(group) else {
            return Vec::new();
        };

        aliases
            .iter()
            .filter_map(|alias| self.repos.get(alias).map(|p| (alias.clone(), p.clone())))
            .collect()
    }

    pub fn add_group(&mut self, name: String, aliases: Vec<String>) -> Result<()> {
        if aliases.is_empty() {
            return Err(anyhow::anyhow!(
                "Group '{}' must contain at least one alias",
                name
            ));
        }

        for alias in &aliases {
            if !self.repos.contains_key(alias) {
                return Err(anyhow::anyhow!(
                    "Unknown alias '{}' for group '{}'",
                    alias,
                    name
                ));
            }
        }

        let mut deduped = Vec::new();
        for alias in aliases {
            if !deduped.contains(&alias) {
                deduped.push(alias);
            }
        }

        self.groups.insert(name, deduped);
        Ok(())
    }

    pub fn remove_group(&mut self, name: &str) -> bool {
        self.groups.remove(name).is_some()
    }

    pub fn alias_for_path(&self, path: &Path) -> Option<String> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.repos
            .iter()
            .find(|(_, p)| normalize_path_for_compare(p) == normalize_path_for_compare(&canonical))
            .map(|(alias, _)| alias.clone())
    }
}

pub fn config_dir() -> Result<PathBuf> {
    let home_dir = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory found"))?;
    Ok(home_dir.join(CONFIG_DIR_NAME))
}

pub fn config_path() -> Result<PathBuf> {
    if let Ok(override_path) = std::env::var(crate::constants::REPOS_CONFIG_ENV) {
        let path = PathBuf::from(&override_path);
        // Validate the env-var override points to a .json file to prevent
        // path traversal / arbitrary file read (CodeQL: uncontrolled data in path).
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext.eq_ignore_ascii_case("json") {
            return Ok(path);
        }
        anyhow::bail!(
            "{} must point to a .json file, got: {}",
            crate::constants::REPOS_CONFIG_ENV,
            override_path
        );
    }
    Ok(config_dir()?.join(REPOS_CONFIG_FILE))
}

fn unique_alias_for_path(existing: &HashMap<String, PathBuf>, path: &Path) -> String {
    let base_raw = path.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    let base = sanitize_alias(base_raw);
    let base = if base.is_empty() {
        "repo".to_string()
    } else {
        base
    };

    if !existing.contains_key(&base) {
        return base;
    }

    let mut idx = 2usize;
    loop {
        let candidate = format!("{}-{}", base, idx);
        if !existing.contains_key(&candidate) {
            return candidate;
        }
        idx += 1;
    }
}

/// Sanitize a raw alias string for use as a repo identifier.
///
/// Preserves the original casing and dots (e.g. "ExampleRepo" stays "ExampleRepo")
/// to match the directory/repo name. Only removes characters that are problematic
/// in identifiers: spaces become dashes, and characters outside `[a-zA-Z0-9._-]`
/// are dropped. Collapses consecutive dashes and trims leading/trailing dashes.
fn sanitize_alias(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else if ch == ' ' {
            out.push('-');
        }
        // All other characters (brackets, accents, etc.) are silently dropped
    }

    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_string()
}

fn normalize_path_for_compare(path: &Path) -> String {
    crate::cache::normalize_path(path)
}

/// Strip the Windows extended-length UNC prefix (`\\?\`) from a path so it is
/// never persisted to repos.json in that form. `Path::canonicalize()` on Windows
/// returns `\\?\C:\...`, which causes `Path::exists()` / `.join()` to behave
/// inconsistently for sub-paths (e.g. `\\?\C:\foo\.codesearch.db` may not exist
/// even when `C:\foo\.codesearch.db` does). Stripping the prefix before storage
/// ensures all downstream path operations use the plain `C:\...` form.
fn strip_unc(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_unique_alias_generation() {
        let mut repos = HashMap::new();
        repos.insert("codesearch".to_string(), PathBuf::from("/tmp/a"));
        let alias = unique_alias_for_path(&repos, Path::new("/tmp/codesearch"));
        assert_eq!(alias, "codesearch-2");
    }

    #[test]
    fn test_register_and_group_roundtrip() {
        let mut cfg = ReposConfig::default();
        let alias = cfg.register(PathBuf::from("/tmp/my-repo"));
        assert!(cfg.resolve(&alias).is_some());

        cfg.add_group("platform".to_string(), vec![alias.clone()])
            .unwrap();
        let resolved = cfg.resolve_group("platform");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, alias);
    }

    #[test]
    fn test_sanitize_alias() {
        assert_eq!(sanitize_alias("My Repo.Name"), "My-Repo.Name");
        // Preserves case and dots
        assert_eq!(sanitize_alias("ExampleRepo"), "ExampleRepo");
        assert_eq!(sanitize_alias("ExampleRepo"), "ExampleRepo");
        // Spaces become dashes
        assert_eq!(sanitize_alias("my repo"), "my-repo");
        // Special characters dropped
        assert_eq!(sanitize_alias("repo@v2!"), "repov2");
        // Collapses double dashes
        assert_eq!(sanitize_alias("a--b"), "a-b");
    }

    #[test]
    fn test_load_legacy_config_without_repos_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("repos.json");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"repos":{{"my-repo":"/tmp/my-repo"}},"groups":{{"g":["my-repo"]}}}}"#
        )
        .unwrap();

        let cfg = ReposConfig::load_from(&path).unwrap();
        assert_eq!(cfg.repos.len(), 1);
        assert_eq!(cfg.groups.len(), 1);
        assert!(cfg.repos_meta.is_empty());
    }

    #[test]
    fn test_save_then_load_roundtrip_with_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("repos.json");

        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("repo-a".to_string(), PathBuf::from("/tmp/repo-a"));
        cfg.touch_last_changed("repo-a", 100);
        cfg.touch_last_scip("repo-a", 120);
        cfg.save_to(&path).unwrap();

        let loaded = ReposConfig::load_from(&path).unwrap();
        let meta = loaded.meta("repo-a");
        assert_eq!(meta.last_changed_unix, Some(100));
        assert_eq!(meta.last_scip_indexed_unix, Some(120));
    }

    #[test]
    fn test_touch_last_changed_idempotent() {
        let mut cfg = ReposConfig::default();
        assert!(cfg.touch_last_changed("repo-a", 200));
        assert!(!cfg.touch_last_changed("repo-a", 200));
        assert!(!cfg.touch_last_changed("repo-a", 199));
        assert!(cfg.touch_last_changed("repo-a", 201));
    }

    #[test]
    fn test_meta_for_unknown_alias_returns_default() {
        let cfg = ReposConfig::default();
        let meta = cfg.meta("unknown");
        assert_eq!(meta, RepoMeta::default());
    }

    #[test]
    fn test_unregister_alias_removes_meta() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("repo-a".to_string(), PathBuf::from("/tmp/repo-a"));
        cfg.touch_last_changed("repo-a", 100);
        cfg.touch_last_scip("repo-a", 120);

        assert!(cfg.unregister_alias("repo-a"));
        assert!(!cfg.repos_meta.contains_key("repo-a"));
    }

    /// Regression: `Path::canonicalize()` on Windows returns a `\\?\`-prefixed UNC
    /// extended-length path. If stored verbatim in repos.json, downstream `.join()`
    /// and `.exists()` calls fail (e.g. `\\?\C:\foo\.codesearch.db` may not exist
    /// even when `C:\foo\.codesearch.db` does). `register` and `register_with_alias`
    /// must strip the prefix before storage so repos.json always holds plain paths.
    #[test]
    fn register_strips_unc_prefix_from_stored_path() {
        let mut cfg = ReposConfig::default();

        // Simulate what canonicalize() returns on Windows: a \\?\ UNC path.
        let unc_path = PathBuf::from(r"\\?\C:\WorkArea\AI\myrepo");
        // register() calls canonicalize() internally, but also accepts any path.
        // Test strip_unc directly (the private fn is in scope via pub(crate) isn't
        // exposed, so we exercise it via register_with_alias on a pre-formed path
        // by bypassing canonicalize with a path that starts with \\?\).
        let alias = cfg
            .register_with_alias(unc_path.clone(), Some("myrepo".to_string()))
            .unwrap();

        let stored = cfg.resolve(&alias).unwrap();
        let stored_str = stored.to_string_lossy();
        assert!(
            !stored_str.starts_with(r"\\?\"),
            "repos.json must not contain UNC prefix, got: {}",
            stored_str
        );
        assert!(
            stored_str.starts_with("C:\\") || stored_str.starts_with("C:/"),
            "stored path should be a plain Windows path, got: {}",
            stored_str
        );
    }
}
