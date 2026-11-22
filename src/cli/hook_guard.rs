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
        // A closed stdout pipe (EPIPE) must not turn "always exit 0" into a
        // panic — the write result is ignored and the hook still exits 0.
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout().lock(), "{out}");
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
    hook_temp_file(STATE_FILE_NAME)
}

/// A guard's scratch file under the shared (test-overridable) temp dir.
fn hook_temp_file(name: &str) -> Option<PathBuf> {
    // Test-only override: TEMP is process-global, and overriding it from a
    // test would race every parallel tempfile::tempdir in the suite.
    #[cfg(test)]
    {
        let guard = TEST_STATE_DIR.lock().unwrap();
        if let Some(dir) = guard.as_ref() {
            return Some(dir.join(name));
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
    Some(dir.join(name))
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
    write_json_atomic(&target, &data);
}

fn write_json_atomic(target: &Path, data: &str) {
    let tmp = target.with_extension(format!("json.tmp{}", std::process::id()));
    if std::fs::write(&tmp, data).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    for attempt in 0..RENAME_RETRY_ATTEMPTS {
        match std::fs::rename(&tmp, target) {
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

// ---
// grep-guard (`codesearch hook grep-guard`) — port of grep-guard.{sh,ps1}
// ---

/// The Bash-tool escape hatch: these tokens invoke the grep family (whole
/// tokens, `.exe` and path prefixes stripped, quoted spans removed first).
const GREP_FAMILY: &[&str] = &["grep", "egrep", "fgrep", "zgrep", "rg"];
/// In-flight-rebuild wait cap, parity with the opencode grep-guard plugin
/// (0 = legacy instant WAIT deny; the twins use the bare env name).
const GREP_WAIT_ENV: &str = "CODESEARCH_GREP_GUARD_WAIT_MS";
const DEFAULT_WAIT_CAP_MS: u64 = 120_000;
/// Bearer key for the /indexing probe on a network-bound serve (the twins
/// use the bare name; distinct from the serve's management key).
const API_KEY_ENV: &str = "CODESEARCH_API_KEY";
/// Liveness probe answer time — keeps grep latency low; /healthz is instant.
const PROBE_TIMEOUT_SECS: u64 = 2;
/// Poll interval while waiting out a rebuild, capped by the remaining time.
const WAIT_POLL_MS: u64 = 3000;

/// `codesearch hook grep-guard` — PreToolUse guard over `Grep` and the Bash
/// grep family. Async arms: the liveness/freshness probes are HTTP. A panic
/// anywhere in the decision path degrades to a silent allow via the spawned
/// task's JoinError — the hook always exits 0.
pub async fn run_grep_guard() {
    let raw = read_stdin();
    let out = tokio::spawn(async move { grep_guard(&raw).await })
        .await
        .unwrap_or_default(); // panic in the decision path -> allow
    emit_stdout(&out);
}

fn emit_stdout(out: &str) {
    if out.is_empty() {
        return;
    }
    // A closed stdout pipe (EPIPE) must not turn "always exit 0" into a
    // panic — the write result is ignored and the hook still exits 0.
    use std::io::Write as _;
    let _ = writeln!(std::io::stdout().lock(), "{out}");
}

/// The grep-guard decision for one event, or empty for a silent allow.
async fn grep_guard(raw: &str) -> String {
    let Ok(data) = serde_json::from_str::<Value>(raw.trim()) else {
        return String::new();
    };
    let tool = data.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let inp = data.get("tool_input").unwrap_or(&Value::Null);

    // Target resolution: a Bash grep follows its OWN operands (absolute,
    // relative, or a leading `cd DIR &&`); with no operand it walks the cwd,
    // and the cwd repo is the target. A Grep call follows `path`.
    let target_root = match tool {
        "Bash" => {
            let cmd = inp.get("command").map(ps_string).unwrap_or_default();
            if !is_grep_family_command(&cmd) {
                return String::new();
            }
            let targets = grep_target_paths(&cmd);
            let mut root = None;
            for candidate in &targets {
                if let Some(r) = resolve_target_git_root(candidate) {
                    if target_registered(&r) {
                        root = Some(r);
                        break;
                    }
                }
            }
            // Every operand is outside every indexed repo: not an indexed search.
            match root {
                Some(r) => r,
                None if targets.is_empty() => resolve_target_git_root("").unwrap_or_default(),
                None => return String::new(),
            }
        }
        "Grep" => {
            let path = inp.get("path").map(ps_string).unwrap_or_default();
            resolve_target_git_root(&path).unwrap_or_default()
        }
        _ => return String::new(),
    };
    if target_root.is_empty() {
        return String::new(); // external target: grep is the right tool
    }
    if !target_registered(&target_root) {
        return String::new(); // unregistered repo -> fail-open
    }

    let base = codesearch_base_url();
    // codesearch DOWN -> grep is genuinely all you have, let it through. An
    // HTTP error RESPONSE still proves the server is reachable: only a
    // connection-level failure opens the escape hatch.
    if !probe_liveness(&base).await {
        return String::new();
    }

    let wait_cap_ms = std::env::var(GREP_WAIT_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_WAIT_CAP_MS);

    // Freshness probe (#55): liveness says UP, not FRESH. Mid-reindex ->
    // wait out the rebuild; past the cap grep opens. An older serve without
    // the endpoint answers 404: probe skipped, standard deny below.
    if let Some(indexing) = probe_indexing(&base, &target_root).await {
        if indexing {
            if wait_cap_ms == 0 {
                return deny_json(WAIT_MESSAGE);
            }
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(wait_cap_ms);
            loop {
                let rest = deadline.saturating_duration_since(std::time::Instant::now());
                if rest.is_zero() {
                    return String::new(); // still rebuilding at the cap: grep is all you have
                }
                tokio::time::sleep(rest.min(std::time::Duration::from_millis(WAIT_POLL_MS))).await;
                match probe_indexing(&base, &target_root).await {
                    Some(false) => return deny_json(&settled_message(wait_cap_ms)),
                    Some(true) => continue,
                    // transient probe failure mid-wait: keep polling
                    None => continue,
                }
            }
        }
    }

    deny_json(&grep_deny_message())
}

/// Uniform PreToolUse deny decision.
fn deny_json(reason: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason
        }
    })
    .to_string()
}

const WAIT_MESSAGE: &str =
    "codesearch is LIVE, but THIS repo's index is being rebuilt right now (a\n\
branch switch or file change fired the serve watcher's full refresh).\n\
Searching immediately would return stale or empty results — that is the\n\
rebuild in progress, not a miss, and NOT a reason to grep.\n\n\
WAIT 15-30 seconds (Bash: sleep 20), then RETRY your codesearch call — it\n\
will answer normally once the rebuild lands. The working tree did not\n\
change; only the index is catching up, so grep adds nothing here.\n\n\
If the rebuild still has not landed after ~2 minutes, run your codesearch\n\
call anyway (a partially fresh index still beats grep) or ask the user how\n\
to proceed.";

fn settled_message(wait_cap_ms: u64) -> String {
    format!(
        "codesearch rebuild on this repo landed while the guard waited\n\
(CODESEARCH_GREP_GUARD_WAIT_MS={wait_cap_ms}). Retry your codesearch call\n\
now — it will answer normally. Do NOT grep."
    )
}

fn grep_deny_message() -> String {
    "codesearch is LIVE for this repo (its /healthz probe just answered) — use it,\n\
do NOT fall back to Grep, and do NOT work around this via a shell grep/rg in\n\
the Bash tool: the same guard denies that too. Grep on an indexed internal\n\
path is only auto-allowed when the hub is DOWN or this repo's index is stuck\n\
mid-rebuild past the guard's wait cap — neither is the case right now.\n\n\
IMPORTANT: a low-confidence or EMPTY codesearch result is a SUCCESSFUL call that\n\
means \"reformulate your query\" — it does NOT mean codesearch is down and it will\n\
NOT unblock Grep. Reformulate instead of grepping.\n\n\
Step 1 — load the deferred MCP tool schemas (Claude Code defers all MCP tools;\n\
this is a one-time step per conversation):\n\
  ToolSearch(\"select:mcp__codesearch__search,mcp__codesearch__find,mcp__codesearch__explore,mcp__codesearch__get_chunk\")\n\n\
Step 2 — pick the RIGHT tool (this is usually why a query came back empty):\n\
  find(symbol=\"Name\", kind=\"definition\")   -- known symbol / type / function definition\n\
  find(symbol=\"Name\", kind=\"usages\")       -- all call sites of a known symbol\n\
  explore(kind=\"outline\", target=\"path\")   -- every symbol in one file\n\
  search(query=\"concept\", mode=\"semantic\") -- concepts / cross-file, the DEFAULT\n\
  search(query=\"exact\", mode=\"literal\", regex=true) -- exact syntax / pattern\n\n\
Query hygiene (this is what produces \"low_confidence: []\"):\n\
  * Do NOT paste grep-style multi-term alternations (\"a|b|c\", \"::\", \"fn foo(\")\n\
    into search — BM25 tokenises on punctuation and the match scores below the\n\
    relevance floor, so you get an empty result even though the string exists.\n\
  * Use ONE clean term, or switch to find()/explore() for exact symbols.\n\n\
Multi-repo serve mode: if the call returns a \"scope_required\" or \"Unknown alias\"\n\
error, you MUST pass project=\"<repo-alias>\" (single repo) or group=\"<group>\"\n\
(cross-repo). The error response LISTS the valid available_projects /\n\
available_groups — pick from that list (the alias may differ from the folder\n\
name).\n\n\
Grep is always allowed for targets outside any git repo, and for repos\n\
codesearch does not cover (no local index, no CODESEARCH_SERVER)."
    .to_string()
}

/// Base URL, mirroring codesearch src/constants.rs: a full `CODESEARCH_SERVER`
/// URL wins, then `CODESEARCH_SERVE_PORT` on the default local host, then the
/// default serve port.
fn codesearch_base_url() -> String {
    use crate::constants::{DEFAULT_SERVE_HOST, DEFAULT_SERVE_PORT, SERVE_PORT_ENV};
    // CODESEARCH_SERVER doubles as the coverage opt-in AND the base URL.
    let url = std::env::var(SERVER_ENV).unwrap_or_default();
    if !url.trim().is_empty() {
        return url.trim_end_matches('/').to_string();
    }
    let port = std::env::var(SERVE_PORT_ENV)
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(DEFAULT_SERVE_PORT);
    format!("http://{DEFAULT_SERVE_HOST}:{port}")
}

/// GET /healthz: any HTTP response (4xx/5xx included) proves the server is
/// up; only a connection-level failure means genuinely down.
async fn probe_liveness(base: &str) -> bool {
    let url = format!("{base}/healthz");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(PROBE_TIMEOUT_SECS))
        .build();
    match client {
        Ok(c) => c.get(&url).send().await.is_ok(),
        Err(_) => false,
    }
}

/// GET /indexing?path=<root>: Some(fresh) when a newer serve answered with
/// parseable JSON (fresh = covered && indexing), None on probe failure —
/// the callers deny (first probe) or keep polling (mid-wait) either way.
async fn probe_indexing(base: &str, root: &str) -> Option<bool> {
    let url = reqwest::Url::parse_with_params(
        &format!("{base}/indexing"),
        &[("path", root.replace('\\', "/"))],
    )
    .ok()?;
    let mut req = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(PROBE_TIMEOUT_SECS))
        .build()
        .ok()?
        .get(url);
    if let Ok(key) = std::env::var(API_KEY_ENV) {
        if !key.is_empty() {
            req = req.header("Authorization", format!("Bearer {key}"));
        }
    }
    let body = req.send().await.ok()?.text().await.ok()?;
    let fresh: Value = serde_json::from_str(body.trim()).ok()?;
    Some(
        fresh
            .get("covered")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && fresh
                .get("indexing")
                .and_then(Value::as_bool)
                .unwrap_or(false),
    )
}

