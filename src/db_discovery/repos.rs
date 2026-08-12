use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cache::{normalize_user_path, safe_canonicalize, strip_unc_prefix};
use crate::constants::{CONFIG_DIR_NAME, REPOS_CONFIG_FILE};

/// A remote `codesearch serve` peer that can be queried for federation.
///
/// A group references a remote by listing `"@<peer_name>"` among its members
/// (the leading `@` marks it as a remote reference rather than a local alias).
/// Queries against such a group fan out to each remote peer over HTTP(S) and
/// the results are merged with the local results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemotePeer {
    /// Base URL of the remote serve instance, e.g. `https://codesearch.example.com`.
    #[serde(alias = "base_url")]
    pub url: String,
    /// Bearer / `X-API-Key` shared secret accepted by the remote (required when
    /// the remote is bound to a non-localhost address).
    #[serde(default)]
    pub api_key: String,
    /// Group to query on the remote (in the remote's own `repos.json`).
    /// When `None`, the remote's virtual `"all"` group is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Per-peer request timeout in seconds (default 15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// A resolved federation target — either a local repo or a remote peer.
///
/// Produced by [`ReposConfig::resolve_group_targets`]. Read-only tool handlers
/// split their resolved targets into local and remote sets: local targets are
/// served from the local LMDB stores as today; remote targets are queried over
/// HTTP and their results merged in.
#[derive(Debug, Clone)]
pub enum Target {
    /// A local repo, identified by alias and on-disk path.
    Local { alias: String, path: PathBuf },
    /// A remote peer, identified by the peer name under which it was declared
    /// in `remotes`, together with its full connection config. Represents the
    /// **whole peer** (its configured group) — produced by group federation.
    Remote { peer_name: String, peer: RemotePeer },
    /// A specific project on a remote peer, mounted locally as `<peer>/<alias>`.
    /// Produced by single-project resolution
    /// ([`ReposConfig::resolve_remote_project`]), never by group resolution.
    /// `remote_alias` is the project's bare, un-namespaced name **on the peer** —
    /// exactly what gets forwarded as `project=` to the peer's API.
    RemoteProject {
        peer_name: String,
        peer: RemotePeer,
        remote_alias: String,
    },
}

/// Prefix that marks a group member as a reference to a remote peer rather than
/// a local alias (e.g. `"@cloud"` → remote peer named `cloud`).
pub const REMOTE_REF_PREFIX: &str = "@";

/// Separator between a peer name and a remote project alias in a mounted remote
/// project's namespaced local name (e.g. `cloud/vendor-a`). Both sides are
/// guaranteed `/`-free: bare aliases are sanitized to `[A-Za-z0-9._-]` (see
/// [`sanitize_alias`]), and peer names are validated to reject `/` in
/// [`ReposConfig::add_remote`]. So the first `/` unambiguously splits
/// `<peer>/<alias>`.
pub const REMOTE_PROJECT_SEPARATOR: &str = "/";

