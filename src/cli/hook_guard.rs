//! `codesearch hook edit-guard` / `codesearch hook edit-guard-post` — the
//! native port of the Claude Code edit-guard hook scripts
//! (`integrations/claude-code/hooks/edit-guard{,-post}.*` plus the shared
//! `codesearch-common` helpers they source). One process per hook call: no
//! pwsh/bash, no jq, no spawned `git`.
//!
//! Contract, identical to the scripts (spec: commit e5c7d5d):
//! - the event JSON arrives on stdin
//! - stdout carries the same JSON the scripts write; a silent allow is
//!   empty stdout
//! - the process ALWAYS exits 0, even on internal error (fail-open), so
//!   the entry points swallow every failure
//!
//! Windows drive-letter paths fold case in the comparisons (NTFS), UNC and
//! POSIX paths compare case-sensitively — mirroring codesearch-common.ps1.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::utils::rename_retry::{
    is_transient_rename_error, RENAME_RETRY_ATTEMPTS, RENAME_RETRY_DELAY_MS,
};

/// Marker state file, shared with the script twins: `$TEMP/` (Windows) or
/// `${TMPDIR:-/tmp}` plus this name.
const STATE_FILE_NAME: &str = ".codesearch-edit-guard-state.json";
/// A consultation unblocks edits for this many seconds (5 minutes).
const WINDOW_SECS: i64 = 300;
/// Opts a pure remote-serve setup into enforcement without a local
/// repos.json. No core constant exists; the script twins use the bare name.
const SERVER_ENV: &str = "CODESEARCH_SERVER";

/// `Edit`/`Write`/`MultiEdit` — the PreToolUse tools edit-guard fires on.
const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit"];
/// Extensions answered by `find_impact` (SCIP-backed languages).
const IMPACT_EXTS: &[&str] = &["cs", "ts", "tsx", "mts", "cts"];
/// Other source code answered by `find(kind="usages")`, mirroring the
/// code-bearing classes of codesearch's own extension map plus
/// razor/cshtml. Docs/markup/config never gate.
const USAGES_EXTS: &[&str] = &[
    "js", "mjs", "cjs", "jsx", "py", "pyw", "pyi", "go", "rs", "java", "c", "h", "cpp", "cc",
    "cxx", "hpp", "hxx", "rb", "rake", "php", "swift", "kt", "kts", "dart", "sh", "bash", "zsh",
    "sql", "ipynb", "proto", "razor", "cshtml",
];

// ---
// CLI entry points — never fail, never exit non-zero.
// ---

/// `codesearch hook edit-guard` — PreToolUse guard. Reads the event JSON on
/// stdin, prints the deny decision when the edit needs a consultation; an
/// allow is silent. Internal errors (including panics) degrade to allow.
pub fn run_edit_guard() {
    let raw = read_stdin();
    let out = std::panic::catch_unwind(|| edit_guard(&raw)).unwrap_or_default();
    if !out.is_empty() {
        println!("{out}");
    }
}

/// `codesearch hook edit-guard-post` — PostToolUse companion recording the
/// consultation markers. Emits nothing, blocks nothing.
pub fn run_edit_guard_post() {
    let raw = read_stdin();
    let _ = std::panic::catch_unwind(|| edit_guard_post(&raw));
}

fn read_stdin() -> String {
    let mut buf = String::new();
    // Invalid UTF-8 or a closed stdin counts as "no input" — allow.
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf);
    buf
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---
// PreToolUse: edit-guard
// ---

/// The decision JSON for one edit event, or empty for a silent allow.
fn edit_guard(raw: &str) -> String {
    let Ok(data) = serde_json::from_str::<Value>(raw.trim()) else {
        return String::new();
    };
    let tool = data.get("tool_name").and_then(Value::as_str).unwrap_or("");
    if !EDIT_TOOLS.contains(&tool) {
        return String::new();
    }
    let inp = data.get("tool_input").unwrap_or(&Value::Null);
    let paths = edit_target_paths(inp);
    if paths.is_empty() {
        return String::new(); // nothing attributable -> allow (fail-open)
    }

    let now = unix_now();
    let state = read_state();
    let mut fail: Option<(String, &'static str)> = None;
    for p in &paths {
        let Some(kind) = edit_kind(p) else {
            continue; // non-code file -> allow (fail-open)
        };
        let Some(root) = resolve_target_git_root(p) else {
            continue; // outside any git repo -> allow
        };
        if !target_registered(&root) {
            continue; // unregistered repo -> fail-open
        }
        let key = norm_state_key(p);
        let root_key = norm_state_key(&root);
        if checked_recently(&state, &key, now) || checked_recently(&state, &root_key, now) {
            continue;
        }
        let needed = if kind == "impact" {
            "mcp__codesearch__find_impact (SCIP-backed language)"
        } else {
            "mcp__codesearch__find(symbol, kind=\"usages\")"
        };
        fail = Some((p.clone(), needed));
        break;
    }
    let Some((path, needed)) = fail else {
        return String::new(); // every path checked recently -> allow
    };

    let msg = format!(
        "edit-guard: codesearch consultation required before this edit.\n\n\
         Blocked: {path}\n\
         Run first: {needed} — on that exact file; any outcome counts\n\
         (\"no results\" / \"no SCIP backend\" too). Then retry this edit;\n\
         it stays allowed for 5 minutes."
    );
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": msg
        }
    })
    .to_string()
}