/// True when a shell command line invokes the grep family — the Bash tool
/// escape hatch around the Grep-tool guard. Quoted spans are stripped first
/// ("echo 'grep is slow'" is not an invocation), separators split, and
/// tokens compare whole after lowercasing, `.exe` and path-prefix removal
/// ("progress" and "rgx" never match; /usr/bin/grep does).
fn is_grep_family_command(cmd: &str) -> bool {
    if cmd.trim().is_empty() {
        return false;
    }
    let stripped = strip_quoted_spans(cmd);
    for token in stripped.split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '`')) {
        if token.is_empty() {
            continue;
        }
        if GREP_FAMILY.contains(&normalize_token(token).as_str()) {
            return true;
        }
    }
    false
}

/// Remove quoted spans (`"…"` / `'…'` with `\"`-style escapes), keeping the
/// span's argument position as a placeholder so the pattern/operand split
/// stays correct for the target-path extractor.
fn strip_quoted_spans(cmd: &str) -> String {
    strip_quoted_char(&strip_quoted_char(cmd, '"'), '\'')
}

fn strip_quoted_char(s: &str, quote: char) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == quote {
            out.push(' ');
            let mut escaped = false;
            for inner in chars.by_ref() {
                if escaped {
                    escaped = false;
                } else if inner == '\\' {
                    escaped = true;
                } else if inner == quote {
                    break;
                }
            }
            out.push_str("__CS_Q__");
        } else {
            out.push(c);
        }
    }
    out
}