/// Build the namespaced local name for a remote project: `"<peer>/<alias>"`.
pub fn remote_project_name(peer_name: &str, remote_alias: &str) -> String {
    format!("{peer_name}{REMOTE_PROJECT_SEPARATOR}{remote_alias}")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReposConfig {
    pub repos: HashMap<String, PathBuf>,
    #[serde(default)]
    pub groups: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub repos_meta: HashMap<String, RepoMeta>,
    /// Per-repo read-only flag. Aliases mapped to `true` are opened **read-only**
    /// by `codesearch serve`: the index is queried but never re-embedded/warmed
    /// (no write open, no incremental refresh). Intended for large static corpora
    /// on a memory-constrained replica where a separate job owns the heavy rebuild
    /// (e.g. the cloud DOCS corpus on the 2 GiB serve replica). Writes/reindexes
    /// against a read-only repo are rejected. Default: every repo is writable.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub repo_read_only: HashMap<String, bool>,
    /// Remote `codesearch serve` peers reachable for federation. Group members
    /// reference these via the `"@<peer_name>"` convention.
    #[serde(default)]
    pub remotes: HashMap<String, RemotePeer>,
    /// Remote projects the user has explicitly mounted locally, as canonical
    /// `"<peer>/<alias>"` names (opt-in allowlist). This list is the **single
    /// source of truth**: only mounted projects are routable
    /// (`project=<peer>/<alias>`), enumerable (`status` / `scope_required`),
    /// shown in the TUI, and included in `@peer` group fan-out. Adding a remote
    /// peer does NOT auto-mount anything — the user picks individual indexes via
    /// `codesearch remote mount`. (Replaces the former opt-out `remote_hidden`.)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_mounts: Vec<String>,
    /// Optional local rename of a mounted remote project: canonical
    /// `"<peer>/<alias>"` -> the custom local name shown/queried instead. The
    /// underlying bare alias sent to the peer is unaffected.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub remote_alias_overrides: HashMap<String, String>,
    /// Last-known remote project lists per peer, for offline fallback when a
    /// peer is unreachable at startup. `peer_name` -> `[bare remote alias, ...]`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub remote_project_cache: HashMap<String, Vec<String>>,
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
                ..Default::default()
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

        // 2b. Same for orphan read-only flags. `skip_serializing_if` only omits the
        //     map when it is entirely empty, so a stale `repo_read_only["gone"]`
        //     survives every round-trip — and an alias that is removed and later
        //     re-added under the same name would silently inherit read-only.
        let orphan_read_only: Vec<String> = self
            .repo_read_only
            .keys()
            .filter(|alias| !self.repos.contains_key(*alias))
            .cloned()
            .collect();
        for alias in orphan_read_only {
            tracing::warn!("repos.json: dropping orphan read-only flag for '{}'", alias);
            self.repo_read_only.remove(&alias);
        }

        // 3. Prune group members referencing unknown aliases OR unknown remote
        //    peers; drop now-empty groups. A member starting with `@` is a
        //    federation reference to a remote peer (`@cloud`), all others are
        //    local aliases. Unknown references on both sides are dropped so a
        //    hand-edited repos.json can never crash a later query.
        let mut empty_groups: Vec<String> = Vec::new();
        for (group, members) in self.groups.iter_mut() {
            let before = members.len();
            members.retain(|member| {
                if let Some(peer_name) = member.strip_prefix(REMOTE_REF_PREFIX) {
                    let known = self.remotes.contains_key(peer_name);
                    if !known {
                        tracing::warn!(
                            "repos.json: pruned unknown remote reference '{}' from group '{}'",
                            member,
                            group
                        );
                    }
                    known
                } else {
                    let known = self.repos.contains_key(member);
                    if !known {
                        tracing::warn!(
                            "repos.json: pruned unknown alias '{}' from group '{}'",
                            member,
                            group
                        );
                    }
                    known
                }
            });
            if members.len() != before {
                tracing::warn!(
                    "repos.json: pruned {} unknown member(s) from group '{}'",
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

        // 4. Prune mounted remote projects whose peer is unknown or whose name
        //    is malformed (no "<peer>/<alias>" split). A hand-edited or stale
        //    `remote_mounts` entry must never make an un-routable name look
        //    available.
        self.remote_mounts.retain(|canonical| {
            match canonical.split_once(REMOTE_PROJECT_SEPARATOR) {
                Some((peer_name, remote_alias))
                    if !peer_name.is_empty()
                        && !remote_alias.is_empty()
                        && self.remotes.contains_key(peer_name) =>
                {
                    true
                }
                _ => {
                    tracing::warn!(
                        "repos.json: pruned mounted remote project '{}' (unknown peer or malformed name)",
                        canonical
                    );
                    false
                }
            }
        });
        // Drop rename overrides that no longer point at a mounted project —
        // orphaned by the prune above OR by a hand-edited `remote_mounts`. An
        // override is only ever consulted for an allowlisted entry, so a stale
        // one is dead config; clearing it unconditionally also prevents a
        // surprise rename resurfacing if the project is later re-mounted.
        let mounted: std::collections::HashSet<&String> = self.remote_mounts.iter().collect();
        self.remote_alias_overrides
            .retain(|canonical, _| mounted.contains(canonical));

        // 5. Drop cached remote-project lists for peers that no longer exist —
        // a removed peer's last-known aliases are meaningless once the peer
        // itself is gone (and would otherwise resurrect a stale peer name if
        // it's later re-added under different projects).
        self.remote_project_cache
            .retain(|peer_name, _| self.remotes.contains_key(peer_name));
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
        // safe_canonicalize strips \\?\ on success AND translates MSYS POSIX
        // paths (`/c/...` → `C:/...`); the fallback re-applies both via
        // normalize_user_path so a not-yet-existing path still enters the
        // registry as `C:/...` instead of the raw `/c/...` (which Windows
        // would later resolve to `<drive>:\c\...`, the path-pollution defect).
        let canonical = safe_canonicalize(&path).unwrap_or_else(|_| normalize_user_path(&path));

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
        let canonical = safe_canonicalize(&path).unwrap_or_else(|_| normalize_user_path(&path));

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
        // normalize_user_path on the fallback keeps register/unregister
        // symmetry: if `register("/c/Users/foo")` stored `C:\Users\foo`, then
        // `unregister_path("/c/Users/foo")` must match it even when the dir
        // no longer exists (canonicalize fails) — see AGENTS.md "structural
        // fix" rule for the warnings-channel class defect this mirrors.
        let canonical = safe_canonicalize(path).unwrap_or_else(|_| normalize_user_path(path));
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
        // Virtual "all" group: resolves to every registered repo, never stored.
        if group == crate::constants::ALL_GROUP_NAME {
            return self
                .repos
                .iter()
                .map(|(a, p)| (a.clone(), p.clone()))
                .collect();
        }
        let Some(aliases) = self.groups.get(group) else {
            return Vec::new();
        };

        aliases
            .iter()
            .filter_map(|alias| self.repos.get(alias).map(|p| (alias.clone(), p.clone())))
            .collect()
    }

    /// Federation-aware group resolution.
    ///
    /// Like [`resolve_group`](Self::resolve_group) but also expands `"@<peer>"`
    /// members into [`Target::Remote`] entries. The virtual `"all"` group is
    /// **always local-only** — it never federates (it expands to every local
    /// repo, exactly as `resolve_group` does), so an `"all"` query can never
    /// accidentally leak to a remote peer.
    ///
    /// Unknown remote references (`@ghost` with no matching `remotes` entry)
    /// are skipped with a warning rather than failing — `reconcile` already
    /// prunes them at load time, this is a defensive double-check for configs
    /// built in-memory.
    pub fn resolve_group_targets(&self, group: &str) -> Vec<Target> {
        // Virtual "all" group: resolves to every registered LOCAL repo, never
        // stored and never federated.
        if group == crate::constants::ALL_GROUP_NAME {
            return self
                .repos
                .iter()
                .map(|(a, p)| Target::Local {
                    alias: a.clone(),
                    path: p.clone(),
                })
                .collect();
        }
        let Some(members) = self.groups.get(group) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for member in members {
            if let Some(peer_name) = member.strip_prefix(REMOTE_REF_PREFIX) {
                match self.remotes.get(peer_name) {
                    Some(peer) => out.push(Target::Remote {
                        peer_name: peer_name.to_string(),
                        peer: peer.clone(),
                    }),
                    None => tracing::warn!(
                        "group '{}' references unknown remote peer '{}'; skipped",
                        group,
                        peer_name
                    ),
                }
            } else if let Some(path) = self.repos.get(member) {
                out.push(Target::Local {
                    alias: member.clone(),
                    path: path.clone(),
                });
            }
        }
        out
    }

    /// Convenience: split a group's targets into local aliases (with paths) and
    /// remote peers. Useful for handlers that fan out local stores and remote
    /// peers separately.
    #[allow(clippy::type_complexity)]
    pub fn split_group_targets(
        &self,
        group: &str,
    ) -> (Vec<(String, PathBuf)>, Vec<(String, RemotePeer)>) {
        let mut locals = Vec::new();
        let mut remotes = Vec::new();
        for t in self.resolve_group_targets(group) {
            match t {
                Target::Local { alias, path } => locals.push((alias, path)),
                Target::Remote { peer_name, peer } => remotes.push((peer_name, peer)),
                // Group resolution never yields RemoteProject today, but keep the
                // match exhaustive: a mounted project maps to its peer.
                Target::RemoteProject {
                    peer_name, peer, ..
                } => remotes.push((peer_name, peer)),
            }
        }
        (locals, remotes)
    }

    /// Produce the user's explicitly mounted remote projects as
    /// `(local_name, Target::RemoteProject)` pairs, derived purely from the
    /// opt-in [`remote_mounts`](Self::remote_mounts) allowlist.
    ///
    /// - Each entry is a canonical `<peer>/<alias>` name; the pair carries the
    ///   bare `remote_alias` (un-namespaced) forwarded to the peer.
    /// - Skips entries whose peer is not in [`remotes`](Self::remotes) or that
    ///   are malformed (no `/`).
    /// - Applies [`remote_alias_overrides`](Self::remote_alias_overrides) so
    ///   `local_name` is the user's chosen rename.
    ///
    /// Live peer discovery is deliberately NOT consulted: mounts are defined by
    /// config, so they resolve even while a peer is unreachable. Result is
    /// de-duplicated and sorted by `local_name` for stable display/ordering.
    pub fn mounted_remote_projects(&self) -> Vec<(String, Target)> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for canonical in &self.remote_mounts {
            if !seen.insert(canonical.as_str()) {
                continue;
            }
            let Some((peer_name, remote_alias)) = canonical.split_once(REMOTE_PROJECT_SEPARATOR)
            else {
                continue;
            };
            if peer_name.is_empty() || remote_alias.is_empty() {
                continue;
            }
            let Some(peer) = self.remotes.get(peer_name) else {
                continue;
            };
            let local_name = self
                .remote_alias_overrides
                .get(canonical)
                .cloned()
                .unwrap_or_else(|| canonical.clone());
            out.push((
                local_name,
                Target::RemoteProject {
                    peer_name: peer_name.to_string(),
                    peer: peer.clone(),
                    remote_alias: remote_alias.to_string(),
                },
            ));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Resolve a project name to a [`Target::RemoteProject`], if it names a
    /// **mounted** remote project.
    ///
    /// Accepts either the canonical `"<peer>/<alias>"` form or a user rename
    /// declared in [`remote_alias_overrides`](Self::remote_alias_overrides).
    /// Returns `None` for local aliases, unknown peers, and — crucially — any
    /// name that is not in the opt-in [`remote_mounts`](Self::remote_mounts)
    /// allowlist. The allowlist is the single source of truth: an un-mounted
    /// `<peer>/<alias>` is unroutable even if the peer exposes it.
    ///
    /// **Precedence:** this method does not consult local repos, so a rename
    /// override whose custom value equals a local alias would resolve here to a
    /// remote target. Callers (MCP dispatch, Stage 2) MUST resolve local aliases
    /// first and only fall back to this, so local repos always win a name clash.
    pub fn resolve_remote_project(&self, name: &str) -> Option<Target> {
        // A rename override maps a custom local name back to its canonical
        // "<peer>/<alias>" key; fall back to treating `name` as canonical.
        let canonical: &str = self
            .remote_alias_overrides
            .iter()
            .find(|(_, custom)| custom.as_str() == name)
            .map(|(canonical, _)| canonical.as_str())
            .unwrap_or(name);

        // Opt-in allowlist gate: only explicitly mounted projects resolve.
        if !self.remote_mounts.iter().any(|m| m == canonical) {
            return None;
        }
        let (peer_name, remote_alias) = canonical.split_once(REMOTE_PROJECT_SEPARATOR)?;
        let peer = self.remotes.get(peer_name)?;
        Some(Target::RemoteProject {
            peer_name: peer_name.to_string(),
            peer: peer.clone(),
            remote_alias: remote_alias.to_string(),
        })
    }

    /// Expand a group's `@peer` references into the **mounted** remote projects
    /// belonging to those peers, as `(peer_name, peer, remote_alias)` tuples.
    ///
    /// This is the remote counterpart of [`resolve_group`](Self::resolve_group):
    /// a group that references `@cloud` fans out only to the individual
    /// `cloud/<alias>` indexes the user has mounted (opt-in
    /// [`remote_mounts`](Self::remote_mounts)) — NOT to the whole peer. A
    /// referenced peer with zero mounts contributes nothing. The virtual "all"
    /// group never federates, so it yields an empty list.
    pub fn group_remote_projects(&self, group: &str) -> Vec<(String, RemotePeer, String)> {
        if group == crate::constants::ALL_GROUP_NAME {
            return Vec::new();
        }
        let Some(members) = self.groups.get(group) else {
            return Vec::new();
        };
        // Peers referenced by this group via "@peer" (that actually exist).
        let referenced: std::collections::HashSet<&str> = members
            .iter()
            .filter_map(|m| m.strip_prefix(REMOTE_REF_PREFIX))
            .filter(|p| self.remotes.contains_key(*p))
            .collect();
        if referenced.is_empty() {
            return Vec::new();
        }
        self.mounted_remote_projects()
            .into_iter()
            .filter_map(|(_local, target)| match target {
                Target::RemoteProject {
                    peer_name,
                    peer,
                    remote_alias,
                } if referenced.contains(peer_name.as_str()) => {
                    Some((peer_name, peer, remote_alias))
                }
                _ => None,
            })
            .collect()
    }

    /// Opt-in mount a remote project by its canonical `<peer>/<alias>` name.
    /// Validates the name is well-formed and the peer exists. Idempotent; keeps
    /// [`remote_mounts`](Self::remote_mounts) sorted.
    pub fn mount_remote_project(&mut self, canonical: &str) -> Result<()> {
        let (peer_name, remote_alias) =
            canonical
                .split_once(REMOTE_PROJECT_SEPARATOR)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid remote project name '{}': expected '<peer>{}<alias>'",
                        canonical,
                        REMOTE_PROJECT_SEPARATOR
                    )
                })?;
        if peer_name.is_empty() || remote_alias.is_empty() {
            return Err(anyhow::anyhow!(
                "invalid remote project name '{}': peer and alias must be non-empty",
                canonical
            ));
        }
        if !self.remotes.contains_key(peer_name) {
            return Err(anyhow::anyhow!(
                "unknown remote peer '{}'; add it first with `codesearch remote add`",
                peer_name
            ));
        }
        if !self.remote_mounts.iter().any(|m| m == canonical) {
            self.remote_mounts.push(canonical.to_string());
            self.remote_mounts.sort();
        }
        Ok(())
    }

    /// Remove a mounted remote project (canonical `<peer>/<alias>`). Also drops
    /// any now-orphaned rename override. Returns `true` if it was mounted.
    pub fn unmount_remote_project(&mut self, canonical: &str) -> bool {
        let before = self.remote_mounts.len();
        self.remote_mounts.retain(|m| m != canonical);
        let removed = self.remote_mounts.len() != before;
        if removed {
            self.remote_alias_overrides.remove(canonical);
        }
        removed
    }

    /// Write-through cache: record the alias list a peer's `/status` just
    /// reported, so [`cached_remote_project_aliases`](Self::cached_remote_project_aliases)
    /// can serve a "last known" answer the next time that peer is unreachable.
    /// Sorted + deduped for stable, diff-friendly `repos.json` output.
    pub fn cache_remote_projects(&mut self, peer_name: &str, mut aliases: Vec<String>) {
        aliases.sort();
        aliases.dedup();
        self.remote_project_cache
            .insert(peer_name.to_string(), aliases);
    }

    /// Last-known alias list for `peer_name`, if any was ever cached via
    /// [`cache_remote_projects`](Self::cache_remote_projects). Used as an
    /// offline fallback when the peer's `/status` can't be reached live.
    pub fn cached_remote_project_aliases(&self, peer_name: &str) -> Option<&[String]> {
        self.remote_project_cache
            .get(peer_name)
            .map(|v| v.as_slice())
    }

    pub fn add_group(&mut self, name: String, aliases: Vec<String>) -> Result<()> {
        if name == crate::constants::ALL_GROUP_NAME {
            return Err(anyhow::anyhow!(
                "Group name '{}' is reserved — it always resolves to all registered repos automatically.",
                name
            ));
        }
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

    /// Return a groups map that includes the virtual `ALL_GROUP_NAME` group
    /// (mapping to every registered alias), for display/discoverability surfaces
    /// such as the `status` tool. The returned map is a clone — `self.groups` is
    /// untouched and "all" is never persisted to `repos.json`.
    pub fn groups_with_virtual_all(&self) -> std::collections::HashMap<String, Vec<String>> {
        let mut out = self.groups.clone();
        if !self.repos.is_empty() {
            let mut all: Vec<String> = self.repos.keys().cloned().collect();
            all.sort();
            out.insert(crate::constants::ALL_GROUP_NAME.to_string(), all);
        }
        out
    }

    /// Inverse index: map each registered repo alias to the **named** group(s)
    /// it belongs to (sorted, de-duplicated). Used by discoverability surfaces
    /// (`status`, the `scope_required` error) so an agent can tell that, e.g.,
    /// `"repo-a"` is a member of group `"group-a"` and prefer a cross-repo
    /// `group=` query over a single-repo `project=` query.
    ///
    /// Deliberate exclusions:
    /// - The virtual `"all"` group is never included — every repo belongs to it,
    ///   so it would be pure noise and drown the high-signal membership. (This
    ///   exclusion is *implicit*: `"all"` is never persisted in `self.groups`
    ///   — it is synthesized on demand by `groups_with_virtual_all` /
    ///   `resolve_group` — so iterating `self.groups` simply never sees it. A
    ///   future change that starts persisting `"all"` would need to filter it
    ///   here explicitly.)
    /// - `"@remote"` group members are skipped — they are federation peers, not
    ///   local project aliases.
    /// - Aliases that belong to no named group are omitted entirely (no empty
    ///   entries).
    pub fn project_groups(&self) -> std::collections::HashMap<String, Vec<String>> {
        let mut out: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (group, members) in &self.groups {
            for member in members {
                // Skip federation references ("@peer") — not local projects.
                if member.starts_with(REMOTE_REF_PREFIX) {
                    continue;
                }
                // Only map known local aliases.
                if self.repos.contains_key(member) {
                    out.entry(member.clone()).or_default().push(group.clone());
                }
            }
        }
        for groups in out.values_mut() {
            groups.sort();
            groups.dedup();
        }
        out
    }

    pub fn remove_group(&mut self, name: &str) -> bool {
        self.groups.remove(name).is_some()
    }

    /// Register (or overwrite) a remote federation peer under `name`.
    ///
    /// The peer becomes referenceable from a group as `"@<name>"`. Adding a
    /// remote does NOT, by itself, make it queryable — the `"@<name>"` reference
    /// must also be added to a group (see [`add_remote_to_group`]).
    ///
    /// Validates that the name is non-empty and does not itself carry the
    /// `@` reference prefix (which is added automatically in group members),
    /// and that the peer URL is non-empty.
    pub fn add_remote(&mut self, name: String, peer: RemotePeer) -> Result<()> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(anyhow::anyhow!("Remote peer name must not be empty"));
        }
        if trimmed.starts_with(REMOTE_REF_PREFIX) {
            return Err(anyhow::anyhow!(
                "Remote peer name must not start with '{}' — that prefix is only used inside group references (e.g. group member \"@{}\")",
                REMOTE_REF_PREFIX,
                trimmed.trim_start_matches(REMOTE_REF_PREFIX)
            ));
        }
        // A peer name is the first segment of a mounted project's namespaced name
        // (`<peer>/<alias>`). Allowing `/` here would break `resolve_remote_project`,
        // which splits on the FIRST separator — enforce the invariant the
        // REMOTE_PROJECT_SEPARATOR doc-comment promises.
        if trimmed.contains(REMOTE_PROJECT_SEPARATOR) {
            return Err(anyhow::anyhow!(
                "Remote peer name '{}' must not contain '{}' — that separator delimits <peer>/<alias> in mounted remote projects",
                trimmed,
                REMOTE_PROJECT_SEPARATOR
            ));
        }
        if peer.url.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Remote peer '{}' must have a non-empty url",
                trimmed
            ));
        }
        self.remotes.insert(trimmed.to_string(), peer);
        Ok(())
    }

    /// Remove a remote peer and prune every `"@<name>"` reference to it from all
    /// groups; groups left empty by the prune are dropped. Returns `false` when
    /// no peer of that name was registered.
    pub fn remove_remote(&mut self, name: &str) -> bool {
        if self.remotes.remove(name).is_none() {
            return false;
        }
        let reference = format!("{REMOTE_REF_PREFIX}{name}");
        for members in self.groups.values_mut() {
            members.retain(|m| m != &reference);
        }
        self.groups.retain(|_, members| !members.is_empty());
        true
    }

    /// Add a `"@<remote_name>"` reference to `group`, creating the group if it
    /// does not exist. Idempotent — a reference already present is not
    /// duplicated. The reserved virtual `"all"` group never federates and
    /// cannot be targeted. Errors when the remote peer is unknown.
    pub fn add_remote_to_group(&mut self, group: String, remote_name: &str) -> Result<()> {
        if group == crate::constants::ALL_GROUP_NAME {
            return Err(anyhow::anyhow!(
                "Group name '{}' is reserved — it always resolves to all registered repos and never federates.",
                group
            ));
        }
        if !self.remotes.contains_key(remote_name) {
            return Err(anyhow::anyhow!(
                "Unknown remote peer '{}' — add it first with `codesearch remote add`.",
                remote_name
            ));
        }
        let reference = format!("{REMOTE_REF_PREFIX}{remote_name}");
        let members = self.groups.entry(group).or_default();
        if !members.contains(&reference) {
            members.push(reference);
        }
        Ok(())
    }

    /// Named groups that reference the given remote peer as `"@<name>"`
    /// (sorted). Used by the `remote list` surface to show where a peer is wired
    /// in. The virtual `"all"` group never federates, so it is never included.
    pub fn groups_referencing_remote(&self, remote_name: &str) -> Vec<String> {
        let reference = format!("{REMOTE_REF_PREFIX}{remote_name}");
        let mut out: Vec<String> = self
            .groups
            .iter()
            .filter(|(_, members)| members.contains(&reference))
            .map(|(name, _)| name.clone())
            .collect();
        out.sort();
        out
    }

    pub fn alias_for_path(&self, path: &Path) -> Option<String> {
        // See unregister_path: normalize_user_path on the fallback preserves
        // lookup symmetry for not-on-disk paths.
        let canonical = safe_canonicalize(path).unwrap_or_else(|_| normalize_user_path(path));
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
    // `git` is spawned once per candidate directory. On Windows/msys (and Unix
    // under heavy parallel load) the OS can transiently refuse to fork the
    // subprocess (EAGAIN / "Resource temporarily unavailable"). Treating that
    // transient spawn failure the same as "no remote" would silently strip a
    // repo's git identity — breaking relocation and causing valid repos to be
    // pruned. Retry a few times with a short backoff on spawn failure; a
    // definitive `NotFound` (git not installed) returns immediately, and an
    // `Ok` result whose status is non-success (not a repo / no origin) is a
    // real answer that is NOT retried.
    const MAX_ATTEMPTS: u32 = 8;
    let mut output = None;
    for attempt in 0..MAX_ATTEMPTS {
        match std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["config", "--get", "remote.origin.url"])
            .output()
        {
            Ok(o) => {
                output = Some(o);
                break;
            }
            // git binary genuinely absent — retrying cannot help.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            // Transient spawn failure (fork exhaustion). Back off and retry.
            Err(_) => {
                if attempt + 1 < MAX_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(20 * (attempt as u64 + 1)));
                }
            }
        }
    }

    let output = output?;
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
            // normalize_user_path on the fallback is defensive (paths here come
            // from a filesystem scan, so they are already clean Windows paths,
            // but the helper is a no-op then).
            out.push(safe_canonicalize(dir).unwrap_or_else(|_| normalize_user_path(dir)));
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
#[path = "repos_tests.rs"]
mod tests;