// ---
// PostToolUse: edit-guard-post
// ---

/// Record consultation markers for one tool result; always returns empty
/// stdout (PostToolUse cannot block).
fn edit_guard_post(raw: &str) -> String {
    let Ok(data) = serde_json::from_str::<Value>(raw.trim()) else {
        return String::new();
    };
    let tool = data.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let inp = data.get("tool_input").unwrap_or(&Value::Null);
    let mark_tool = match tool {
        "mcp__codesearch__find_impact" => "find_impact",
        "mcp__codesearch__find" => {
            // The matcher regex can only filter on tool name; the kind
            // compare happens here.
            let kind = inp.get("kind").map(ps_string).unwrap_or_default();
            if kind != "usages" {
                return String::new();
            }
            "find_usages"
        }
        _ => return String::new(),
    };

    let project = inp.get("project").map(ps_string).unwrap_or_default();
    let symbol = inp.get("symbol").map(ps_string).unwrap_or_default();
    let symbol_name = inp.get("symbol_name").map(ps_string).unwrap_or_default();
    let call_cwd = inp.get("cwd").map(ps_string).unwrap_or_default();

    // The cwd repo's registered root stands in for a missing, stale or
    // unregistered `project` alias.
    let cwd_root = if call_cwd.is_empty() {
        String::new()
    } else {
        resolve_target_git_root(&call_cwd)
            .filter(|c| target_registered(c))
            .unwrap_or_default()
    };

    let mut keys: Vec<String> = Vec::new();
    let mark = |keys: &mut Vec<String>, k: String| {
        if !k.is_empty() && !keys.contains(&k) {
            keys.push(k);
        }
    };

    // Source 1: a file-ish input field, first non-empty wins.
    for field in ["file", "path", "file_path"] {
        if let Some(v) = inp.get(field) {
            let candidate = ps_string(v);
            if candidate.is_empty() {
                continue;
            }
            mark(&mut keys, norm_state_key(&candidate));
            break;
        }
    }

    // Source 1b: a path-like `symbol` / `symbol_name` marks the path it
    // names — the exact call shapes edit-guard itself demands.
    for sym in [&symbol, &symbol_name] {
        if sym.is_empty() || !has_path_shape(sym) {
            continue;
        }
        let mut marked = false;
        if !project.is_empty() || !cwd_root.is_empty() {
            for abs in resolve_result_path(sym, &project, &cwd_root) {
                marked = true;
                mark(&mut keys, norm_state_key(&abs));
            }
        }
        if !marked {
            mark(&mut keys, norm_state_key(sym));
        }
    }

    // Source 2: every path the response mentions (file-path-shaped values
    // only — a bare symbol once fabricated a bogus <root>/<symbol> key).
    if let Some(resp) = data.get("tool_response").filter(|v| !v.is_null()) {
        for rp in collect_result_paths(resp) {
            for abs in resolve_result_path(&rp, &project, &cwd_root) {
                mark(&mut keys, norm_state_key(&abs));
            }
        }
    }

    // Source 3: a repo-level marker for EVERY qualifying consult —
    // per-file attribution cannot be exhaustive (find_impact answers with
    // callers, so the consulted file itself is often absent).
    let mut root = if project.is_empty() {
        String::new()
    } else {
        registered_project_root(&project).unwrap_or_default()
    };
    if root.is_empty() {
        root.clone_from(&cwd_root);
    }
    if !root.is_empty() {
        mark(&mut keys, norm_state_key(&root));
    }

    if keys.is_empty() {
        return String::new();
    }
    upsert_state(&keys, mark_tool);
    String::new()
}

/// Reload the state, prune expired entries, set every key, write atomically.
/// Corrupt/unreadable state starts over from this batch's markers.
fn upsert_state(keys: &[String], mark_tool: &str) {
    let now = unix_now();
    let mut state = read_state();
    state.retain(|_, entry| {
        let ts = entry.get("ts").and_then(parse_ts).unwrap_or(0); // non-numeric ts is corruption -> expired
        now - ts < WINDOW_SECS
    });
    for k in keys {
        state.insert(k.clone(), json!({ "tool": mark_tool, "ts": now }));
    }
    write_state(&state);
}

// ---
// Shared helpers (ports of codesearch-common)
// ---