/// Whole-token compare: lowercase, strip a trailing `.exe`, strip any path
/// prefix (`/usr/bin/grep` and `C:/tools/grep.exe` are plain `grep`).
fn normalize_token(token: &str) -> String {
    let lower = token.to_lowercase();
    let no_ext = lower.strip_suffix(".exe").unwrap_or(&lower);
    match no_ext.rfind(['/', '\\']) {
        Some(idx) => no_ext[idx + 1..].to_string(),
        None => no_ext.to_string(),
    }
}

/// Absolute paths a Bash grep will read files from, mirroring
/// command_grep_targets: coverage follows the command's OWN operands
/// (absolute, relative to `base`, or a leading `cd DIR &&`), never the
/// session cwd. Empty when there is no path operand at all (grep then walks
/// the cwd). `base` is the shell's working directory.
fn grep_target_paths(cmd: &str) -> Vec<String> {
    let base = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    grep_target_paths_with_base(cmd, &base)
}

fn grep_target_paths_with_base(cmd: &str, cwd: &str) -> Vec<String> {
    let mut out = Vec::new();
    if cmd.trim().is_empty() {
        return out;
    }
    let mut base = cwd.to_string();
    // A leading `cd DIR &&` moves the grep's relative base.
    if let Some(rest) = cmd.trim_start().strip_prefix("cd ") {
        let dest: String = rest
            .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|'))
            .next()
            .unwrap_or("")
            .trim_matches(|c| c == '"' || c == '\'')
            .to_string();
        if !dest.is_empty() && dest != "-" {
            base = dest;
        }
    }
    if !looks_absolute(&base) && !base.starts_with('~') {
        base = format!("{}/{}", cwd.trim_end_matches('/'), base);
    }
    // Quoted spans first (a pattern like "^./.git/" is not a path), then one
    // pipeline SEGMENT per element so the pattern/operand split is per grep.
    // Redirections are not operands: drop fd duplication (`2>&1`), then an
    // operator with its target (attached or spaced).
    let stripped = strip_quoted_spans(cmd);
    let stripped = drop_redirections(&stripped);
    for seg in stripped.split([';', '|', '&', '`']) {
        let words: Vec<&str> = seg.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let gidx = words
            .iter()
            .position(|w| GREP_FAMILY.contains(&normalize_token(w).as_str()));
        let Some(gidx) = gidx else { continue };
        let mut pattern_seen = false;
        for tok in words.iter().skip(gidx + 1) {
            if tok.is_empty() || tok.starts_with('-') {
                continue;
            }
            // The first bare word is grep's PATTERN; the rest are paths.
            if !pattern_seen {
                pattern_seen = true;
                continue;
            }
            let p = if looks_absolute(tok) {
                tok.to_string()
            } else if let Some(rest) = tok.strip_prefix('~') {
                // sh twin resolves HOME, the ps1 twin $env:USERPROFILE —
                // honour both so `~/repo` operands resolve on every platform.
                let home = std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap_or_default();
                if home.is_empty() {
                    continue;
                }
                format!("{home}{rest}")
            } else if *tok == "." {
                base.clone()
            } else {
                format!(
                    "{}/{}",
                    base.trim_end_matches('/'),
                    tok.strip_prefix("./").unwrap_or(tok)
                )
            };
            let p = if p.len() > 1 {
                p.trim_end_matches('/').to_string()
            } else {
                p
            };
            if !p.is_empty() {
                out.push(p);
            }
        }
    }
    out
}

