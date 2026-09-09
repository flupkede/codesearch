//! `codesearch hooks claude install` — installs the Claude Code PreToolUse
//! guard hooks (codesearch-first enforcement) into a `settings.json`.
//!
//! This is the Rust port of `integrations/claude-code/install.{sh,ps1}`. The
//! hook scripts are embedded at compile time (single source of truth remains
//! `integrations/claude-code/hooks/`) so the shipped binary is self-contained —
//! no dependency on the source tree at install time.
//!
//! Behaviour mirrors the shell installers:
//!   1. Write the hook scripts (guards, the PostToolUse companion and the
//!      shared helper libraries) into `<claude_dir>/hooks/codesearch/`.
//!   2. Merge one PreToolUse registration per guard and one PostToolUse
//!      registration per companion into `settings.json`, keyed by the exact
//!      command string so re-running never duplicates.
//!   3. Back up an existing `settings.json` before rewriting it.
//!
//! `<claude_dir>` is `~/.claude` (user scope) or `./.claude` (`--project`).

use anyhow::{Context, Result};
use colored::Colorize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// Embedded hook scripts. `integrations/claude-code/hooks/` stays the single
// source of truth; these are baked into the binary at build time.
const GREP_GUARD_SH: &str = include_str!("../../integrations/claude-code/hooks/grep-guard.sh");
const GREP_GUARD_PS1: &str = include_str!("../../integrations/claude-code/hooks/grep-guard.ps1");
const PREAMBLE_SH: &str = include_str!("../../integrations/claude-code/hooks/subagent-preamble.sh");
const PREAMBLE_PS1: &str =
    include_str!("../../integrations/claude-code/hooks/subagent-preamble.ps1");
const WEB_GUARD_SH: &str = include_str!("../../integrations/claude-code/hooks/web-guard.sh");
const WEB_GUARD_PS1: &str = include_str!("../../integrations/claude-code/hooks/web-guard.ps1");
const EDIT_GUARD_SH: &str = include_str!("../../integrations/claude-code/hooks/edit-guard.sh");
const EDIT_GUARD_PS1: &str = include_str!("../../integrations/claude-code/hooks/edit-guard.ps1");
const EDIT_GUARD_POST_SH: &str =
    include_str!("../../integrations/claude-code/hooks/edit-guard-post.sh");
const EDIT_GUARD_POST_PS1: &str =
    include_str!("../../integrations/claude-code/hooks/edit-guard-post.ps1");
const CODESEARCH_COMMON_SH: &str =
    include_str!("../../integrations/claude-code/hooks/codesearch-common.sh");
const CODESEARCH_COMMON_PS1: &str =
    include_str!("../../integrations/claude-code/hooks/codesearch-common.ps1");

/// A PreToolUse guard hook to install: the tool matcher it fires on, the script
/// basename, and the per-platform script files to write out.
struct GuardHook {
    /// Claude Code matcher — a regex over the tool name (e.g. `"Grep"`).
    matcher: &'static str,
    /// Script basename without extension (e.g. `"grep-guard"`).
    stem: &'static str,
    /// `(filename, embedded contents)` for each shell variant.
    files: &'static [(&'static str, &'static str)],
}

/// The guards installed by `hooks claude install`:
/// - `Grep` → codesearch-first for internal code discovery.
/// - `Agent` → inject a codesearch-first preamble into subagent prompts.
/// - `WebSearch`/`WebFetch` → steer to remote doc mounts before the open web.
/// - `Edit`/`Write`/`MultiEdit` → require a recent codesearch consultation
///   (find_impact / find kind="usages") per file before edits.
static GUARD_HOOKS: &[GuardHook] = &[
    GuardHook {
        matcher: "Grep",
        stem: "grep-guard",
        files: &[
            ("grep-guard.sh", GREP_GUARD_SH),
            ("grep-guard.ps1", GREP_GUARD_PS1),
        ],
    },
    GuardHook {
        matcher: "Agent",
        stem: "subagent-preamble",
        files: &[
            ("subagent-preamble.sh", PREAMBLE_SH),
            ("subagent-preamble.ps1", PREAMBLE_PS1),
        ],
    },
    GuardHook {
        // A single matcher entry matching both web tools (regex over tool name).
        matcher: "WebSearch|WebFetch",
        stem: "web-guard",
        files: &[
            ("web-guard.sh", WEB_GUARD_SH),
            ("web-guard.ps1", WEB_GUARD_PS1),
        ],
    },
    GuardHook {
        matcher: "Edit|Write|MultiEdit",
        stem: "edit-guard",
        files: &[
            ("edit-guard.sh", EDIT_GUARD_SH),
            ("edit-guard.ps1", EDIT_GUARD_PS1),
        ],
    },
];