/// Target paths of an Edit/Write/MultiEdit event: the primary `file_path`
/// plus, defensively, every per-edit entry — non-empty, order-unique.
fn edit_target_paths(inp: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(obj) = inp.as_object() else {
        return out;
    };
    if let Some(v) = obj.get("file_path") {
        let s = ps_string(v);
        if !s.is_empty() {
            out.push(s);
        }
    }
    if let Some(edits) = obj.get("edits").filter(|v| !v.is_null()) {
        let items: Vec<&Value> = match edits {
            Value::Array(a) => a.iter().collect(),
            other => vec![other], // a single object iterates once, like @() in the twin
        };
        for e in items {
            if let Some(v) = e.get("file_path") {
                let s = ps_string(v);
                if !s.is_empty() && !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// Which consultation a path needs before editing: "impact" for SCIP-backed
/// languages, "usages" for other source code, None for non-code files.
/// Keys on EXTENSION only: filename-mapped code files (Dockerfile, ...)
/// pass too — rare refactor targets.
fn edit_kind(path: &str) -> Option<&'static str> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if IMPACT_EXTS.contains(&ext.as_str()) {
        Some("impact")
    } else if USAGES_EXTS.contains(&ext.as_str()) {
        Some("usages")
    } else {
        None
    }
}

/// Git root of a target path — by walking up for `.git` (a directory, or a
/// file for worktrees). NEVER a spawned `git`: one hook call is one process.
/// Absolute paths resolve against the target's own root (walking up to the
/// nearest existing ancestor first, so glob operands resolve too); empty or
/// relative paths resolve against the cwd repo.
fn resolve_target_git_root(path: &str) -> Option<String> {
    let norm = path.trim_end_matches(['/', '\\']);
    if !looks_absolute(norm) {
        let cwd = std::env::current_dir().ok()?;
        return find_git_root_dir(&cwd);
    }
    let mut probe = PathBuf::from(norm);
    if !probe.exists() {
        while !probe.exists() {
            let parent = probe.parent()?.to_path_buf();
            if parent == probe {
                return None;
            }
            probe = parent;
        }
    }
    if probe.is_file() {
        probe = probe.parent()?.to_path_buf();
    }
    find_git_root_dir(&probe)
}

/// Walk up from `start` until a directory containing `.git` — that directory
/// is the root, for both plain repos and worktrees (`.git` file).
fn find_git_root_dir(start: &Path) -> Option<String> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current.display().to_string());
        }
        current = current.parent()?.to_path_buf();
    }
}

/// `^([A-Za-z]:[\\/]|/)` — Windows drive path or any POSIX/UNC absolute.
fn looks_absolute(s: &str) -> bool {
    s.starts_with('/') || starts_with_drive(s)
}

/// `^[A-Za-z]:[\\/]` — drive letter, colon, either separator.
fn starts_with_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

/// True when the normalized (forward-slash) form starts with a Windows
/// drive letter — the case-folding trigger.
fn is_drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/'
}