/// Drop redirections from a command line: fd duplication (`2>&1`), then an
/// `>`/`>>`/`<` operator with its target (attached or spaced) — the same
/// three replaces the ps1 twin makes, so `grep foo > out.txt` keeps no
/// phantom operand.
fn drop_redirections(s: &str) -> String {
    let dup = regex::Regex::new(r"[0-9]*>&[0-9-]+").expect("static regex");
    let to = regex::Regex::new(r"[0-9]*>{1,2}\s*[^\s;|&]+").expect("static regex");
    let from = regex::Regex::new(r"[0-9]*<\s*[^\s;|&]+").expect("static regex");
    let s = dup.replace_all(s, " ");
    let s = to.replace_all(&s, " ");
    from.replace_all(&s, " ").into_owned()
}

// ---
// web-guard (`codesearch hook web-guard`) — port of web-guard.{sh,ps1}
// ---

/// Web-steer retry cache, shared with the script twins: `$TEMP` (Windows) or
/// `${TMPDIR:-/tmp}` plus this name; `{ "<tool>|<mount>": <unix secs> }`.
const WEB_CACHE_FILE_NAME: &str = ".codesearch-web-guard.json";
const WEB_CACHE_TTL_SECS: i64 = 300;

/// `codesearch hook web-guard` — PreToolUse guard steering WebSearch/WebFetch
/// toward the codesearch remote doc mounts before the open web. One steer per
/// mount per 5-minute window; the follow-up web query is allowed.
pub fn run_web_guard() {
    let raw = read_stdin();
    let out = std::panic::catch_unwind(|| web_guard(&raw)).unwrap_or_default();
    emit_stdout(&out);
}

/// The web-guard decision for one event, or empty for a silent allow.
fn web_guard(raw: &str) -> String {
    let Ok(data) = serde_json::from_str::<Value>(raw.trim()) else {
        return String::new();
    };
    let tool = data.get("tool_name").and_then(Value::as_str).unwrap_or("");
    if tool != "WebSearch" && tool != "WebFetch" {
        return String::new();
    }
    let inp = data.get("tool_input").unwrap_or(&Value::Null);
    // Query (WebSearch) or target URL (WebFetch) — used for matching, the
    // cache key and the guidance text.
    let q = inp
        .get("query")
        .or_else(|| inp.get("url"))
        .map(ps_string)
        .unwrap_or_default();
    if q.trim().is_empty() {
        return String::new();
    }

    // 1. Remote doc mounts live in repos.json under `.remote_mounts`
    //    (canonical "<peer>/<alias>" names — the opt-in allowlist). No
    //    mounts -> nothing to prefer -> allow. Unreadable config never
    //    gets in the way.
    let Some(cfg) = load_hook_config() else {
        return String::new();
    };
    let mounts = remote_mounts(&cfg);
    if mounts.is_empty() {
        return String::new();
    }

    // 2. Is this query actually ABOUT one of the mounted products? Keywords
    //    per mount: the alias itself, plus extra terms declared in
    //    `.remote_mount_topics` (keyed by the full "<peer>/<alias>" or the
    //    bare alias). No match -> not the mirror's subject -> allow.
    let haystack = q.to_lowercase();
    let matched: Vec<&String> = mounts
        .iter()
        .filter(|mount| {
            let alias = mount.rsplit('/').next().unwrap_or(mount);
            if keyword_match(&haystack, alias) {
                return true;
            }
            mount_topics(&cfg, mount, alias)
                .iter()
                .any(|topic| keyword_match(&haystack, topic))
        })
        .collect();
    if matched.is_empty() {
        return String::new();
    }
    let relevant = matched
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let primary = matched[0].clone();

    // 3. Retry cache: same mount steered for recently -> let it through.
    //    Keyed on tool + mount, NOT the raw query: one steer per vendor per
    //    window, so refined follow-up queries are never re-blocked.
    let cache_key = format!("{tool}|{primary}");
    let now = unix_now();
    let mut cache = read_web_cache(now);
    if cache.contains_key(&cache_key) {
        return String::new();
    }
    cache.insert(cache_key, json!(now));
    write_web_cache(&cache);

    // 4. Block with actionable guidance — naming ONLY the mounts that matched.
    deny_json(&web_deny_message(&relevant, &q, &primary, tool))
}

fn web_deny_message(relevant: &str, q: &str, primary: &str, tool: &str) -> String {
    format!(
        "This looks like a question about a product whose documentation codesearch has\n\
indexed — search that mirror before the web.\n\
Relevant mount(s): {relevant}\n\n\
These indexed mounts often answer product/API/docs questions more precisely\n\
(and more currently) than a web search, and they cover vendor sites that need a\n\
login and would fail an anonymous fetch anyway.\n\n\
Step 1 — load the deferred MCP tool schemas (one-time per conversation):\n\
  ToolSearch(\"select:mcp__codesearch__search,mcp__codesearch__get_chunk\")\n\n\
Step 2 — search the relevant mount (compact=false reads matching content inline):\n\
  mcp__codesearch__search(query=\"{q}\", project=\"{primary}\", compact=false)\n\
  mcp__codesearch__get_chunk(chunk_ref=\"{primary}:<id from a result>\")  # full context\n\n\
For a canonical source link, read the doc's front-matter chunk (start_line 0)\n\
and cite its ``url:`` field verbatim rather than reconstructing a URL.\n\n\
If the mount does NOT have the answer, go straight to the web: any further\n\
{tool} call about '{primary}' is allowed for the next 5 minutes. You do NOT need\n\
to repeat this call verbatim — refining your search terms is fine and will not\n\
be blocked again."
    )
}

