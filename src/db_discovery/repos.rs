use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cache::{safe_canonicalize, strip_unc_prefix};
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
    // `git` is spawned once per candidate directory. On Windows/msys (and Unix
    // under heavy parallel load) the OS can transiently refuse to fork the
    // subprocess (EAGAIN / "Resource temporarily unavailable"). Treating that
    // transient spawn failure the same as "no remote" would silently strip a
    // repo's git identity — breaking relocation and causing valid repos to be
    // pruned. Retry a few times with a short backoff on spawn failure; a
    // definitive `NotFound` (git not installed) returns immediately, and an
    // `Ok` result whose status is non-success (not a repo / no origin) is a
    // real answer that is NOT retried.
    const MAX_ATTEMPTS: u32 = 5;
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

    /// Process-wide lock serializing the git-spawning / directory-renaming
    /// relocation tests.
    ///
    /// These tests `git init` a directory and then rename it. On Windows the OS
    /// indexer / antivirus scans each freshly-created `.git` tree and holds
    /// handles on it, which blocks the rename ("Access is denied"). When many
    /// such tests run concurrently the scanner is overwhelmed and the handles
    /// linger for many seconds — long enough to exhaust even a generous
    /// `rename_retry`. Serializing them so only one `.git` tree is created/
    /// renamed at a time keeps each scan window short and the rename reliable.
    static GIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Acquire the relocation-test serialization lock, recovering from a
    /// poisoned mutex (a panic in one test must not cascade-fail the rest).
    fn git_serial_lock() -> std::sync::MutexGuard<'static, ()> {
        GIT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Initialise a git repo at `dir` with an `origin` remote pointing at `url`.
    fn init_git_remote(dir: &Path, url: &str) {
        // Retry on transient spawn failure (fork exhaustion under parallel test
        // load on Windows/msys); only a genuine missing-git binary is fatal.
        let run = |args: &[&str]| {
            for attempt in 0..5u64 {
                match std::process::Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .output()
                {
                    Ok(o) => return o,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        panic!("git not available in test env: {e}");
                    }
                    Err(_) if attempt < 4 => {
                        std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
                    }
                    Err(e) => panic!("git spawn failed after retries: {e}"),
                }
            }
            unreachable!("loop returns or panics")
        };
        run(&["init"]);
        run(&["remote", "add", "origin", url]);
    }

    /// Rename a directory with automatic retries.
    ///
    /// On Windows, git subprocesses spawned by `init_git_remote` (and the OS
    /// file indexer / antivirus) may keep a handle on the directory open
    /// briefly after the process exits, so `std::fs::rename` fails with
    /// "Access is denied". Under heavy parallel test load those handles linger
    /// longer, so we use a generous retry budget with a capped back-off
    /// (~7s worst case; in practice it succeeds on the first or second try).
    #[track_caller]
    fn rename_retry(from: &Path, to: &Path) {
        const MAX_ATTEMPTS: u64 = 40;
        let mut last_err = None;
        for attempt in 0..MAX_ATTEMPTS {
            match std::fs::rename(from, to) {
                Ok(()) => return,
                Err(e) => {
                    last_err = Some(e);
                    // Ramp the back-off but cap it so the total budget stays
                    // bounded even under sustained handle contention.
                    let backoff = (20 * (attempt + 1)).min(250);
                    std::thread::sleep(std::time::Duration::from_millis(backoff));
                }
            }
        }
        panic!(
            "rename {:?} → {:?} failed after {} attempts: {}",
            from,
            to,
            MAX_ATTEMPTS,
            last_err.unwrap()
        );
    }

    #[test]
    fn captures_git_remote_on_register() {
        let _serial = git_serial_lock();
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
        let _serial = git_serial_lock();
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
        let _serial = git_serial_lock();
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
        let _serial = git_serial_lock();
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
        let _serial = git_serial_lock();
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
        let _serial = git_serial_lock();
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
        let _serial = git_serial_lock();
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
        let _serial = git_serial_lock();
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
        let _serial = git_serial_lock();
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

    // ── Virtual "all" group (issue #131) ───────────────────────────────

    #[test]
    fn add_group_rejects_reserved_all_name() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("repo-a".to_string(), PathBuf::from("/tmp/repo-a"));

        let err = cfg
            .add_group("all".to_string(), vec!["repo-a".to_string()])
            .unwrap_err();
        assert!(
            err.to_string().contains("reserved"),
            "expected 'reserved' in error, got: {}",
            err
        );
    }

    #[test]
    fn resolve_group_all_returns_every_registered_repo() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("repo-a".to_string(), PathBuf::from("/tmp/repo-a"));
        cfg.repos
            .insert("repo-b".to_string(), PathBuf::from("/tmp/repo-b"));

        let resolved = cfg.resolve_group(crate::constants::ALL_GROUP_NAME);
        let mut names: Vec<String> = resolved.into_iter().map(|(a, _)| a).collect();
        names.sort();
        assert_eq!(names, vec!["repo-a".to_string(), "repo-b".to_string()]);
    }

    #[test]
    fn resolve_group_all_is_empty_when_no_repos_registered() {
        let cfg = ReposConfig::default();
        let resolved = cfg.resolve_group(crate::constants::ALL_GROUP_NAME);
        assert!(resolved.is_empty());
    }

    #[test]
    fn groups_with_virtual_all_advertises_all_without_storing_it() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("repo-a".to_string(), PathBuf::from("/tmp/repo-a"));
        cfg.repos
            .insert("repo-b".to_string(), PathBuf::from("/tmp/repo-b"));
        cfg.add_group("platform".to_string(), vec!["repo-a".to_string()])
            .unwrap();

        // The advertised map includes both the real group and "all".
        let advertised = cfg.groups_with_virtual_all();
        assert_eq!(advertised.len(), 2);
        let mut all_members = advertised
            .get(crate::constants::ALL_GROUP_NAME)
            .unwrap()
            .clone();
        all_members.sort();
        assert_eq!(
            all_members,
            vec!["repo-a".to_string(), "repo-b".to_string()]
        );

        // But the stored config is untouched — "all" must never be persisted.
        assert!(
            !cfg.groups.contains_key(crate::constants::ALL_GROUP_NAME),
            "\"all\" must not leak into the stored groups map"
        );
    }

    #[test]
    fn project_groups_maps_aliases_to_named_groups() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("repo-a".to_string(), PathBuf::from("/tmp/a"));
        cfg.repos
            .insert("repo-b".to_string(), PathBuf::from("/tmp/b"));
        cfg.repos
            .insert("lonely".to_string(), PathBuf::from("/tmp/lonely"));
        // repo-a is a member of two named groups.
        cfg.add_group(
            "group-x".to_string(),
            vec!["repo-a".to_string(), "repo-b".to_string()],
        )
        .unwrap();
        cfg.add_group("group-y".to_string(), vec!["repo-a".to_string()])
            .unwrap();

        let pg = cfg.project_groups();

        // Multi-group membership is sorted + de-duplicated.
        assert_eq!(
            pg.get("repo-a"),
            Some(&vec!["group-x".to_string(), "group-y".to_string()])
        );
        assert_eq!(pg.get("repo-b"), Some(&vec!["group-x".to_string()]));
        // A repo in no named group is omitted entirely (no empty entry).
        assert!(!pg.contains_key("lonely"));
    }

    #[test]
    fn project_groups_excludes_virtual_all_and_remote_refs() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("local-a".to_string(), PathBuf::from("/tmp/a"));
        cfg.remotes
            .insert("cloud".to_string(), make_peer("https://cloud"));
        cfg.groups.insert(
            "docs".to_string(),
            vec!["local-a".to_string(), "@cloud".to_string()],
        );

        let pg = cfg.project_groups();

        // Only the local alias is mapped; "@cloud" never appears as a key.
        assert_eq!(pg.get("local-a"), Some(&vec!["docs".to_string()]));
        assert!(!pg.contains_key("@cloud"));
        assert!(!pg.contains_key("cloud"));
        // The virtual "all" group is never a member entry.
        for groups in pg.values() {
            assert!(!groups.contains(&crate::constants::ALL_GROUP_NAME.to_string()));
        }
    }

    // ── Federation: remotes + resolve_group_targets ───────────────────

    fn make_peer(url: &str) -> RemotePeer {
        RemotePeer {
            url: url.to_string(),
            api_key: "secret".to_string(),
            group: Some("docs".to_string()),
            timeout_secs: Some(15),
        }
    }

    #[test]
    fn resolve_group_targets_expands_local_and_remote_members() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("local-a".to_string(), PathBuf::from("/tmp/a"));
        cfg.remotes
            .insert("cloud".to_string(), make_peer("https://cloud"));
        cfg.groups.insert(
            "docs".to_string(),
            vec!["local-a".to_string(), "@cloud".to_string()],
        );

        let targets = cfg.resolve_group_targets("docs");
        assert_eq!(targets.len(), 2);
        // Local member expands to a Local target.
        assert!(matches!(
            &targets[0],
            Target::Local { alias, .. } if alias == "local-a"
        ));
        // Remote member expands to a Remote target carrying the peer config.
        match &targets[1] {
            Target::Remote { peer_name, peer } => {
                assert_eq!(peer_name, "cloud");
                assert_eq!(peer.url, "https://cloud");
            }
            other => panic!("expected Remote, got {:?}", other),
        }
    }

    #[test]
    fn split_group_targets_partitions_locals_and_remotes() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("local-a".to_string(), PathBuf::from("/tmp/a"));
        cfg.repos
            .insert("local-b".to_string(), PathBuf::from("/tmp/b"));
        cfg.remotes
            .insert("cloud".to_string(), make_peer("https://cloud"));
        cfg.groups.insert(
            "docs".to_string(),
            vec![
                "@cloud".to_string(),
                "local-a".to_string(),
                "local-b".to_string(),
            ],
        );

        let (locals, remotes) = cfg.split_group_targets("docs");
        assert_eq!(locals.len(), 2);
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].0, "cloud");
    }

    fn cfg_with_cloud() -> ReposConfig {
        let mut cfg = ReposConfig::default();
        cfg.remotes
            .insert("cloud".to_string(), make_peer("https://cloud"));
        cfg
    }

    #[test]
    fn mounted_remote_projects_namespaces_and_sorts() {
        let mut cfg = cfg_with_cloud();
        // Opt-in allowlist, deliberately out of order to prove sorting.
        cfg.remote_mounts = vec!["cloud/bynder".to_string(), "cloud/akeneo".to_string()];
        let mounts = cfg.mounted_remote_projects();
        // Sorted by local name: cloud/akeneo before cloud/bynder.
        let names: Vec<&str> = mounts.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["cloud/akeneo", "cloud/bynder"]);
        match &mounts[0].1 {
            Target::RemoteProject {
                peer_name,
                remote_alias,
                peer,
            } => {
                assert_eq!(peer_name, "cloud");
                assert_eq!(remote_alias, "akeneo"); // bare alias, un-namespaced
                assert_eq!(peer.url, "https://cloud");
            }
            other => panic!("expected RemoteProject, got {:?}", other),
        }
    }

    #[test]
    fn mounted_remote_projects_only_allowlisted_and_skips_unknown_peer() {
        let mut cfg = cfg_with_cloud();
        // akeneo opted in; an entry for an unknown peer must be ignored entirely.
        // (bynder is available on the peer but NOT mounted, so it never appears.)
        cfg.remote_mounts = vec!["cloud/akeneo".to_string(), "ghost/x".to_string()];
        let mounts = cfg.mounted_remote_projects();
        let names: Vec<&str> = mounts.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["cloud/akeneo"]);
    }

    #[test]
    fn mounted_remote_projects_applies_rename_override() {
        let mut cfg = cfg_with_cloud();
        cfg.remote_mounts = vec!["cloud/akeneo".to_string()];
        cfg.remote_alias_overrides
            .insert("cloud/akeneo".to_string(), "pim".to_string());
        let mounts = cfg.mounted_remote_projects();
        assert_eq!(mounts[0].0, "pim"); // local name is the rename
        match &mounts[0].1 {
            // ...but the peer still receives the bare original alias.
            Target::RemoteProject { remote_alias, .. } => assert_eq!(remote_alias, "akeneo"),
            other => panic!("expected RemoteProject, got {:?}", other),
        }
    }

    #[test]
    fn resolve_remote_project_requires_mount_rename_and_negatives() {
        let mut cfg = cfg_with_cloud();
        cfg.remote_mounts = vec!["cloud/akeneo".to_string(), "cloud/bynder".to_string()];
        cfg.remote_alias_overrides
            .insert("cloud/akeneo".to_string(), "pim".to_string());
        cfg.repos
            .insert("local-a".to_string(), PathBuf::from("/tmp/a"));

        // A mounted canonical "<peer>/<alias>" resolves.
        assert!(matches!(
            cfg.resolve_remote_project("cloud/bynder"),
            Some(Target::RemoteProject { ref remote_alias, .. }) if remote_alias == "bynder"
        ));
        // A rename of a mounted project resolves back to its canonical alias.
        assert!(matches!(
            cfg.resolve_remote_project("pim"),
            Some(Target::RemoteProject { ref remote_alias, .. }) if remote_alias == "akeneo"
        ));
        // Un-mounted (peer has it but user didn't opt in), unknown peer, and
        // plain local aliases do not resolve remotely.
        assert!(cfg.resolve_remote_project("cloud/secret").is_none());
        assert!(cfg.resolve_remote_project("ghost/x").is_none());
        assert!(cfg.resolve_remote_project("local-a").is_none());
    }

    #[test]
    fn mount_and_unmount_remote_project_roundtrip() {
        let mut cfg = cfg_with_cloud();
        cfg.mount_remote_project("cloud/akeneo").unwrap();
        cfg.mount_remote_project("cloud/akeneo").unwrap(); // idempotent
        assert_eq!(cfg.remote_mounts, vec!["cloud/akeneo".to_string()]);
        // Unknown peer and malformed names are rejected.
        assert!(cfg.mount_remote_project("ghost/x").is_err());
        assert!(cfg.mount_remote_project("no-separator").is_err());
        assert!(cfg.mount_remote_project("cloud/").is_err());
        // Unmount drops the mount and any orphaned rename override.
        cfg.remote_alias_overrides
            .insert("cloud/akeneo".to_string(), "pim".to_string());
        assert!(cfg.unmount_remote_project("cloud/akeneo"));
        assert!(cfg.remote_mounts.is_empty());
        assert!(!cfg.remote_alias_overrides.contains_key("cloud/akeneo"));
        assert!(!cfg.unmount_remote_project("cloud/akeneo")); // already gone
    }

    #[test]
    fn group_remote_projects_only_mounted_members_of_referenced_peers() {
        let mut cfg = cfg_with_cloud();
        cfg.remote_mounts = vec!["cloud/akeneo".to_string(), "cloud/bynder".to_string()];
        cfg.groups
            .insert("docs".to_string(), vec!["@cloud".to_string()]);
        let projs = cfg.group_remote_projects("docs");
        let aliases: Vec<&str> = projs.iter().map(|(_, _, a)| a.as_str()).collect();
        assert_eq!(aliases, vec!["akeneo", "bynder"]);

        // A group that references no peer yields nothing.
        cfg.groups
            .insert("solo".to_string(), vec!["@cloud".to_string()]);
        cfg.remote_mounts.clear();
        assert!(cfg.group_remote_projects("solo").is_empty());
        // The virtual "all" group never federates.
        assert!(cfg
            .group_remote_projects(crate::constants::ALL_GROUP_NAME)
            .is_empty());
    }

    #[test]
    fn reconcile_prunes_mounts_with_unknown_peer_or_malformed_name() {
        let mut cfg = cfg_with_cloud();
        cfg.remote_mounts = vec![
            "cloud/akeneo".to_string(), // keep
            "ghost/x".to_string(),      // unknown peer → prune
            "malformed".to_string(),    // no separator → prune
            "cloud/".to_string(),       // empty alias → prune
        ];
        cfg.remote_alias_overrides
            .insert("ghost/x".to_string(), "orphan".to_string());
        cfg.reconcile();
        assert_eq!(cfg.remote_mounts, vec!["cloud/akeneo".to_string()]);
        // Rename override orphaned by the prune is dropped too.
        assert!(!cfg.remote_alias_overrides.contains_key("ghost/x"));
    }

    #[test]
    fn resolve_group_targets_all_never_federates() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("local-a".to_string(), PathBuf::from("/tmp/a"));
        cfg.remotes
            .insert("cloud".to_string(), make_peer("https://cloud"));
        // Even if a group "docs" federates, querying "all" must stay local.
        cfg.groups
            .insert("docs".to_string(), vec!["@cloud".to_string()]);

        let targets = cfg.resolve_group_targets(crate::constants::ALL_GROUP_NAME);
        assert!(targets.iter().all(|t| matches!(t, Target::Local { .. })));
        assert_eq!(targets.len(), 1); // local-a only
    }

    #[test]
    fn resolve_group_targets_skips_unknown_remote_ref() {
        let mut cfg = ReposConfig::default();
        cfg.groups.insert(
            "docs".to_string(),
            vec!["@ghost".to_string()], // no `remotes` entry for "ghost"
        );

        let targets = cfg.resolve_group_targets("docs");
        assert!(targets.is_empty(), "unknown remote ref must be skipped");
    }

    #[test]
    fn reconcile_prunes_unknown_remote_ref_and_drops_now_empty_group() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("real".to_string(), PathBuf::from("/tmp/real"));
        cfg.groups
            .insert("docs".to_string(), vec!["@ghost".to_string()]);
        // Only "ghost" is referenced but "cloud" exists → "ghost" pruned, group
        // becomes empty and is dropped.
        cfg.remotes
            .insert("cloud".to_string(), make_peer("https://cloud"));

        cfg.reconcile();
        assert!(
            !cfg.groups.contains_key("docs"),
            "empty group must be dropped"
        );
    }

    #[test]
    fn reconcile_keeps_valid_remote_ref() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("real".to_string(), PathBuf::from("/tmp/real"));
        cfg.remotes
            .insert("cloud".to_string(), make_peer("https://cloud"));
        cfg.groups.insert(
            "docs".to_string(),
            vec!["real".to_string(), "@cloud".to_string()],
        );

        cfg.reconcile();
        assert_eq!(
            cfg.groups.get("docs"),
            Some(&vec!["real".to_string(), "@cloud".to_string()]),
            "valid local alias AND valid remote ref must both survive reconcile"
        );
    }

    #[test]
    fn remotes_roundtrip_through_json() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("local-a".to_string(), PathBuf::from("/tmp/a"));
        cfg.remotes
            .insert("cloud".to_string(), make_peer("https://cloud"));
        cfg.groups
            .insert("docs".to_string(), vec!["@cloud".to_string()]);

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("repos.json");
        cfg.save_to(&path).unwrap();

        let loaded = ReposConfig::load_from(&path).unwrap();
        assert_eq!(loaded.remotes.len(), 1);
        let peer = loaded.remotes.get("cloud").unwrap();
        assert_eq!(peer.url, "https://cloud");
        assert_eq!(peer.api_key, "secret");
        assert_eq!(peer.group.as_deref(), Some("docs"));
        // Group with remote ref survives the load+reconcile roundtrip.
        assert_eq!(loaded.groups.get("docs"), Some(&vec!["@cloud".to_string()]));
    }

    #[test]
    fn add_remote_inserts_and_overwrites() {
        let mut cfg = ReposConfig::default();
        cfg.add_remote("cloud".to_string(), make_peer("https://cloud"))
            .unwrap();
        assert_eq!(cfg.remotes.get("cloud").unwrap().url, "https://cloud");
        // Overwrite with a new URL.
        cfg.add_remote("cloud".to_string(), make_peer("https://cloud2"))
            .unwrap();
        assert_eq!(cfg.remotes.len(), 1);
        assert_eq!(cfg.remotes.get("cloud").unwrap().url, "https://cloud2");
    }

    #[test]
    fn add_remote_rejects_empty_name_prefixed_name_and_empty_url() {
        let mut cfg = ReposConfig::default();
        assert!(cfg
            .add_remote("  ".to_string(), make_peer("https://cloud"))
            .is_err());
        assert!(cfg
            .add_remote("@cloud".to_string(), make_peer("https://cloud"))
            .is_err());
        let mut blank = make_peer("https://cloud");
        blank.url = "  ".to_string();
        assert!(cfg.add_remote("cloud".to_string(), blank).is_err());
        // A '/' in a peer name would break <peer>/<alias> namespacing — rejected.
        assert!(cfg
            .add_remote("a/b".to_string(), make_peer("https://cloud"))
            .is_err());
        assert!(cfg.remotes.is_empty());
    }

    #[test]
    fn add_remote_trims_name() {
        let mut cfg = ReposConfig::default();
        cfg.add_remote("  cloud  ".to_string(), make_peer("https://cloud"))
            .unwrap();
        assert!(cfg.remotes.contains_key("cloud"));
    }

    #[test]
    fn add_remote_to_group_creates_and_is_idempotent() {
        let mut cfg = ReposConfig::default();
        cfg.add_remote("cloud".to_string(), make_peer("https://cloud"))
            .unwrap();
        cfg.add_remote_to_group("docs".to_string(), "cloud")
            .unwrap();
        cfg.add_remote_to_group("docs".to_string(), "cloud")
            .unwrap(); // idempotent
        assert_eq!(cfg.groups.get("docs"), Some(&vec!["@cloud".to_string()]));
    }

    #[test]
    fn add_remote_to_group_rejects_reserved_all_and_unknown_remote() {
        let mut cfg = ReposConfig::default();
        cfg.add_remote("cloud".to_string(), make_peer("https://cloud"))
            .unwrap();
        assert!(cfg
            .add_remote_to_group(crate::constants::ALL_GROUP_NAME.to_string(), "cloud")
            .is_err());
        assert!(cfg
            .add_remote_to_group("docs".to_string(), "ghost")
            .is_err());
    }

    #[test]
    fn remove_remote_prunes_group_references_and_empties() {
        let mut cfg = ReposConfig::default();
        cfg.repos
            .insert("local-a".to_string(), PathBuf::from("/tmp/a"));
        cfg.add_remote("cloud".to_string(), make_peer("https://cloud"))
            .unwrap();
        cfg.groups.insert(
            "docs".to_string(),
            vec!["local-a".to_string(), "@cloud".to_string()],
        );
        cfg.groups
            .insert("cloud-only".to_string(), vec!["@cloud".to_string()]);

        assert!(cfg.remove_remote("cloud"));
        assert!(!cfg.remotes.contains_key("cloud"));
        // The mixed group keeps its local member but drops the remote ref.
        assert_eq!(cfg.groups.get("docs"), Some(&vec!["local-a".to_string()]));
        // The group that only referenced the remote is dropped entirely.
        assert!(!cfg.groups.contains_key("cloud-only"));
    }

    #[test]
    fn remove_remote_returns_false_for_unknown() {
        let mut cfg = ReposConfig::default();
        assert!(!cfg.remove_remote("ghost"));
    }

    #[test]
    fn groups_referencing_remote_lists_sorted_groups() {
        let mut cfg = ReposConfig::default();
        cfg.add_remote("cloud".to_string(), make_peer("https://cloud"))
            .unwrap();
        cfg.groups
            .insert("zeta".to_string(), vec!["@cloud".to_string()]);
        cfg.groups
            .insert("alpha".to_string(), vec!["@cloud".to_string()]);
        cfg.groups
            .insert("other".to_string(), vec!["@somewhere".to_string()]);
        assert_eq!(
            cfg.groups_referencing_remote("cloud"),
            vec!["alpha".to_string(), "zeta".to_string()]
        );
    }

    #[test]
    fn remotes_alias_base_url_field() {
        // The `url` field accepts the friendlier `base_url` alias for ergonomics.
        let json = r#"{
            "repos": {"a": "/tmp/a"},
            "remotes": {"cloud": {"base_url": "https://cloud", "api_key": "k"}}
        }"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("repos.json");
        std::fs::write(&path, json).unwrap();
        let cfg = ReposConfig::load_from(&path).unwrap();
        assert_eq!(cfg.remotes.get("cloud").unwrap().url, "https://cloud");
    }
}
