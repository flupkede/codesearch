use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cache::{safe_canonicalize, strip_unc_prefix};
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
    /// Git remote URL (`remote.origin.url`) captured at registration time.
    /// Used to re-locate a repo whose folder was renamed/moved (best-effort).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_remote: Option<String>,
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
        if let Ok(mut config) = serde_json::from_str::<Self>(&content) {
            config.reconcile();
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

            let mut config = Self {
                repos,
                groups: HashMap::new(),
                repos_meta: HashMap::new(),
            };
            config.reconcile();
            return Ok(config);
        }

        // Both parses failed — file is corrupt
        Err(anyhow::anyhow!(
            "repos.json is corrupt or unrecognised at: {}",
            path.display()
        ))
    }

    /// Harden an in-memory config loaded from disk so a hand-edited
    /// `repos.json` can never crash the app. This is best-effort cleanup,
    /// performed in memory only (no disk write here):
    ///
    /// 1. Drop repo entries whose alias key is empty/blank.
    /// 2. Drop `repos_meta` entries that reference an unknown alias.
    /// 3. Prune group members that reference unknown aliases; drop now-empty
    ///    groups.
    ///
    /// Existing (non-empty) alias keys are never renamed — that would break
    /// group references — so a merely "non-standard" hand-edited alias is
    /// tolerated as-is.
    pub(crate) fn reconcile(&mut self) {
        // 1. Drop empty/blank alias keys.
        let empty_keys: Vec<String> = self
            .repos
            .keys()
            .filter(|alias| alias.trim().is_empty())
            .cloned()
            .collect();
        for alias in empty_keys {
            tracing::warn!("repos.json: dropping entry with empty alias key");
            self.repos.remove(&alias);
        }

        // 2. Drop meta entries pointing at unknown aliases.
        let orphan_meta: Vec<String> = self
            .repos_meta
            .keys()
            .filter(|alias| !self.repos.contains_key(*alias))
            .cloned()
            .collect();
        for alias in orphan_meta {
            tracing::warn!("repos.json: dropping orphan metadata for '{}'", alias);
            self.repos_meta.remove(&alias);
        }

        // 3. Prune group members referencing unknown aliases; drop empty groups.
        let mut empty_groups: Vec<String> = Vec::new();
        for (group, members) in self.groups.iter_mut() {
            let before = members.len();
            members.retain(|alias| self.repos.contains_key(alias));
            if members.len() != before {
                tracing::warn!(
                    "repos.json: pruned {} unknown alias(es) from group '{}'",
                    before - members.len(),
                    group
                );
            }
            if members.is_empty() {
                empty_groups.push(group.clone());
            }
        }
        for group in empty_groups {
            tracing::warn!("repos.json: dropping now-empty group '{}'", group);
            self.groups.remove(&group);
        }
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
        // safe_canonicalize strips \\?\ on success; strip_unc_prefix handles the
        // fallback so UNC paths never enter the registry even if the path doesn't
        // exist yet (e.g. a path that will be created during indexing).
        let canonical = safe_canonicalize(&path).unwrap_or_else(|_| strip_unc_prefix(path));

        if let Some((alias, _)) = self
            .repos
            .iter()
            .find(|(_, p)| normalize_path_for_compare(p) == normalize_path_for_compare(&canonical))
        {
            return alias.clone();
        }

        let alias = unique_alias_for_path(&self.repos, &canonical);
        if let Some(remote) = git_remote_url(&canonical) {
            self.repos_meta.entry(alias.clone()).or_default().git_remote = Some(remote);
        }
        self.repos.insert(alias.clone(), canonical);
        alias
    }

    pub fn register_with_alias(&mut self, path: PathBuf, alias: Option<String>) -> Result<String> {
        let canonical = safe_canonicalize(&path).unwrap_or_else(|_| strip_unc_prefix(path));

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

        if let Some(remote) = git_remote_url(&canonical) {
            self.repos_meta
                .entry(final_alias.clone())
                .or_default()
                .git_remote = Some(remote);
        }
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
        let canonical =
            safe_canonicalize(path).unwrap_or_else(|_| strip_unc_prefix(path.to_path_buf()));
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
        let canonical =
            safe_canonicalize(path).unwrap_or_else(|_| strip_unc_prefix(path.to_path_buf()));
        self.repos
            .iter()
            .find(|(_, p)| normalize_path_for_compare(p) == normalize_path_for_compare(&canonical))
            .map(|(alias, _)| alias.clone())
    }

    /// Best-effort relocation of a registered repo whose stored path no longer
    /// exists (e.g. its folder was renamed/moved). Starting from the nearest
    /// still-existing ancestor of the stale path, scans (bounded depth) for a
    /// git repository whose `remote.origin.url` matches the one captured at
    /// registration time. Returns the new path only on a single unambiguous
    /// match; `None` when the path still exists, no remote was recorded, or the
    /// match is absent/ambiguous.
    pub fn try_relocate(&self, alias: &str) -> Option<PathBuf> {
        let stale = self.repos.get(alias)?;
        if stale.exists() {
            return None; // path is fine — nothing to relocate
        }

        let target_remote = self.repos_meta.get(alias)?.git_remote.clone()?;

        // Walk up to the nearest ancestor that still exists on disk.
        let mut anchor = stale.parent();
        while let Some(dir) = anchor {
            if dir.exists() {
                break;
            }
            anchor = dir.parent();
        }
        let anchor = anchor?;

        let mut matches = Vec::new();
        scan_for_remote(anchor, &target_remote, relocate_max_depth(), &mut matches);

        // Don't relocate onto a path already registered under another alias.
        matches.retain(|p| {
            !self.repos.iter().any(|(a, existing)| {
                a != alias && normalize_path_for_compare(existing) == normalize_path_for_compare(p)
            })
        });

        if matches.len() == 1 {
            Some(strip_unc_prefix(matches.into_iter().next().unwrap()))
        } else {
            None
        }
    }

    /// Relocate every registered repo whose stored path no longer exists.
    ///
    /// For each missing path a best-effort git-identity relocation is attempted
    /// ([`Self::try_relocate`]); successful matches rewrite the in-memory
    /// `repos` map.
    ///
    /// **Note:** this method performs disk I/O (filesystem traversal, git
    /// subprocess) and should not be called while holding an async lock or from
    /// an async task without `spawn_blocking`. No logging is emitted — callers
    /// are responsible for reporting results.
    ///
    /// Returns `(relocated, unresolved)` where `relocated` is the list of
    /// `(alias, new_path)` rewrites and `unresolved` is the list of aliases
    /// whose path is still missing.
    #[must_use]
    pub fn relocate_missing(&mut self) -> (Vec<(String, PathBuf)>, Vec<String>) {
        let aliases: Vec<String> = self.repos.keys().cloned().collect();
        let mut relocated = Vec::new();
        let mut unresolved = Vec::new();

        for alias in aliases {
            let Some(path) = self.repos.get(&alias) else {
                continue;
            };
            if path.exists() {
                continue;
            }
            match self.try_relocate(&alias) {
                Some(new_path) => {
                    self.repos.insert(alias.clone(), new_path.clone());
                    relocated.push((alias, new_path));
                }
                None => unresolved.push(alias),
            }
        }

        (relocated, unresolved)
    }

    /// Prune stale entries: relocate what can be relocated, then unregister the
    /// rest.
    ///
    /// **Note:** this method performs disk I/O (filesystem traversal, git
    /// subprocess) via [`Self::relocate_missing`]. No logging is emitted.
    ///
    /// Returns `(relocated, removed)`.
    #[must_use]
    pub fn prune_stale(&mut self) -> (Vec<(String, PathBuf)>, Vec<String>) {
        let (relocated, unresolved) = self.relocate_missing();
        let mut removed = Vec::new();
        for alias in unresolved {
            if self.unregister_alias(&alias) {
                removed.push(alias);
            }
        }
        (relocated, removed)
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

/// Best-effort lookup of a directory's git remote URL (`remote.origin.url`).
///
/// Returns `None` when `git` is unavailable, the path is not a git repo, or the
/// repo has no `origin` remote. Used both to capture a repo's identity at
/// registration time and to match candidate directories during relocation.
pub(crate) fn git_remote_url(path: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

/// Configured relocation scan depth (`CODESEARCH_RELOCATE_MAX_DEPTH`, default 3).
fn relocate_max_depth() -> usize {
    std::env::var(crate::constants::RELOCATE_MAX_DEPTH_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(crate::constants::DEFAULT_RELOCATE_MAX_DEPTH)
}

/// Directory names never worth descending into during a relocation scan.
fn is_skippable_scan_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name == crate::constants::DB_DIR_NAME
        || matches!(
            name,
            ".git" | "node_modules" | "target" | "bin" | "obj" | "dist" | "build"
        )
}

/// Recursively collect git roots under `dir` (bounded by `depth`) whose
/// `remote.origin.url` matches `target_remote`. A matching git root is recorded
/// and not descended into (nested repos below it are ignored).
fn scan_for_remote(dir: &Path, target_remote: &str, depth: usize, out: &mut Vec<PathBuf>) {
    if dir.join(".git").exists() {
        if git_remote_url(dir).as_deref() == Some(target_remote) {
            // Canonicalize to resolve 8.3 short names on Windows (e.g. RUNNER~1 →
            // runneradmin) so stored and found paths are always in the same form.
            out.push(safe_canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()));
        }
        return;
    }

    if depth == 0 {
        return;
    }

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() && !is_skippable_scan_dir(&child) {
                scan_for_remote(&child, target_remote, depth - 1, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Canonicalize then normalize a path for use in test assertions.
    ///
    /// On Windows, `tempfile::tempdir()` may return an 8.3 short-name path
    /// (e.g. `C:/Users/RUNNER~1/...`) while `std::fs::read_dir` can resolve the
    /// same directory to its long-name form (`C:/Users/runneradmin/...`).
    /// Applying `safe_canonicalize` before `normalize_path_for_compare` ensures
    /// both sides of an assertion use the same form.
    fn canon_norm(p: &Path) -> String {
        normalize_path_for_compare(&safe_canonicalize(p).unwrap_or_else(|_| p.to_path_buf()))
    }

    /// Initialise a git repo at `dir` with an `origin` remote pointing at `url`.
    fn init_git_remote(dir: &Path, url: &str) {
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git available in test env")
        };
        run(&["init"]);
        run(&["remote", "add", "origin", url]);
    }

    /// Rename a directory with automatic retries.
    ///
    /// On Windows, git subprocesses spawned by `init_git_remote` may keep a
    /// file handle on the directory open briefly after the process exits.
    /// `std::fs::rename` fails with `Access is denied` in that window.
    /// Retrying with a short exponential back-off is the simplest robust fix.
    #[track_caller]
    fn rename_retry(from: &Path, to: &Path) {
        let mut last_err = None;
        for attempt in 0..10u64 {
            match std::fs::rename(from, to) {
                Ok(()) => return,
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
                }
            }
        }
        panic!(
            "rename {:?} → {:?} failed after 10 attempts: {}",
            from,
            to,
            last_err.unwrap()
        );
    }

    #[test]
    fn captures_git_remote_on_register() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_git_remote(&repo, "https://example.com/acme/repo.git");

        let mut cfg = ReposConfig::default();
        let alias = cfg.register(repo);
        assert_eq!(
            cfg.meta(&alias).git_remote.as_deref(),
            Some("https://example.com/acme/repo.git")
        );
    }

    #[test]
    fn register_derives_alias_from_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("My.Cool-Repo");
        std::fs::create_dir(&repo).unwrap();

        let mut cfg = ReposConfig::default();
        let alias = cfg.register(repo.clone());
        // Alias is derived from (and sanitized from) the directory name.
        assert_eq!(alias, sanitize_alias("My.Cool-Repo"));
        assert!(cfg.repos.contains_key(&alias));
    }

    #[test]
    fn try_relocate_finds_renamed_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let repo = parent.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_remote(&repo, "https://example.com/acme/parent-repo.git");

        let mut cfg = ReposConfig::default();
        let alias = cfg.register(repo.clone());

        // Rename the PARENT folder; the stored repo path is now stale, but the
        // repo itself sits one level below the nearest existing ancestor (tmp).
        rename_retry(&parent, &tmp.path().join("parent-renamed"));

        let expected = tmp.path().join("parent-renamed").join("repo");
        let found = cfg
            .try_relocate(&alias)
            .expect("should relocate via renamed parent");
        assert_eq!(canon_norm(&found), canon_norm(&expected));
    }

    #[test]
    fn try_relocate_none_beyond_max_depth() {
        // Default max depth is 3. Bury the repo deeper than that below the
        // nearest existing ancestor so the scan cannot reach it.
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("oldbox").join("l1").join("l2").join("repo");
        std::fs::create_dir_all(&deep).unwrap();
        init_git_remote(&deep, "https://example.com/acme/deep.git");

        let mut cfg = ReposConfig::default();
        let alias = cfg.register(deep.clone());

        // Rename the top box; nearest existing ancestor becomes tmp root, and
        // the repo now sits 4 levels below it (box/l1/l2/repo) — out of reach.
        rename_retry(&tmp.path().join("oldbox"), &tmp.path().join("box"));

        assert!(
            cfg.try_relocate(&alias).is_none(),
            "repo beyond CODESEARCH_RELOCATE_MAX_DEPTH must not be relocated"
        );
    }

    #[test]
    fn relocate_missing_rewrites_only_moved_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let moved = tmp.path().join("moved");
        let stable = tmp.path().join("stable");
        std::fs::create_dir(&moved).unwrap();
        std::fs::create_dir(&stable).unwrap();
        init_git_remote(&moved, "https://example.com/acme/moved.git");
        init_git_remote(&stable, "https://example.com/acme/stable.git");

        let mut cfg = ReposConfig::default();
        let moved_alias = cfg.register(moved.clone());
        let stable_alias = cfg.register(stable.clone());

        let renamed = tmp.path().join("moved-renamed");
        rename_retry(&moved, &renamed);

        let (relocated, unresolved) = cfg.relocate_missing();
        assert!(unresolved.is_empty());
        assert_eq!(relocated.len(), 1);
        assert_eq!(relocated[0].0, moved_alias);
        assert_eq!(
            canon_norm(cfg.repos.get(&moved_alias).unwrap()),
            canon_norm(&renamed)
        );
        // The stable repo is untouched.
        assert_eq!(
            canon_norm(cfg.repos.get(&stable_alias).unwrap()),
            canon_norm(&stable)
        );
    }

    #[test]
    fn prune_stale_removes_unrelocatable_entries() {
        let tmp = tempfile::tempdir().unwrap();
        // No git remote → cannot be relocated → must be pruned.
        let plain = tmp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();

        let mut cfg = ReposConfig::default();
        let alias = cfg.register(plain.clone());
        cfg.add_group("g".to_string(), vec![alias.clone()]).unwrap();

        rename_retry(&plain, &tmp.path().join("plain-moved"));

        let (relocated, removed) = cfg.prune_stale();
        assert!(relocated.is_empty());
        assert_eq!(removed, vec![alias.clone()]);
        assert!(!cfg.repos.contains_key(&alias));
        // unregister_alias also cleans group membership.
        assert!(!cfg.groups.contains_key("g"));
    }

    #[test]
    fn prune_stale_relocates_then_keeps_relocatable_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_git_remote(&repo, "https://example.com/acme/keep.git");

        let mut cfg = ReposConfig::default();
        let alias = cfg.register(repo.clone());

        let renamed = tmp.path().join("repo-renamed");
        rename_retry(&repo, &renamed);

        let (relocated, removed) = cfg.prune_stale();
        assert!(removed.is_empty());
        assert_eq!(relocated.len(), 1);
        assert!(cfg.repos.contains_key(&alias), "relocated entry is kept");
    }

    #[test]
    fn load_from_applies_reconcile_to_hand_edited_file() {
        // A hand-edited repos.json with an empty-alias entry and a group that
        // references an unknown alias must be reconciled (not crash) on load.
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("repos.json");
        let json = r#"{
            "repos": { "": "/tmp/blank", "good": "/tmp/good" },
            "groups": { "mix": ["good", "ghost"], "dead": ["ghost"] },
            "repos_meta": { "ghost": {} }
        }"#;
        std::fs::write(&cfg_path, json).unwrap();

        let cfg = ReposConfig::load_from(&cfg_path).expect("load should succeed");
        assert!(!cfg.repos.contains_key(""), "empty alias dropped");
        assert!(cfg.repos.contains_key("good"));
        assert_eq!(cfg.groups.get("mix"), Some(&vec!["good".to_string()]));
        assert!(!cfg.groups.contains_key("dead"), "empty group dropped");
        assert!(!cfg.repos_meta.contains_key("ghost"), "orphan meta dropped");
    }

    #[test]
    fn try_relocate_finds_renamed_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("myrepo");
        std::fs::create_dir(&original).unwrap();
        init_git_remote(&original, "https://example.com/acme/myrepo.git");

        let mut cfg = ReposConfig::default();
        let alias = cfg.register(original.clone());

        // Rename the leaf folder; stored path is now stale.
        let renamed = tmp.path().join("myrepo-renamed");
        rename_retry(&original, &renamed);

        let found = cfg
            .try_relocate(&alias)
            .expect("should relocate renamed leaf");
        assert_eq!(canon_norm(&found), canon_norm(&renamed));
    }

    #[test]
    fn try_relocate_returns_none_when_path_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("live");
        std::fs::create_dir(&repo).unwrap();
        init_git_remote(&repo, "https://example.com/acme/live.git");

        let mut cfg = ReposConfig::default();
        let alias = cfg.register(repo);
        assert!(cfg.try_relocate(&alias).is_none());
    }

    #[test]
    fn try_relocate_none_without_recorded_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();

        let mut cfg = ReposConfig::default();
        let alias = cfg.register(plain.clone());
        assert!(cfg.meta(&alias).git_remote.is_none());

        rename_retry(&plain, &tmp.path().join("plain-moved"));
        assert!(cfg.try_relocate(&alias).is_none());
    }

    #[test]
    fn reconcile_drops_empty_alias_key() {
        let mut cfg = ReposConfig::default();
        cfg.repos.insert(String::new(), PathBuf::from("/tmp/x"));
        cfg.repos
            .insert("good".to_string(), PathBuf::from("/tmp/good"));
        cfg.reconcile();
        assert!(!cfg.repos.contains_key(""));
        assert!(cfg.repos.contains_key("good"));
    }

    #[test]
    fn reconcile_prunes_unknown_group_members_and_empty_groups() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("real".to_string(), PathBuf::from("/tmp/real"));
        cfg.groups.insert(
            "mix".to_string(),
            vec!["real".to_string(), "ghost".to_string()],
        );
        cfg.groups
            .insert("dead".to_string(), vec!["ghost".to_string()]);
        cfg.reconcile();
        assert_eq!(cfg.groups.get("mix"), Some(&vec!["real".to_string()]));
        assert!(
            !cfg.groups.contains_key("dead"),
            "group with only unknown members should be dropped"
        );
    }

    #[test]
    fn reconcile_drops_orphan_meta() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("real".to_string(), PathBuf::from("/tmp/real"));
        cfg.repos_meta
            .insert("ghost".to_string(), RepoMeta::default());
        cfg.reconcile();
        assert!(!cfg.repos_meta.contains_key("ghost"));
    }

    #[test]
    fn try_relocate_none_when_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("orig");
        std::fs::create_dir(&original).unwrap();
        init_git_remote(&original, "https://example.com/acme/dup.git");

        let mut cfg = ReposConfig::default();
        let alias = cfg.register(original.clone());

        // Two candidates with the same remote → ambiguous → no relocation.
        let a = tmp.path().join("copy-a");
        let b = tmp.path().join("copy-b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        init_git_remote(&a, "https://example.com/acme/dup.git");
        init_git_remote(&b, "https://example.com/acme/dup.git");
        // On Windows, git subprocesses spawned by init_git_remote may keep a
        // handle on the directory briefly, causing remove_dir_all to fail under
        // parallel test load. Ignore the error: if removal fails, `original`
        // still exists and try_relocate returns None because the path is present;
        // if removal succeeds, two ambiguous candidates are found → None.
        // Either way the assertion holds.
        let _ = std::fs::remove_dir_all(&original);

        assert!(cfg.try_relocate(&alias).is_none());
    }

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