/// The whole global registry (top-level `remote_mounts` / `remote_mount_topics`
/// — NOT the `repos` object the edit-guard reads).
fn load_hook_config() -> Option<Value> {
    let content = std::fs::read_to_string(repos_config_file()?).ok()?;
    serde_json::from_str(&content).ok()
}

/// Config string list, CR/LN/space trimmed per entry (a trailing `\r` from a
/// CRLF-written registry silently kills every match otherwise).
fn config_string_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|items| {
            items
                .iter()
                .map(ps_string)
                .map(|s| s.trim_matches(['\r', '\n', ' ']).to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn remote_mounts(cfg: &Value) -> Vec<String> {
    cfg.get("remote_mounts")
        .map(config_string_list)
        .unwrap_or_default()
}

/// Extra keywords a mount answers for: `.remote_mount_topics` keyed by the
/// full "<peer>/<alias>" or the bare alias.
fn mount_topics(cfg: &Value, mount: &str, alias: &str) -> Vec<String> {
    let Some(topics) = cfg.get("remote_mount_topics").and_then(Value::as_object) else {
        return Vec::new();
    };
    for key in [mount, alias] {
        if let Some(v) = topics.get(key) {
            return config_string_list(v);
        }
    }
    Vec::new()
}

/// Case-insensitive word-ish boundary match: "acme" hits "Acme DAM API" and
/// "help.acme.com" but not "acmexyz".
fn keyword_match(haystack_lower: &str, keyword: &str) -> bool {
    if keyword.trim().is_empty() {
        return false;
    }
    let pattern = format!(r"(^|[^a-z0-9]){}([^a-z0-9]|$)", regex::escape(keyword));
    regex::Regex::new(&pattern)
        .map(|re| re.is_match(haystack_lower))
        .unwrap_or(false)
}

fn read_web_cache(now: i64) -> Map<String, Value> {
    let Some(path) = hook_temp_file(WEB_CACHE_FILE_NAME) else {
        return Map::new();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Map::new();
    };
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .map(|cache| {
            // Expired entries are dropped, not written back — the file stays
            // small and a corrupt entry counts as expired.
            cache
                .into_iter()
                .filter(|(_, ts)| parse_ts(ts).is_some_and(|ts| now - ts < WEB_CACHE_TTL_SECS))
                .collect()
        })
        .unwrap_or_default()
}

fn write_web_cache(cache: &Map<String, Value>) {
    let Ok(data) = serde_json::to_string(cache) else {
        return;
    };
    if let Some(path) = hook_temp_file(WEB_CACHE_FILE_NAME) {
        write_json_atomic(&path, &data);
    }
}

// ---
// subagent-preamble (`codesearch hook subagent-preamble`) — port of
// subagent-preamble.{sh,ps1}
// ---

/// Idempotency marker: a prompt already carrying it is left untouched.
const PREAMBLE_MARKER: &str = "[[CODESEARCH-PREAMBLE]]";

/// `codesearch hook subagent-preamble` — PreToolUse hook injecting a
/// codesearch-first preamble into every Agent prompt. A fresh subagent does
/// NOT inherit AGENTS.md or the MCP initialize instructions; it DOES get
/// Grep/Glob as always-on tools. Never blocks, only rewrites the prompt.
pub fn run_subagent_preamble() {
    let raw = read_stdin();
    let out = std::panic::catch_unwind(|| subagent_preamble(&raw)).unwrap_or_default();
    emit_stdout(&out);
}

/// The rewritten-input decision for one Agent event, or empty when nothing
/// to inject (non-Agent tool, no prompt, marker already present).
fn subagent_preamble(raw: &str) -> String {
    let Ok(data) = serde_json::from_str::<Value>(raw.trim()) else {
        return String::new();
    };
    let tool = data.get("tool_name").and_then(Value::as_str).unwrap_or("");
    if tool != "Agent" {
        return String::new();
    }
    let inp = data.get("tool_input").unwrap_or(&Value::Null);
    let Some(obj) = inp.as_object() else {
        return String::new();
    };
    let Some(prompt) = obj.get("prompt").map(ps_string).filter(|p| !p.is_empty()) else {
        return String::new();
    };
    if prompt.contains(PREAMBLE_MARKER) {
        return String::new();
    }

    let new_prompt = format!("{PREAMBLE_MARKER}\n{PREAMBLE_TEXT}\n---\n{prompt}");
    let mut new_input = obj.clone();
    new_input.insert("prompt".to_string(), json!(new_prompt));
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": new_input,
            "permissionDecisionReason": "Injected codesearch-first preamble into subagent prompt (deferred MCP tools need an explicit ToolSearch load)."
        }
    })
    .to_string()
}

