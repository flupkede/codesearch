use std::path::Path;

// === Multi-store fan-out traits ===

/// Trait for types that have a chunk ID (used for deduplication in group fan-out).
pub(crate) trait HasChunkId {
    fn chunk_id(&self) -> u32;
}

/// Trait for types that have a relevance score (used for sorting in group fan-out).
pub(crate) trait HasScore {
    fn score(&self) -> f32;
}

impl HasChunkId for crate::vectordb::SearchResult {
    fn chunk_id(&self) -> u32 {
        self.id
    }
}

impl HasScore for crate::vectordb::SearchResult {
    fn score(&self) -> f32 {
        self.score
    }
}

impl HasChunkId for crate::fts::FtsResult {
    fn chunk_id(&self) -> u32 {
        self.chunk_id
    }
}

impl HasScore for crate::fts::FtsResult {
    fn score(&self) -> f32 {
        self.score
    }
}

// === Simple Glob Matcher ===

pub(crate) fn normalize_tool_path(path: &str, project_root: &Path) -> String {
    let p = Path::new(path);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        project_root.join(p)
    };
    crate::cache::normalize_path_str(resolved.to_string_lossy().as_ref())
}

/// Strip a project-alias prefix from a tool path.
///
/// In serve mode, tools like explore receive `target = "ALIAS/src/foo.rs"` with
/// `project = "ALIAS"`.  The alias prefix must be stripped before calling
/// `chunks_for_file`, which expects a path relative to the project root.
pub(crate) fn strip_alias_prefix(path: &str, alias: Option<&String>) -> String {
    if let Some(a) = alias {
        let prefix = format!("{}/", a);
        match path.strip_prefix(&prefix) {
            Some(rest) => rest.to_string(),
            None => path.to_string(),
        }
    } else {
        path.to_string()
    }
}

/// Prefix a result path with its repo alias for group queries, normalizing
/// Windows backslashes to forward slashes in the process. When `alias` is
/// None or empty, the path is still normalized (useful for stdio mode).
pub(crate) fn prefix_path_with_alias(
    path: &str,
    alias: Option<&str>,
    project_root: &str,
) -> String {
    let normalized = crate::cache::normalize_path_str(path);
    let normalized_root = crate::cache::normalize_path_str(project_root)
        .trim_end_matches('/')
        .to_string();
    match normalized.strip_prefix(&normalized_root) {
        Some(rest) => {
            let relative = rest.trim_start_matches('/');
            match alias {
                Some(a) if !a.is_empty() => format!("{}/{}", a, relative),
                _ => relative.to_string(),
            }
        }
        None => normalized,
    }
}

/// Prefix a result path with the matching repo alias from a set of aliases and their roots.
/// Used by handlers that have alias/root info but not a full `MultiStoreContext`.
pub(crate) fn prefix_path_multi(
    path: &str,
    aliases: &[String],
    alias_roots: &std::collections::HashMap<String, String>,
) -> String {
    let normalized = crate::cache::normalize_path_str(path);
    for alias in aliases {
        if let Some(root) = alias_roots.get(alias) {
            if normalized.starts_with(root.as_str()) {
                return prefix_path_with_alias(path, Some(alias), root);
            }
        }
    }
    normalized
}

/// Pick the project root to relativise a result path against for a `filter_path`
/// prefix match, so `filter_path` is interpreted **relative to the repo root**
/// in every routing mode:
/// - serve single-project routing → the routed alias's root (`alias_roots[alias]`);
/// - serve multi/group → the longest alias root the (absolute) path lives under;
/// - stdio single-repo (no alias roots) → the service's own `project_path`
///   (`fallback_root`).
///
/// Before this, the filter always used the service's `project_path`, which for a
/// serve-routed project is NOT the routed repo's root — so the absolute stored
/// path never relativised and every hit was dropped. The federated paths solve
/// the same class of bug client-side (see `retain_by_filter_path`); this covers
/// the local (non-federated) serve/multi case.
pub(crate) fn pick_filter_root(
    path: &str,
    project_alias: Option<&str>,
    alias_roots: &std::collections::HashMap<String, String>,
    fallback_root: &str,
) -> String {
    if let Some(alias) = project_alias {
        if let Some(root) = alias_roots.get(alias) {
            return root.clone();
        }
    }
    if !alias_roots.is_empty() {
        let normalized = crate::cache::normalize_path_str(path);
        if let Some(root) = alias_roots
            .values()
            .filter(|r| normalized.starts_with(r.as_str()))
            .max_by_key(|r| r.len())
        {
            return root.clone();
        }
    }
    fallback_root.to_string()
}