/// `\\?\` prefix stripped, backslashes folded to slashes, trailing slashes
/// trimmed (`/` itself stays).
fn norm_repo_path(p: &str) -> String {
    let mut s = p.strip_prefix(r"\\?\").unwrap_or(p).replace('\\', "/");
    while s != "/" && s.ends_with('/') {
        s.truncate(s.len() - 1);
    }
    s
}

/// Path equality for repo registration: exact on POSIX-style paths,
/// case-insensitive for Windows drive-letter paths (NTFS folds case).
fn repo_path_equal(a: &str, b: &str) -> bool {
    let a = norm_repo_path(a);
    let b = norm_repo_path(b);
    let fold = |s: String| {
        if is_drive_path(&s) {
            s.to_lowercase()
        } else {
            s
        }
    };
    fold(a) == fold(b)
}

/// The state-file key for a path: normalized, joined onto the cwd when
/// relative, drive-letter paths lowercased.
fn norm_state_key(p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    let norm = p.trim_end_matches(['/', '\\']);
    let joined;
    let effective = if looks_absolute(norm) {
        p
    } else {
        let Ok(cwd) = std::env::current_dir() else {
            return String::new();
        };
        joined = cwd.join(p).display().to_string();
        joined.as_str()
    };
    let k = norm_repo_path(effective);
    if is_drive_path(&k) {
        k.to_lowercase()
    } else {
        k
    }
}

/// `CODESEARCH_REPOS_CONFIG` overrides the location, `CODESEARCH_HOME`
/// relocates the global root, else `~/.codesearch/repos.json` — the same
/// chain the rest of the codebase uses.
fn repos_config_file() -> Option<PathBuf> {
    crate::db_discovery::repos::config_path().ok()
}

fn server_opted_in() -> bool {
    std::env::var(SERVER_ENV)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// The `repos` object of the global registry, when readable.
fn load_repos() -> Option<Map<String, Value>> {
    let content = std::fs::read_to_string(repos_config_file()?).ok()?;
    let json: Value = serde_json::from_str(&content).ok()?;
    json.get("repos")?.as_object().cloned()
}

/// Coverage: `CODESEARCH_SERVER` opts in unconditionally; else the root must
/// be registered (string entry) in the registry. Anything unresolvable fails
/// open.
fn target_registered(root: &str) -> bool {
    if server_opted_in() {
        return true;
    }
    let Some(repos) = load_repos() else {
        return false;
    };
    repos
        .values()
        .any(|v| v.as_str().is_some_and(|s| repo_path_equal(s, root)))
}

/// Root path registered for a project alias; None when unresolvable —
/// callers skip the path (the twin returned the empty string for this).
fn registered_project_root(alias: &str) -> Option<String> {
    if alias.is_empty() || server_opted_in() {
        return None;
    }
    load_repos()?
        .get(alias)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Resolve one tool-RESULT path to local absolute candidates: absolute paths
/// pass through; alias-prefixed or repo-relative paths join onto the
/// registered root of the named `project` alias, else the cwd repo's root
/// (both shape candidates — repo-relative vs alias-prefixed — when no alias
/// resolves the join).
fn resolve_result_path(path: &str, project_alias: &str, fallback_root: &str) -> Vec<String> {
    if path.is_empty() {
        return Vec::new();
    }
    if path.starts_with('/') || path.starts_with('\\') || starts_with_drive(path) {
        return vec![path.to_string()];
    }
    if let Some(root) = registered_project_root(project_alias) {
        let rest = path.strip_prefix("./").unwrap_or(path);
        let rest = if rest.starts_with(&format!("{project_alias}/")) {
            &rest[project_alias.len() + 1..]
        } else {
            rest
        };
        return vec![format!("{}/{}", root.trim_end_matches(['/', '\\']), rest)];
    }
    if fallback_root.is_empty() {
        return Vec::new();
    }
    let rest = path.strip_prefix("./").unwrap_or(path);
    let base = fallback_root.trim_end_matches(['/', '\\']);
    if let Some((_, without_first)) = rest.split_once('/') {
        return vec![format!("{base}/{rest}"), format!("{base}/{without_first}")];
    }
    vec![format!("{base}/{rest}")]
}

/// A value is file-path-shaped when it carries a separator anywhere, or ends
/// in a dot + alphanumeric run (`[/\\]|\.[A-Za-z0-9]+$`). A dotted
/// `obj.method()` is NOT shaped — the repo-level marker must survive.
fn has_path_shape(v: &str) -> bool {
    if v.contains('/') || v.contains('\\') {
        return true;
    }
    match v.rsplit_once('.') {
        Some((_, ext)) => !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()),
        None => false,
    }
}

/// Every file-path-shaped `path`/`file`/`file_path` value at any depth,
/// recursing into JSON-stringified text payloads (the MCP content[].text
/// envelope). A named key with a non-shaped string value is dropped, not
/// recursed — same as the twin.
fn collect_result_paths(node: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_paths(node, &mut out);
    out
}

fn collect_paths(node: &Value, out: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            for (k, v) in map {
                if matches!(k.as_str(), "path" | "file" | "file_path")
                    && v.is_string()
                    && !v.as_str().unwrap().is_empty()
                {
                    let s = v.as_str().unwrap();
                    if has_path_shape(s) {
                        out.push(s.to_string());
                    }
                } else {
                    collect_paths(v, out);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_paths(item, out);
            }
        }
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                    collect_paths(&parsed, out);
                }
            }
        }
        _ => {}
    }
}

/// PowerShell-style stringification of a JSON scalar: null -> empty,
/// numbers/bools -> their text, containers -> nothing usable.
fn ps_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn parse_ts(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}

/// State file location: `%TEMP%` on Windows (the ps1 resolution), else
/// `${TMPDIR:-/tmp}` — std's temp_dir is exactly that on unix.
fn state_file_path() -> Option<PathBuf> {
    // Test-only override: TEMP is process-global, and overriding it from a
    // test would race every parallel tempfile::tempdir in the suite.
    #[cfg(test)]
    {
        let guard = TEST_STATE_DIR.lock().unwrap();
        if let Some(dir) = guard.as_ref() {
            return Some(dir.join(STATE_FILE_NAME));
        }
    }
    let dir = if cfg!(windows) {
        match std::env::var("TEMP") {
            Ok(t) if !t.is_empty() => PathBuf::from(t),
            _ => std::env::temp_dir(),
        }
    } else {
        std::env::temp_dir()
    };
    Some(dir.join(STATE_FILE_NAME))
}

