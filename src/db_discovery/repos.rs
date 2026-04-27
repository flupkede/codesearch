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
        let canonical = path.canonicalize().unwrap_or(path);

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
        let canonical = path.canonicalize().unwrap_or(path);

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
            return Err(anyhow::anyhow!("Group '{}' must contain at least one alias", name));
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
        return Ok(PathBuf::from(override_path));
    }
    Ok(config_dir()?.join(REPOS_CONFIG_FILE))
}

fn unique_alias_for_path(existing: &HashMap<String, PathBuf>, path: &Path) -> String {
    let base_raw = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
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

fn sanitize_alias(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else if ch == ' ' || ch == '.' {
            out.push('-');
        }
    }

    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_string()
}

fn normalize_path_for_compare(path: &Path) -> String {
    crate::cache::normalize_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(sanitize_alias("My Repo.Name"), "my-repo-name");
    }
}