/// PostToolUse companion hooks. These can never block anything (Claude Code
/// ignores decisions there); they only record state — `edit-guard-post`
/// marks "codesearch consulted" per file path after qualifying MCP calls,
/// which `edit-guard` honours on the next Edit/Write/MultiEdit.
static POST_TOOL_USE_HOOKS: &[GuardHook] = &[GuardHook {
    matcher: "mcp__codesearch__find_impact|mcp__codesearch__find",
    stem: "edit-guard-post",
    files: &[
        ("edit-guard-post.sh", EDIT_GUARD_POST_SH),
        ("edit-guard-post.ps1", EDIT_GUARD_POST_PS1),
    ],
}];

/// Shared helper libraries sourced by the guard scripts at runtime; written
/// once into the same hooks directory as the guards themselves.
static COMMON_HOOK_FILES: &[(&str, &str)] = &[
    ("codesearch-common.sh", CODESEARCH_COMMON_SH),
    ("codesearch-common.ps1", CODESEARCH_COMMON_PS1),
];

/// Resolve the `.claude` directory for the requested scope.
fn claude_dir(project: bool) -> Result<PathBuf> {
    if project {
        Ok(std::env::current_dir()
            .context("could not determine current directory")?
            .join(".claude"))
    } else {
        Ok(dirs::home_dir()
            .context("could not determine home directory")?
            .join(".claude"))
    }
}

/// Build the settings.json `command` string for a guard, picking the shell that
/// matches the host OS (pwsh on Windows, bash elsewhere). Paths use forward
/// slashes so the command is valid regardless of shell quoting rules.
fn hook_command(hooks_dest: &Path, stem: &str) -> String {
    let dir = hooks_dest.display().to_string().replace('\\', "/");
    if cfg!(windows) {
        format!("pwsh -NoProfile -NonInteractive -File \"{dir}/{stem}.ps1\"")
    } else {
        format!("bash \"{dir}/{stem}.sh\"")
    }
}

/// Ensure `settings.hooks.<event>` contains a `{matcher, hooks:[…]}` entry
/// for `command`. Idempotent: returns `Ok(false)` without modifying anything if
/// an entry with this exact `command` already exists anywhere in `<event>`.
/// Returns an error if a pre-existing `hooks`/`<event>` value has a shape
/// incompatible with the expected object/array.
fn add_matcher_hook(
    settings: &mut Value,
    event: &str,
    matcher: &str,
    command: &str,
) -> Result<bool> {
    let root = settings
        .as_object_mut()
        .context("settings.json root must be a JSON object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("settings.hooks must be a JSON object")?;
    let list = hooks
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("settings.hooks.{event} must be a JSON array")?;

    let already = list.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(Value::as_array)
            .map(|hs| {
                hs.iter()
                    .any(|h| h.get("command").and_then(Value::as_str) == Some(command))
            })
            .unwrap_or(false)
    });
    if already {
        return Ok(false);
    }

    list.push(json!({
        "matcher": matcher,
        "hooks": [ { "type": "command", "command": command } ]
    }));
    Ok(true)
}