const PREAMBLE_TEXT: &str = "SEARCH RULE — read before doing anything:\n\
codesearch is the preferred search tool for code in the current repo.\n\
codesearch tools are DEFERRED: they do NOT appear in your tool list until you load them.\n\n\
Load them first (before any Grep or Glob):\n\
  ToolSearch(\"select:mcp__codesearch__search,mcp__codesearch__find,mcp__codesearch__explore,mcp__codesearch__get_chunk\")\n\n\
Then use:\n\
  mcp__codesearch__search(query, mode=\"semantic\")             -- concepts, identifiers, cross-file lookup\n\
  mcp__codesearch__search(query, mode=\"literal\", regex=true)  -- exact patterns / regex\n\
  mcp__codesearch__find(symbol, kind=\"definition\")             -- where a symbol is defined\n\
  mcp__codesearch__find(symbol, kind=\"usages\")                 -- all call sites\n\
  mcp__codesearch__explore(target, kind=\"outline\")             -- file/class structure\n\
  mcp__codesearch__get_chunk(chunk_id)                         -- read a specific code chunk\n\n\
Multi-repo serve mode: if a call returns \"scope_required\" or \"Unknown alias\",\n\
add project=\"<repo-alias>\" (single repo) or group=\"<group>\" (cross-repo). The\n\
error response lists the valid available_projects / available_groups — pick\n\
from that list; the alias may differ from the folder name.\n\n\
EDIT RULE: before editing code in a codesearch-registered repo, consult it\n\
for the file first — mcp__codesearch__find_impact (C#/TS) or\n\
mcp__codesearch__find(kind=\"usages\") (other code). The edit-guard hook\n\
blocks Edit/Write/MultiEdit on CODE files in that repo until you do\n\
(5-minute window); docs and config are never blocked.\n\n\
Fall back to Grep/Glob only after codesearch returns no useful results,\n\
or when the path is outside the current repo (codesearch covers internal\n\
paths only unless you're in multi-repo serve mode with an explicit group).\n\n\
If ToolSearch returns no codesearch tools, codesearch is not active for this\n\
session — proceed with Grep/Glob as normal.";

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

    // --- grep-guard (pure detector/target helpers; HTTP probes need a live
    // serve and are covered by the twin-parity contract + manual e2e) ---

    #[test]
    fn grep_family_detector_matches_contract() {
        let cases: &[(&str, bool)] = &[
            ("grep -rn foo .", true),
            ("rg pattern src/", true),
            ("egrep -i a b.txt", true),
            ("cat f | grep -i bar", true),
            ("git grep -n TODO", true),
            ("/usr/bin/grep -i x", true),
            ("C:/tools/grep.exe -i x", true),
            ("grep|head -5 --include=*.cs x", true),
            ("timeout 5 grep -r x .", true),
            ("echo `grep -rn foo src/`", true),
            ("echo \"grep is slow, use codesearch\"", false),
            ("cargo build --release", false),
            ("progress --all", false),
            ("rgx = /ab/", false),
            ("", false),
        ];
        for (cmd, want) in cases {
            assert_eq!(
                is_grep_family_command(cmd),
                *want,
                "detector mismatch for {cmd:?}"
            );
        }
    }

    #[test]
    fn grep_targets_follow_operands() {
        let cwd = "/work/session";
        assert!(
            grep_target_paths_with_base("grep -rn foo .", cwd)
                .iter()
                .all(|p| p.starts_with("/work/session")),
            "dot operand resolves against the base"
        );
        assert_eq!(
            grep_target_paths_with_base("grep -rn foo src/lib.rs", cwd),
            vec!["/work/session/src/lib.rs".to_string()],
            "relative operand joins the base"
        );
        assert_eq!(
            grep_target_paths_with_base("cd /other/repo && grep -rn foo .", cwd),
            vec!["/other/repo".to_string()],
            "cd prefix moves the relative base"
        );
        assert_eq!(
            grep_target_paths_with_base("grep -rn foo /abs/repo", cwd),
            vec!["/abs/repo".to_string()],
            "absolute operand passes through"
        );
        assert!(
            grep_target_paths_with_base("grep -rn foo", cwd).is_empty(),
            "no path operand: grep walks the cwd (empty = cwd target)"
        );
        assert!(
            grep_target_paths_with_base("grep -rn foo > out.txt", cwd)
                .iter()
                .all(|p| !p.ends_with("out.txt")),
            "redirect target is not an operand"
        );
        assert!(
            grep_target_paths_with_base("echo \"src/x\" && grep -rn foo .", cwd)
                .iter()
                .all(|p| !p.ends_with("src/x\"")),
            "quoted spans are never operands"
        );
    }

    #[test]
    fn drop_redirections_variants() {
        assert_eq!(
            drop_redirections("grep foo 2>&1").trim(),
            "grep foo",
            "fd duplication dropped"
        );
        assert_eq!(
            drop_redirections("grep foo > out.txt").trim(),
            "grep foo",
            "redirect with target dropped"
        );
        assert_eq!(
            drop_redirections("grep foo >> log").trim(),
            "grep foo",
            "append redirect dropped"
        );
        assert_eq!(
            drop_redirections("grep foo < in.txt").trim(),
            "grep foo",
            "stdin redirect dropped"
        );
    }

    // --- grep-guard HTTP decision path (in-process stub hub) ---

    fn grep_event(path: &Path) -> String {
        json!({ "tool_name": "Grep", "tool_input": { "path": path.display().to_string() } })
            .to_string()
    }

    /// Minimal HTTP stub: /healthz always answers 200; every /indexing call
    /// pops the next scripted response (None = 404). Lives on an ephemeral
    /// port; the returned URL goes into CODESEARCH_SERVER — which also pins
    /// the base-URL precedence (the default serve port answers nothing here).
    /// The accept thread is detached (it parks forever in `incoming`); test
    /// processes drop it at exit.
    fn stub_hub(script: Vec<Option<&'static str>>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let mut idx = 0usize;
            for stream in listener.incoming() {
                let Ok(mut sock) = stream else { break };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read until end of headers (or EOF); these GETs are tiny.
                loop {
                    match std::io::Read::read(&mut sock, &mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16_384 {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let req = String::from_utf8_lossy(&buf);
                let (status, body) = if req.contains("/healthz") {
                    ("200 OK", "ok".to_string())
                } else if req.contains("/indexing") {
                    match script.get(idx).copied().flatten() {
                        Some(body) => {
                            idx += 1;
                            ("200 OK", body.to_string())
                        }
                        None => ("404 Not Found", String::new()),
                    }
                } else {
                    ("404 Not Found", String::new())
                };
                let http = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut sock, http.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    /// A port with nothing behind it (bound, then released) — connection
    /// refused within the probe timeout.
    fn dead_base_url() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        format!("http://127.0.0.1:{port}")
    }

    fn run_async<F, T>(f: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        tokio::runtime::Runtime::new().unwrap().block_on(f)
    }

    fn grep_env(base: &str, wait_ms: &str) -> EnvRestore {
        EnvRestore::set(&[(SERVER_ENV, base), (GREP_WAIT_ENV, wait_ms)])
    }

    #[serial]
    #[test]
    fn grep_guard_allows_when_hub_is_down() {
        let fx = fixture();
        let envs = grep_env(&dead_base_url(), "0");
        let out = run_async(grep_guard(&grep_event(&fx.root_a)));
        assert!(out.is_empty(), "hub down: silent allow, got {out}");
        drop(envs);
    }

    #[serial]
    #[test]
    fn grep_guard_denies_when_live_and_fresh() {
        // Only the stub answers (CODESEARCH_SERVER) — a deny here also proves
        // the base-URL precedence: the default serve port answers nothing.
        let fx = fixture();
        let base = stub_hub(vec![Some(r#"{"covered":true,"indexing":false}"#)]);
        let envs = grep_env(&base, "0");
        let out = run_async(grep_guard(&grep_event(&fx.root_a)));
        let v: Value = serde_json::from_str(&out).expect("deny JSON");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            v["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("LIVE"),
            "standard deny message: {out}"
        );
        drop(envs);
    }

    #[serial]
    #[test]
    fn grep_guard_wait_cap_zero_denies_immediately_mid_reindex() {
        let fx = fixture();
        let base = stub_hub(vec![Some(r#"{"covered":true,"indexing":true}"#)]);
        let envs = grep_env(&base, "0");
        let out = run_async(grep_guard(&grep_event(&fx.root_a)));
        let v: Value = serde_json::from_str(&out).expect("deny JSON");
        assert!(
            v["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("WAIT 15-30 seconds"),
            "instant wait-deny on cap=0: {out}"
        );
        drop(envs);
    }

    #[serial]
    #[test]
    fn grep_guard_waits_out_rebuild_then_denies_settled() {
        let fx = fixture();
        // First probe: indexing; the poll lands on settled.
        let base = stub_hub(vec![
            Some(r#"{"covered":true,"indexing":true}"#),
            Some(r#"{"covered":true,"indexing":false}"#),
        ]);
        let envs = grep_env(&base, "1200");
        let out = run_async(grep_guard(&grep_event(&fx.root_a)));
        let v: Value = serde_json::from_str(&out).expect("deny JSON");
        assert!(
            v["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("landed while the guard waited"),
            "settled deny: {out}"
        );
        drop(envs);
    }

    #[serial]
    #[test]
    fn grep_guard_opens_grep_when_rebuild_outlasts_the_cap() {
        let fx = fixture();
        let base = stub_hub(vec![Some(r#"{"covered":true,"indexing":true}"#)]);
        let envs = grep_env(&base, "300");
        let out = run_async(grep_guard(&grep_event(&fx.root_a)));
        assert!(out.is_empty(), "past the cap grep opens, got {out}");
        drop(envs);
    }

    #[serial]
    #[test]
    fn grep_guard_allows_unregistered_target_without_probing() {
        // root_b is not in the registry: allow with zero HTTP (dead base).
        let fx = fixture();
        let envs = grep_env(&dead_base_url(), "0");
        let out = run_async(grep_guard(&grep_event(&fx.root_b)));
        assert!(out.is_empty(), "unregistered repo: allow, got {out}");
        drop(envs);
    }

    // --- web-guard ---

    /// Mounts registry + private cache dir; env restored RAII, cache dir
    /// cleared on drop (same pattern as Fixture).
    struct WebFx {
        _env_set: EnvRestore,
        _env_removed: EnvRestore,
        _tmp: tempfile::TempDir,
    }

    impl WebFx {
        fn drop_guard(&self) {
            TEST_STATE_DIR.lock().unwrap().take();
        }
    }

    impl Drop for WebFx {
        fn drop(&mut self) {
            self.drop_guard();
        }
    }

    fn web_fx(body: &str) -> WebFx {
        let tmp = tempfile::tempdir().unwrap();
        let repos_json = tmp.path().join("repos.json");
        std::fs::write(&repos_json, body).unwrap();
        let cache_dir = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        *TEST_STATE_DIR.lock().unwrap() = Some(cache_dir);
        let repos_str = repos_json.display().to_string();
        let env_set = EnvRestore::set(&[(REPOS_CONFIG_ENV, repos_str.as_str())]);
        let env_removed = EnvRestore::remove(&[SERVER_ENV, CODESEARCH_HOME_ENV]);
        WebFx {
            _env_set: env_set,
            _env_removed: env_removed,
            _tmp: tmp,
        }
    }

    fn web_event(tool: &str, text: &str) -> String {
        let input = if tool == "WebFetch" {
            json!({ "url": text })
        } else {
            json!({ "query": text })
        };
        json!({ "tool_name": tool, "tool_input": input }).to_string()
    }

    const MOUNTED_REGISTRY: &str = r#"{"repos": {}, "remote_mounts": ["peer/acme"], "remote_mount_topics": {"acme": ["inriver"]}}"#;

    #[serial]
    #[test]
    fn web_guard_steers_first_query_per_mount_then_allows() {
        let fx = web_fx(MOUNTED_REGISTRY);
        let denied = web_guard(&web_event(
            "WebSearch",
            "How does the Acme API handle pagination?",
        ));
        let v: Value = serde_json::from_str(&denied).expect("deny JSON");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert!(
            reason.contains("peer/acme"),
            "deny names the mount: {reason}"
        );
        assert!(reason.contains("pagination"), "deny echoes the query");
        // Follow-up about the same mount inside the window: allowed.
        assert!(
            web_guard(&web_event("WebSearch", "Acme pagination again")).is_empty(),
            "second query inside the window passes"
        );
        // A different tool is a different key: steered independently.
        assert!(
            !web_guard(&web_event("WebFetch", "https://help.acme.com/docs")).is_empty(),
            "WebFetch is steered on its own key"
        );
        fx.drop_guard();
    }

    #[serial]
    #[test]
    fn web_guard_ignores_unrelated_and_degenerate() {
        let fx = web_fx(MOUNTED_REGISTRY);
        // Not about a mounted product.
        assert!(
            web_guard(&web_event("WebSearch", "rust async trait objects")).is_empty(),
            "unrelated query passes"
        );
        // Mount-adjacent but no word boundary ("acmexyz" != "acme").
        assert!(
            web_guard(&web_event("WebSearch", "acmexyz licensing")).is_empty(),
            "keyword must not substring-match"
        );
        // Empty query.
        assert!(web_guard(&web_event("WebSearch", "  ")).is_empty());
        // Unrelated tool.
        assert!(
            web_guard(&web_event("Grep", "acme")).is_empty(),
            "non-web tools pass"
        );
        // Registry without mounts.
        let no_mounts = web_fx(r#"{"repos": {}}"#);
        assert!(
            web_guard(&web_event("WebSearch", "Acme API")).is_empty(),
            "no mounts: nothing to prefer"
        );
        drop(no_mounts);
        fx.drop_guard();
    }

    #[serial]
    #[test]
    fn web_guard_topic_keywords_and_crlf_trim() {
        // The topic keyword extends the match beyond the alias itself.
        let fx = web_fx(MOUNTED_REGISTRY);
        assert!(
            !web_guard(&web_event("WebSearch", "inriver PIM entity model")).is_empty(),
            "topic keyword matches and steers"
        );
        fx.drop_guard();

        // A CRLF-written registry must not kill every match: the trailing \r
        // inside the string value is trimmed before matching.
        let body = json!({
            "repos": {},
            "remote_mounts": ["peer/acme\r"],
            "remote_mount_topics": {}
        })
        .to_string();
        let fx = web_fx(&body);
        assert!(
            !web_guard(&web_event("WebSearch", "Acme docs")).is_empty(),
            "CRLF-trimmed mount still matches"
        );
        fx.drop_guard();
    }

    // --- subagent-preamble ---

    #[test]
    fn preamble_injects_into_agent_prompt() {
        let raw = json!({
            "tool_name": "Agent",
            "tool_input": { "prompt": "Find the auth flow", "subagent_type": "scout" }
        })
        .to_string();
        let out = subagent_preamble(&raw);
        let v: Value = serde_json::from_str(&out).expect("allow JSON");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
        let prompt = v["hookSpecificOutput"]["updatedInput"]["prompt"]
            .as_str()
            .expect("updated prompt");
        assert!(prompt.starts_with(PREAMBLE_MARKER), "marker prepended");
        assert!(
            prompt.contains("Find the auth flow"),
            "original prompt kept"
        );
        assert!(
            prompt.contains("ToolSearch("),
            "preamble teaches the deferred-tool load"
        );
        // Other tool_input keys survive.
        assert_eq!(
            v["hookSpecificOutput"]["updatedInput"]["subagent_type"],
            "scout"
        );
    }

    #[test]
    fn preamble_is_idempotent_and_narrow() {
        let already = json!({
            "tool_name": "Agent",
            "tool_input": { "prompt": "[[CODESEARCH-PREAMBLE]] already injected" }
        })
        .to_string();
        assert!(
            subagent_preamble(&already).is_empty(),
            "marker present: no re-injection"
        );
        assert!(
            subagent_preamble(&json!({ "tool_name": "Bash", "tool_input": {} }).to_string())
                .is_empty(),
            "non-Agent tools untouched"
        );
        assert!(
            subagent_preamble(
                &json!({ "tool_name": "Agent", "tool_input": { "prompt": "" } }).to_string()
            )
            .is_empty(),
            "empty prompt untouched"
        );
        assert!(subagent_preamble("not json").is_empty(), "garbage passes");
    }
}