/// Per-test marker state directory (see state_file_path).
#[cfg(test)]
static TEST_STATE_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Missing or corrupt state counts as NOT consulted.
fn read_state() -> Map<String, Value> {
    let Some(path) = state_file_path() else {
        return Map::new();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Map::new();
    };
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Atomic write: sibling temp file (pid-suffixed so concurrent hook calls
/// never collide), then rename over the target with the transient-rename
/// retry budget. A failed write just means the marker is missing — fail-open.
fn write_state(state: &Map<String, Value>) {
    let Ok(data) = serde_json::to_string(state) else {
        return;
    };
    let Some(target) = state_file_path() else {
        return;
    };
    let tmp = target.with_extension(format!("json.tmp{}", std::process::id()));
    if std::fs::write(&tmp, &data).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    for attempt in 0..RENAME_RETRY_ATTEMPTS {
        match std::fs::rename(&tmp, &target) {
            Ok(()) => return,
            Err(e) if attempt + 1 < RENAME_RETRY_ATTEMPTS && is_transient_rename_error(&e) => {
                std::thread::sleep(std::time::Duration::from_millis(RENAME_RETRY_DELAY_MS));
            }
            Err(_) => break,
        }
    }
    let _ = std::fs::remove_file(&tmp);
}

fn checked_recently(state: &Map<String, Value>, key: &str, now: i64) -> bool {
    state
        .get(key)
        .and_then(|entry| entry.get("ts"))
        .and_then(parse_ts)
        .is_some_and(|ts| now - ts < WINDOW_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{CODESEARCH_HOME_ENV, REPOS_CONFIG_ENV};
    use crate::testing::EnvRestore;
    use serial_test::serial;
    use std::process::Command;

    /// Temp repos + a fake registry + a private state dir; every env
    /// override is RAII-restored. No network, no serve, no real registry.
    struct Fixture {
        _env_set: EnvRestore,
        _env_removed: EnvRestore,
        _tmp: tempfile::TempDir,
        root_a: PathBuf,
        root_b: PathBuf,
        state: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("repoA");
        let root_b = tmp.path().join("repoB");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        for repo in [&root_a, &root_b] {
            let out = Command::new("git")
                .args(["init", "-q"])
                .current_dir(repo)
                .output()
                .expect("git must be on PATH for the test repos");
            assert!(out.status.success(), "git init failed in {repo:?}");
        }
        for f in [
            "x.cs", "m.ts", "p.py", "z.cs", "w.cs", "v.cs", "u.cs", "s.py", "t.py",
        ] {
            std::fs::write(root_a.join(f), "").unwrap();
        }
        std::fs::write(root_b.join("b.cs"), "").unwrap();
        let repos_json = tmp.path().join("repos.json");
        std::fs::write(
            &repos_json,
            format!(
                r#"{{"repos": {{"repoA": {}}}}}"#,
                serde_json::to_string(&root_a).unwrap()
            ),
        )
        .unwrap();
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state = state_dir.join(STATE_FILE_NAME);
        // TEMP stays untouched (process-global); the marker dir goes through
        // the test-only override instead.
        *TEST_STATE_DIR.lock().unwrap() = Some(state_dir);
        let repos_str = repos_json.display().to_string();
        let env_set = EnvRestore::set(&[(REPOS_CONFIG_ENV, repos_str.as_str())]);
        let env_removed = EnvRestore::remove(&[SERVER_ENV, CODESEARCH_HOME_ENV]);
        Fixture {
            _env_set: env_set,
            _env_removed: env_removed,
            _tmp: tmp,
            root_a,
            root_b,
            state,
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            TEST_STATE_DIR.lock().unwrap().take();
        }
    }

    impl Fixture {
        fn fresh(&self) {
            let _ = std::fs::remove_file(&self.state);
        }
        fn state_json(&self) -> Option<Value> {
            std::fs::read_to_string(&self.state)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
        }
        fn age_state(&self, seconds_ago: i64) {
            let mut v = self.state_json().expect("state must exist to be aged");
            let now = unix_now();
            if let Some(map) = v.as_object_mut() {
                for entry in map.values_mut() {
                    entry["ts"] = json!(now - seconds_ago);
                }
            }
            std::fs::write(&self.state, v.to_string()).unwrap();
        }
    }

    fn edit_event(path: &Path) -> String {
        json!({ "tool_name": "Edit", "tool_input": { "file_path": path.display().to_string() } })
            .to_string()
    }

    fn multiedit_event(a: &Path, b: &Path) -> String {
        json!({
            "tool_name": "MultiEdit",
            "tool_input": {
                "file_path": a.display().to_string(),
                "edits": [
                    { "file_path": a.display().to_string() },
                    { "file_path": b.display().to_string() }
                ]
            }
        })
        .to_string()
    }

    fn post_find_impact(file: &Path) -> String {
        json!({
            "tool_name": "mcp__codesearch__find_impact",
            "tool_input": { "file": file.display().to_string() }
        })
        .to_string()
    }

    fn post_find(path: &Path, kind: &str) -> String {
        json!({
            "tool_name": "mcp__codesearch__find",
            "tool_input": { "path": path.display().to_string(), "kind": kind }
        })
        .to_string()
    }

    fn mcp_text_response(text: &str) -> Value {
        json!({ "content": [ { "type": "text", "text": text } ] })
    }

    fn allow(_fx: &Fixture, raw: &str, label: &str) {
        let out = edit_guard(raw);
        assert!(out.is_empty(), "{label}: expected silent allow, got {out}");
    }

    fn deny(_fx: &Fixture, raw: &str, label: &str, contains: &str) {
        let out = edit_guard(raw);
        let v: Value = serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("{label}: expected deny JSON, got {out:?} ({e})"));
        assert_eq!(
            v["hookSpecificOutput"]["permissionDecision"], "deny",
            "{label}"
        );
        let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap_or("");
        assert!(
            reason.contains(contains),
            "{label}: deny message missing {contains:?} — got {reason:?}"
        );
    }

    fn no_junk_key(fx: &Fixture, needle: &str, label: &str) {
        if let Some(state) = fx.state_json() {
            assert!(
                !state.to_string().contains(needle),
                "{label}: junk key {needle:?} written into state"
            );
        }
    }

    // --- edit-guard cases (ported from run-tests.sh) ---

    #[serial]
    #[test]
    fn unregistered_repo_allows() {
        let fx = fixture();
        allow(
            &fx,
            &edit_event(&fx.root_b.join("b.cs")),
            "unregistered repo",
        );
    }

    #[serial]
    #[test]
    fn covered_cs_denied_needs_find_impact() {
        let fx = fixture();
        deny(
            &fx,
            &edit_event(&fx.root_a.join("x.cs")),
            "covered .cs",
            "find_impact",
        );
    }

    #[serial]
    #[test]
    fn covered_ts_denied_needs_find_impact() {
        let fx = fixture();
        deny(
            &fx,
            &edit_event(&fx.root_a.join("m.ts")),
            "covered .ts",
            "find_impact",
        );
    }

    #[serial]
    #[test]
    fn covered_py_needs_usages() {
        let fx = fixture();
        deny(
            &fx,
            &edit_event(&fx.root_a.join("p.py")),
            "covered .py",
            "kind=\"usages\"",
        );
    }

    #[serial]
    #[test]
    fn non_code_files_allow_without_consultation() {
        let fx = fixture();
        allow(
            &fx,
            &edit_event(&fx.root_a.join("docs/notes.md")),
            "covered .md",
        );
        allow(
            &fx,
            &edit_event(&fx.root_a.join("config/settings.json")),
            "covered .json",
        );
    }

    #[serial]
    #[test]
    fn razor_sql_ipynb_need_usages() {
        let fx = fixture();
        for f in [
            "Components/Counter.razor",
            "migrations/001_init.sql",
            "analysis/notebook.ipynb",
        ] {
            deny(&fx, &edit_event(&fx.root_a.join(f)), f, "kind=\"usages\"");
        }
    }

    #[serial]
    #[test]
    fn definition_kind_does_not_mark() {
        let fx = fixture();
        edit_guard_post(&post_find(&fx.root_a.join("m.ts"), "definition"));
        deny(
            &fx,
            &edit_event(&fx.root_a.join("m.ts")),
            "definition does not mark",
            "find_impact",
        );
    }

    #[serial]
    #[test]
    fn find_impact_marker_allows() {
        let fx = fixture();
        edit_guard_post(&post_find_impact(&fx.root_a.join("x.cs")));
        allow(&fx, &edit_event(&fx.root_a.join("x.cs")), "marked x.cs");
    }

    #[serial]
    #[test]
    fn empty_first_field_falls_through() {
        let fx = fixture();
        let raw = json!({
            "tool_name": "mcp__codesearch__find_impact",
            "tool_input": { "file": "", "path": fx.root_a.join("m.ts").display().to_string() }
        })
        .to_string();
        edit_guard_post(&raw);
        allow(
            &fx,
            &edit_event(&fx.root_a.join("m.ts")),
            "empty file field",
        );
    }

    #[serial]
    #[test]
    fn multiedit_partial_marks_denied() {
        let fx = fixture();
        // Mark the .cs first (as earlier cases in run-tests.sh do), so the
        // denial names the unmarked .py — MultiEdit needs EVERY path marked.
        edit_guard_post(&post_find_impact(&fx.root_a.join("x.cs")));
        deny(
            &fx,
            &multiedit_event(&fx.root_a.join("x.cs"), &fx.root_a.join("p.py")),
            "multiedit partial marks",
            &fx.root_a.join("p.py").display().to_string(),
        );
    }

    #[serial]
    #[test]
    fn find_usages_marker_allows_multiedit_when_all_marked() {
        let fx = fixture();
        edit_guard_post(&post_find(&fx.root_a.join("p.py"), "usages"));
        allow(&fx, &edit_event(&fx.root_a.join("p.py")), "marked p.py");
        edit_guard_post(&post_find_impact(&fx.root_a.join("x.cs")));
        allow(
            &fx,
            &multiedit_event(&fx.root_a.join("x.cs"), &fx.root_a.join("p.py")),
            "multiedit with every path marked",
        );
    }

    // --- edit-guard-post cases (ported from run-tests.sh) ---

    #[serial]
    #[test]
    fn response_paths_mark_via_mcp_text_envelope() {
        let fx = fixture();
        let hits = json!({ "results": [ { "path": "repoA/z.cs", "chunk_id": 7 } ] }).to_string();
        let raw = json!({
            "tool_name": "mcp__codesearch__find_impact",
            "tool_input": { "symbol_name": "CloseAsync", "project": "repoA" },
            "tool_response": mcp_text_response(&hits)
        })
        .to_string();
        edit_guard_post(&raw);
        allow(
            &fx,
            &edit_event(&fx.root_a.join("z.cs")),
            "envelope hit paths",
        );
    }

    #[serial]
    #[test]
    fn string_tool_response_parses_the_same_payload() {
        let fx = fixture();
        let hits = json!({ "results": [ { "path": "repoA/v.cs", "chunk_id": 9 } ] }).to_string();
        let raw = json!({
            "tool_name": "mcp__codesearch__find_impact",
            "tool_input": { "project": "repoA", "symbol_name": "CloseAsync" },
            "tool_response": hits
        })
        .to_string();
        edit_guard_post(&raw);
        allow(&fx, &edit_event(&fx.root_a.join("v.cs")), "string response");
    }

    #[serial]
    #[test]
    fn symbolic_without_project_still_denied() {
        let fx = fixture();
        fx.fresh();
        let raw = json!({
            "tool_name": "mcp__codesearch__find_impact",
            "tool_input": { "symbol_name": "CloseAsync" },
            "tool_response": mcp_text_response("No symbol index available for csharp")
        })
        .to_string();
        edit_guard_post(&raw);
        deny(
            &fx,
            &edit_event(&fx.root_a.join("w.cs")),
            "no project, no cwd: nothing attributable",
            "find_impact",
        );
    }

    #[serial]
    #[test]
    fn no_backend_answer_marks_repo() {
        let fx = fixture();
        let raw = json!({
            "tool_name": "mcp__codesearch__find_impact",
            "tool_input": { "symbol_name": "CloseAsync", "project": "repoA" },
            "tool_response": mcp_text_response("No symbol index available for csharp")
        })
        .to_string();
        edit_guard_post(&raw);
        allow(&fx, &edit_event(&fx.root_a.join("p.py")), "repo marker");
    }

    #[serial]
    #[test]
    fn path_symbol_marks_file() {
        let fx = fixture();
        let raw = json!({
            "tool_name": "mcp__codesearch__find",
            "tool_input": { "symbol": "s.py", "kind": "usages", "project": "repoA" },
            "tool_response": mcp_text_response(
                "No usages found for 's.py'. The symbol may not be indexed."
            )
        })
        .to_string();
        edit_guard_post(&raw);
        allow(
            &fx,
            &edit_event(&fx.root_a.join("s.py")),
            "path-level marker",
        );
    }

    #[serial]
    #[test]
    fn dotted_symbol_marks_repo_without_junk_key() {
        let fx = fixture();
        fx.fresh();
        let raw = json!({
            "tool_name": "mcp__codesearch__find",
            "tool_input": { "symbol": "obj.method()", "kind": "usages", "project": "repoA" },
            "tool_response": mcp_text_response(
                "No usages found for 'obj.method()'. The symbol may not be indexed."
            )
        })
        .to_string();
        edit_guard_post(&raw);
        allow(
            &fx,
            &edit_event(&fx.root_a.join("p.py")),
            "repo fallback fires",
        );
        no_junk_key(&fx, "obj.method()", "dotted symbol");
    }

    #[serial]
    #[test]
    fn cwd_scoped_response_paths_mark_the_cwd_repo() {
        let fx = fixture();
        fx.fresh();
        let raw = json!({
            "tool_name": "mcp__codesearch__find",
            "tool_input": { "symbol": "git_repo", "kind": "usages", "cwd": fx.root_a.display().to_string() },
            "tool_response": { "results": [ { "path": "repoA/t.py", "chunk_id": 3 } ] }
        })
        .to_string();
        edit_guard_post(&raw);
        allow(&fx, &edit_event(&fx.root_a.join("t.py")), "cwd-scoped hits");
    }

    #[serial]
    #[test]
    fn cwd_junk_path_marks_repo_only() {
        let fx = fixture();
        fx.fresh();
        let raw = json!({
            "tool_name": "mcp__codesearch__find",
            "tool_input": { "symbol": "git_repo", "kind": "usages", "cwd": fx.root_a.display().to_string() },
            "tool_response": mcp_text_response(
                "No usages found for 'git_repo'. The symbol may not be indexed."
            )
        })
        .to_string();
        edit_guard_post(&raw);
        allow(
            &fx,
            &edit_event(&fx.root_a.join("w.cs")),
            "repo marker unblocks",
        );
        allow(
            &fx,
            &edit_event(&fx.root_a.join("p.py")),
            "cwd repo-root marker",
        );
        no_junk_key(&fx, "git_repo", "bare symbol response");
    }

    #[serial]
    #[test]
    fn window_expiry_denies_again() {
        let fx = fixture();
        edit_guard_post(&post_find_impact(&fx.root_a.join("x.cs")));
        allow(&fx, &edit_event(&fx.root_a.join("x.cs")), "fresh marker");
        fx.age_state(400);
        deny(
            &fx,
            &edit_event(&fx.root_a.join("x.cs")),
            "expired marker",
            "find_impact",
        );
    }

    #[serial]
    #[test]
    fn impact_symbol_name_path_marks_file() {
        // THE theme-2-merge wedge: find_impact's target rides in
        // `symbol_name` and its answer lists CALLERS — the consulted file
        // need not appear in the response at all.
        let fx = fixture();
        fx.fresh();
        deny(
            &fx,
            &edit_event(&fx.root_a.join("u.cs")),
            "isolation: fresh state",
            "find_impact",
        );
        let hits = json!({ "results": [ { "path": "repoA/p.py", "chunk_id": 11 } ] }).to_string();
        let raw = json!({
            "tool_name": "mcp__codesearch__find_impact",
            "tool_input": {
                "project": "repoA",
                "symbol_name": fx.root_a.join("u.cs").display().to_string()
            },
            "tool_response": mcp_text_response(&hits)
        })
        .to_string();
        edit_guard_post(&raw);
        allow(
            &fx,
            &edit_event(&fx.root_a.join("u.cs")),
            "symbol_name=<path> marks the consulted file",
        );
    }

    #[serial]
    #[test]
    fn impact_symbol_name_cwd_pathless_marks_file() {
        // Serve-mode shape: cwd scoping (no project) and a pathless answer.
        let fx = fixture();
        fx.fresh();
        let raw = json!({
            "tool_name": "mcp__codesearch__find_impact",
            "tool_input": {
                "cwd": fx.root_a.display().to_string(),
                "symbol_name": fx.root_a.join("x.cs").display().to_string()
            },
            "tool_response": mcp_text_response("No symbol index available for csharp")
        })
        .to_string();
        edit_guard_post(&raw);
        allow(
            &fx,
            &edit_event(&fx.root_a.join("x.cs")),
            "file + repo marker",
        );
    }

    #[serial]
    #[test]
    fn unknown_alias_cwd_fallback_marks_repo() {
        let fx = fixture();
        fx.fresh();
        let raw = json!({
            "tool_name": "mcp__codesearch__find_impact",
            "tool_input": {
                "project": "nosuch",
                "symbol_name": "ActivityClient",
                "cwd": fx.root_a.display().to_string()
            },
            "tool_response": mcp_text_response(
                "No usages found for 'ActivityClient'. The symbol may not be indexed."
            )
        })
        .to_string();
        edit_guard_post(&raw);
        allow(&fx, &edit_event(&fx.root_a.join("w.cs")), "cwd repo marker");
        no_junk_key(&fx, "ActivityClient", "unknown alias");
    }

    #[serial]
    #[test]
    fn codesearch_home_registry_is_honoured() {
        let fx = fixture();
        let home = fx.state.parent().unwrap().join("cs-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("repos.json"),
            format!(
                r#"{{"repos": {{"repoA": {}}}}}"#,
                serde_json::to_string(&fx.root_a).unwrap()
            ),
        )
        .unwrap();
        let home_str = home.display().to_string();
        let drop_cfg = EnvRestore::remove(&[REPOS_CONFIG_ENV]);
        let drop_home = EnvRestore::set(&[(CODESEARCH_HOME_ENV, home_str.as_str())]);
        deny(
            &fx,
            &edit_event(&fx.root_a.join("x.cs")),
            "CODESEARCH_HOME registry: covered .cs",
            "find_impact",
        );
        allow(
            &fx,
            &edit_event(&fx.root_b.join("b.cs")),
            "CODESEARCH_HOME registry: unregistered fails open",
        );
        drop(drop_home);
        drop(drop_cfg);
    }

    #[serial]
    #[test]
    fn state_marker_shape_is_normalized() {
        let fx = fixture();
        edit_guard_post(&post_find_impact(&fx.root_a.join("x.cs")));
        let state = fx.state_json().expect("state written");
        let key = norm_state_key(&fx.root_a.join("x.cs").display().to_string());
        let entry = state.get(&key).expect("normalized file key present");
        assert_eq!(entry["tool"], "find_impact");
        assert!(entry["ts"].is_i64());
    }

    #[serial]
    #[test]
    fn multiedit_duplicate_paths_dedup() {
        // file_path + edits[] naming the same file must not matter.
        let fx = fixture();
        edit_guard_post(&post_find_impact(&fx.root_a.join("x.cs")));
        let raw = json!({
            "tool_name": "MultiEdit",
            "tool_input": {
                "file_path": fx.root_a.join("x.cs").display().to_string(),
                "edits": [ { "file_path": fx.root_a.join("x.cs").display().to_string() } ]
            }
        })
        .to_string();
        allow(&fx, &raw, "all-duplicate paths marked");
    }
}