/// Write one hook script (or shared library) into the hooks directory,
/// executable on unix. Scripts are re-written verbatim on every install.
fn write_hook_file(hooks_dest: &Path, name: &str, contents: &str) -> Result<()> {
    let path = hooks_dest.join(name);
    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    if name.ends_with(".sh") {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms)?;
    }
    Ok(())
}

/// Load an existing `settings.json` (backing it up first) or start from `{}`.
fn load_or_init_settings(settings_path: &Path) -> Result<Value> {
    if !settings_path.exists() {
        return Ok(json!({}));
    }
    let raw = std::fs::read_to_string(settings_path)
        .with_context(|| format!("reading {}", settings_path.display()))?;

    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup = format!("{}.bak-{stamp}", settings_path.display());
    std::fs::write(&backup, &raw).with_context(|| format!("writing backup {backup}"))?;
    eprintln!("Backed up existing settings to {backup}");

    serde_json::from_str(&raw)
        .with_context(|| format!("{} is not valid JSON", settings_path.display()))
}

/// Install the Claude Code guard hooks. `project` selects `./.claude` over the
/// default `~/.claude`.
pub fn run_claude_install(project: bool) -> Result<()> {
    let claude_dir = claude_dir(project)?;
    let hooks_dest = claude_dir.join("hooks").join("codesearch");
    std::fs::create_dir_all(&hooks_dest)
        .with_context(|| format!("creating {}", hooks_dest.display()))?;

    // 1. Write the hook scripts (guards, PostToolUse companions, shared
    //    helper libraries — all into the same directory).
    for (name, contents) in COMMON_HOOK_FILES {
        write_hook_file(&hooks_dest, name, contents)?;
    }
    for gh in GUARD_HOOKS.iter().chain(POST_TOOL_USE_HOOKS) {
        for (name, contents) in gh.files {
            write_hook_file(&hooks_dest, name, contents)?;
        }
    }

    // 2. Merge the PreToolUse / PostToolUse registrations into settings.json.
    // (`claude_dir` already exists — creating `hooks_dest` above made it.)
    let settings_path = claude_dir.join("settings.json");
    let mut settings = load_or_init_settings(&settings_path)?;

    for (event, hooks) in [
        ("PreToolUse", GUARD_HOOKS),
        ("PostToolUse", POST_TOOL_USE_HOOKS),
    ] {
        for gh in hooks {
            let cmd = hook_command(&hooks_dest, gh.stem);
            if add_matcher_hook(&mut settings, event, gh.matcher, &cmd)? {
                eprintln!(
                    "{}",
                    format!("Registered {} {} hook -> {}", event, gh.matcher, cmd).green()
                );
            } else {
                eprintln!("Already registered: {} {} (skipping)", event, gh.matcher);
            }
        }
    }

    let pretty = serde_json::to_string_pretty(&settings)?;
    std::fs::write(&settings_path, pretty)
        .with_context(|| format!("writing {}", settings_path.display()))?;

    eprintln!();
    eprintln!(
        "{}",
        format!("✓ Claude Code hooks installed to {}", hooks_dest.display()).green()
    );
    eprintln!("  Settings updated: {}", settings_path.display());
    eprintln!("  Restart Claude Code (or start a new session) for the hooks to take effect.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_matcher_hook_registers_on_empty_settings() {
        let mut settings = json!({});
        let added =
            add_matcher_hook(&mut settings, "PreToolUse", "Grep", "bash /x/grep-guard.sh").unwrap();
        assert!(added, "first registration must add the hook");
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["matcher"], "Grep");
        assert_eq!(pre[0]["hooks"][0]["type"], "command");
        assert_eq!(pre[0]["hooks"][0]["command"], "bash /x/grep-guard.sh");
    }

    #[test]
    fn add_matcher_hook_is_idempotent_by_command() {
        let mut settings = json!({});
        let cmd = "bash /x/grep-guard.sh";
        assert!(add_matcher_hook(&mut settings, "PreToolUse", "Grep", cmd).unwrap());
        // Same command again -> no-op, no duplicate.
        assert!(!add_matcher_hook(&mut settings, "PreToolUse", "Grep", cmd).unwrap());
        assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn post_tool_use_hook_is_idempotent_by_command() {
        let mut settings = json!({});
        let cmd = "bash /x/edit-guard-post.sh";
        assert!(add_matcher_hook(
            &mut settings,
            "PostToolUse",
            "mcp__codesearch__find_impact|mcp__codesearch__find",
            cmd
        )
        .unwrap());
        // Same command again -> no-op, no duplicate, and the entry stays in
        // PostToolUse (never leaking into PreToolUse).
        assert!(!add_matcher_hook(
            &mut settings,
            "PostToolUse",
            "mcp__codesearch__find_impact|mcp__codesearch__find",
            cmd
        )
        .unwrap());
        let post = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 1);
        assert_eq!(
            post[0]["matcher"],
            "mcp__codesearch__find_impact|mcp__codesearch__find"
        );
        assert!(settings["hooks"].get("PreToolUse").is_none());
    }

    #[test]
    fn add_matcher_hook_preserves_unrelated_entries() {
        let mut settings = json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [ { "type": "command", "command": "echo hi" } ] }
                ]
            }
        });
        assert!(
            add_matcher_hook(&mut settings, "PreToolUse", "Grep", "bash /x/grep-guard.sh").unwrap()
        );
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "existing Bash hook must survive");
        assert_eq!(settings["model"], "opus", "unrelated settings must survive");
    }

    #[test]
    fn add_matcher_hook_rejects_bad_pretooluse_shape() {
        let mut settings = json!({ "hooks": { "PreToolUse": "not-an-array" } });
        assert!(add_matcher_hook(&mut settings, "PreToolUse", "Grep", "cmd").is_err());
    }

    #[test]
    fn add_matcher_hook_rejects_non_object_hooks() {
        let mut settings = json!({ "hooks": "not-an-object" });
        assert!(add_matcher_hook(&mut settings, "PreToolUse", "Grep", "cmd").is_err());
    }

    #[test]
    fn add_matcher_hook_rejects_non_object_root() {
        let mut settings = json!(["not", "an", "object"]);
        assert!(add_matcher_hook(&mut settings, "PreToolUse", "Grep", "cmd").is_err());
    }

    #[test]
    fn guard_hooks_cover_grep_agent_edit_and_web() {
        let matchers: Vec<&str> = GUARD_HOOKS.iter().map(|g| g.matcher).collect();
        assert!(matchers.contains(&"Grep"));
        assert!(matchers.contains(&"Agent"));
        assert!(matchers.contains(&"WebSearch|WebFetch"));
        assert!(matchers.contains(&"Edit|Write|MultiEdit"));
        // The PostToolUse companion registers under its own event list.
        let post: Vec<&str> = POST_TOOL_USE_HOOKS.iter().map(|g| g.matcher).collect();
        assert!(post.contains(&"mcp__codesearch__find_impact|mcp__codesearch__find"));
        // The shared helper libraries ship alongside the guards.
        for (name, body) in COMMON_HOOK_FILES {
            assert!(!body.is_empty(), "{name} embedded body is empty");
        }
        // Every guard and companion ships both a .sh and a .ps1 with
        // non-empty embedded bodies.
        for g in GUARD_HOOKS.iter().chain(POST_TOOL_USE_HOOKS) {
            assert_eq!(
                g.files.len(),
                2,
                "guard {} must ship both shells",
                g.matcher
            );
            for (name, body) in g.files {
                assert!(!body.is_empty(), "{name} embedded body is empty");
            }
        }
    }

    #[test]
    fn hook_command_targets_host_shell() {
        let cmd = hook_command(Path::new("/home/u/.claude/hooks/codesearch"), "grep-guard");
        if cfg!(windows) {
            assert!(cmd.starts_with("pwsh "));
            assert!(cmd.ends_with("grep-guard.ps1\""));
        } else {
            assert!(cmd.starts_with("bash "));
            assert!(cmd.ends_with("grep-guard.sh\""));
        }
    }
}
