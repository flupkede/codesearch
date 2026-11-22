//! `codesearch hooks claude install` — installs the Claude Code PreToolUse
//! guard hooks (codesearch-first enforcement) into a `settings.json`.
//!
//! This is the Rust port of `integrations/claude-code/install.{sh,ps1}`. The
//! hook scripts are embedded at compile time (single source of truth remains
//! `integrations/claude-code/hooks/`) so the shipped binary is self-contained —
//! no dependency on the source tree at install time.
//!
//! Behaviour mirrors the shell installers:
//!   1. Write the script guards (grep/preamble/web) and the shared helper
//!      libraries into `<claude_dir>/hooks/codesearch/` — the edit-guard
//!      hooks run natively as `codesearch hook <stem>` and ship no scripts.
//!   2. Merge one PreToolUse registration per guard and one PostToolUse
//!      registration per companion into `settings.json`, keyed by the exact
//!      command string so re-running never duplicates. Legacy script-file
//!      registrations of the edit guards are removed on the way (migration).
//!   3. Back up an existing `settings.json` before rewriting it.
//!
//! `<claude_dir>` is `~/.claude` (user scope) or `./.claude` (`--project`).

use anyhow::{Context, Result};
use colored::Colorize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// All guards run natively as `codesearch hook <stem>` subcommands
// (src/cli/hook_guard.rs) — no script files are shipped or written anymore.

/// A guard hook to install: the Claude Code tool matcher it fires on and the
/// `codesearch hook <stem>` subcommand it dispatches to.
struct GuardHook {
    /// Claude Code matcher — a regex over the tool name (e.g. `"Grep"`).
    matcher: &'static str,
    /// The `codesearch hook <stem>` subcommand name (e.g. `"grep-guard"`).
    stem: &'static str,
}

/// The guards installed by `hooks claude install`, all native (`codesearch
/// hook <stem>`):
/// - `Grep` → codesearch-first for internal code discovery. `Bash` is in the
///   same matcher: a shell grep/rg through the Bash tool is the escape hatch
///   around the Grep denial, so grep-guard guards it identically (detector:
///   `is_grep_family_command` in hook_guard.rs).
/// - `Agent` → inject a codesearch-first preamble into subagent prompts.
/// - `WebSearch`/`WebFetch` → steer to remote doc mounts before the open web.
/// - `Edit`/`Write`/`MultiEdit` → require a recent codesearch consultation
///   (find_impact / find kind="usages") per file before edits.
static GUARD_HOOKS: &[GuardHook] = &[
    GuardHook {
        matcher: "Grep|Bash",
        stem: "grep-guard",
    },
    GuardHook {
        matcher: "Agent",
        stem: "subagent-preamble",
    },
    GuardHook {
        // A single matcher entry matching both web tools (regex over tool name).
        matcher: "WebSearch|WebFetch",
        stem: "web-guard",
    },
    GuardHook {
        matcher: "Edit|Write|MultiEdit",
        stem: "edit-guard",
    },
];

/// PostToolUse companion hooks. These can never block anything (Claude Code
/// ignores decisions there); they only record state — `edit-guard-post`
/// marks "codesearch consulted" per file path after qualifying MCP calls,
/// which `edit-guard` honours on the next Edit/Write/MultiEdit.
static POST_TOOL_USE_HOOKS: &[GuardHook] = &[GuardHook {
    matcher: "mcp__codesearch__find_impact|mcp__codesearch__find",
    stem: "edit-guard-post",
}];

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

/// Command string for a native hook: invoke the running binary's own
/// `hook <stem>` subcommand — one process per call, no shell, no script
/// files. Forward slashes and quoting, same convention as a path.
fn native_hook_command(stem: &str) -> Result<String> {
    let exe = std::env::current_exe()
        .context("could not resolve the running codesearch executable")?
        .display()
        .to_string()
        .replace('\\', "/");
    Ok(format!("\"{exe}\" hook {stem}"))
}

/// Legacy script suffixes the installer migrates away from: the pre-native
/// `pwsh …/edit-guard.ps1"` / `bash …/edit-guard.sh"` registrations (and the
/// -post variants). The trailing quote keeps `/other/edit-guard-old.ps1"`
/// style commands from matching.
fn legacy_script_suffixes(stem: &str) -> [String; 2] {
    [format!("/{stem}.ps1\""), format!("/{stem}.sh\"")]
}

/// Remove pre-native registrations of a hook from `hooks.<event>`: every
/// inner command ending in one of `suffixes` is dropped, entries left
/// without commands are removed whole. Unrelated entries survive untouched.
/// Returns true when anything changed.
fn remove_legacy_hooks(settings: &mut Value, event: &str, suffixes: &[String]) -> bool {
    let Some(list) = settings
        .get_mut("hooks")
        .and_then(|h| h.get_mut(event))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let mut changed = false;
    for entry in list.iter_mut() {
        let Some(obj) = entry.as_object_mut() else {
            continue;
        };
        let Some(hs) = obj.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        let before = hs.len();
        hs.retain(|h| {
            let cmd = h.get("command").and_then(Value::as_str).unwrap_or("");
            !suffixes.iter().any(|s| cmd.ends_with(s.as_str()))
        });
        changed |= hs.len() != before;
    }
    if changed {
        list.retain(|e| {
            e.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|hs| !hs.is_empty())
        });
    }
    changed
}

/// Ensure `settings.hooks.<event>` contains a `{matcher, hooks:[…]}` entry
/// for `command`. Idempotent: returns `Ok(false)` without modifying anything if
/// an entry with this exact `command` AND `matcher` already exists in `<event>`.
/// An entry with the same command but an older matcher is upgraded in place
/// (returns `Ok(true)`) — a re-install after a matcher change (e.g. `Grep` →
/// `Grep|Bash`) must reach already-installed settings, not silently skip.
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

    for entry in list.iter_mut() {
        let has_command = entry
            .get("hooks")
            .and_then(Value::as_array)
            .map(|hs| {
                hs.iter()
                    .any(|h| h.get("command").and_then(Value::as_str) == Some(command))
            })
            .unwrap_or(false);
        if !has_command {
            continue;
        }
        if entry.get("matcher").and_then(Value::as_str) == Some(matcher) {
            return Ok(false);
        }
        entry
            .as_object_mut()
            .context("hooks entry must be a JSON object")?
            .insert("matcher".to_string(), json!(matcher));
        return Ok(true);
    }

    list.push(json!({
        "matcher": matcher,
        "hooks": [ { "type": "command", "command": command } ]
    }));
    Ok(true)
}

/// Migrate + register one guard in `settings.json`: native hooks drop their
/// pre-native script registrations first (the dedup is by command), then the
/// `{matcher, command}` pair is merged idempotently.
fn register_guard(settings: &mut Value, event: &str, gh: &GuardHook) -> Result<()> {
    let cmd = native_hook_command(gh.stem)?;
    let suffixes = legacy_script_suffixes(gh.stem);
    if remove_legacy_hooks(settings, event, &suffixes) {
        eprintln!(
            "{}",
            format!(
                "Migrated {} {}: removed legacy script registration",
                event, gh.stem
            )
            .yellow()
        );
    }
    if add_matcher_hook(settings, event, gh.matcher, &cmd)? {
        eprintln!(
            "{}",
            format!("Registered {} {} hook -> {}", event, gh.matcher, cmd).green()
        );
    } else {
        eprintln!("Already registered: {} {} (skipping)", event, gh.matcher);
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
/// default `~/.claude`. All guards are native subcommands of this binary —
/// only `settings.json` is touched; no script files are written.
pub fn run_claude_install(project: bool) -> Result<()> {
    let claude_dir = claude_dir(project)?;
    let settings_path = claude_dir.join("settings.json");
    let mut settings = load_or_init_settings(&settings_path)?;

    for (event, hooks) in [
        ("PreToolUse", GUARD_HOOKS),
        ("PostToolUse", POST_TOOL_USE_HOOKS),
    ] {
        for gh in hooks {
            register_guard(&mut settings, event, gh)?;
        }
    }

    let pretty = serde_json::to_string_pretty(&settings)?;
    std::fs::write(&settings_path, pretty)
        .with_context(|| format!("writing {}", settings_path.display()))?;

    eprintln!();
    eprintln!(
        "{}",
        format!(
            "✓ Claude Code hooks registered in {}",
            settings_path.display()
        )
        .green()
    );
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
    fn add_matcher_hook_upgrades_matcher_in_place() {
        let mut settings = json!({});
        let cmd = "bash /x/grep-guard.sh";
        assert!(add_matcher_hook(&mut settings, "PreToolUse", "Grep", cmd).unwrap());
        // Same command, new matcher -> upgrade in place, never a duplicate
        // (a re-install after "Grep" -> "Grep|Bash" must reach installed settings).
        assert!(add_matcher_hook(&mut settings, "PreToolUse", "Grep|Bash", cmd).unwrap());
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1, "matcher upgrade must not duplicate the entry");
        assert_eq!(pre[0]["matcher"], "Grep|Bash");
        assert_eq!(pre[0]["hooks"][0]["command"], cmd);
        // Upgraded shape is idempotent again.
        assert!(!add_matcher_hook(&mut settings, "PreToolUse", "Grep|Bash", cmd).unwrap());
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
        assert!(
            matchers.contains(&"Grep|Bash"),
            "grep-guard must fire on the Grep tool AND the Bash escape hatch"
        );
        assert!(matchers.contains(&"Agent"));
        assert!(matchers.contains(&"WebSearch|WebFetch"));
        assert!(matchers.contains(&"Edit|Write|MultiEdit"));
        // The PostToolUse companion registers under its own event list.
        let post: Vec<&str> = POST_TOOL_USE_HOOKS.iter().map(|g| g.matcher).collect();
        assert!(post.contains(&"mcp__codesearch__find_impact|mcp__codesearch__find"));
        // Every installed stem must be a real `codesearch hook <stem>`
        // subcommand — the dispatcher in cli/mod.rs is the other half.
        let native: &[&str] = &[
            "grep-guard",
            "subagent-preamble",
            "web-guard",
            "edit-guard",
            "edit-guard-post",
        ];
        for g in GUARD_HOOKS.iter().chain(POST_TOOL_USE_HOOKS) {
            assert!(
                native.contains(&g.stem),
                "guard {} must have a native hook subcommand",
                g.stem
            );
        }
    }

    #[test]
    fn native_hook_command_shape() {
        for stem in [
            "grep-guard",
            "subagent-preamble",
            "web-guard",
            "edit-guard",
            "edit-guard-post",
        ] {
            let cmd = native_hook_command(stem).unwrap();
            assert!(
                cmd.starts_with('"'),
                "native command must quote the exe path: {cmd}"
            );
            assert_eq!(
                cmd.rsplit_once("\" hook ").map(|(_, s)| s),
                Some(stem),
                "native command must invoke `hook {stem}`: {cmd}"
            );
            assert!(!cmd.contains('\\'), "path must use forward slashes: {cmd}");
            assert!(
                !cmd.contains("pwsh") && !cmd.contains("bash"),
                "no shell: {cmd}"
            );
        }
    }

    #[test]
    fn migration_removes_legacy_script_registration() {
        let mut settings = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Edit|Write|MultiEdit", "hooks": [ { "type": "command",
                      "command": "pwsh -NoProfile -NonInteractive -File \"C:/u/.claude/hooks/codesearch/edit-guard.ps1\"" } ] }
                ],
                "PostToolUse": [
                    { "matcher": "mcp__codesearch__find_impact|mcp__codesearch__find", "hooks": [ { "type": "command",
                      "command": "bash \"C:/u/.claude/hooks/codesearch/edit-guard-post.sh\"" } ] }
                ]
            }
        });
        let gh = GUARD_HOOKS.iter().find(|g| g.stem == "edit-guard").unwrap();
        register_guard(&mut settings, "PreToolUse", gh).unwrap();
        let gh_post = POST_TOOL_USE_HOOKS
            .iter()
            .find(|g| g.stem == "edit-guard-post")
            .unwrap();
        register_guard(&mut settings, "PostToolUse", gh_post).unwrap();

        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(
            pre.len(),
            1,
            "legacy pwsh entry must be replaced, not duplicated"
        );
        assert!(
            !serde_json::to_string(&pre)
                .unwrap()
                .contains("edit-guard.ps1"),
            "pwsh script registration must be gone"
        );
        assert_eq!(
            pre[0]["hooks"][0]["command"],
            native_hook_command("edit-guard").unwrap()
        );

        let post = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 1);
        assert!(
            !serde_json::to_string(&post)
                .unwrap()
                .contains("edit-guard-post.sh"),
            "sh script registration must be gone"
        );
    }

    #[test]
    fn migration_preserves_unrelated_and_replaces_script_guards() {
        let mut settings = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [ { "type": "command", "command": "echo unrelated" } ] },
                    { "matcher": "Grep|Bash", "hooks": [ { "type": "command",
                      "command": "pwsh -NoProfile -NonInteractive -File \"C:/u/.claude/hooks/codesearch/grep-guard.ps1\"" } ] },
                    { "matcher": "WebSearch|WebFetch", "hooks": [ { "type": "command",
                      "command": "bash \"/u/.claude/hooks/codesearch/web-guard.sh\"" } ] }
                ]
            }
        });
        let grep = GUARD_HOOKS.iter().find(|g| g.stem == "grep-guard").unwrap();
        register_guard(&mut settings, "PreToolUse", grep).unwrap();
        let web = GUARD_HOOKS.iter().find(|g| g.stem == "web-guard").unwrap();
        register_guard(&mut settings, "PreToolUse", web).unwrap();
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 3, "unrelated survives, script entries replaced");
        let grep_entry = pre.iter().find(|e| e["matcher"] == "Grep|Bash").unwrap();
        assert_eq!(
            grep_entry["hooks"][0]["command"],
            native_hook_command("grep-guard").unwrap(),
            "grep-guard script registration must be migrated to native"
        );
        let web_entry = pre
            .iter()
            .find(|e| e["matcher"] == "WebSearch|WebFetch")
            .unwrap();
        assert_eq!(
            web_entry["hooks"][0]["command"],
            native_hook_command("web-guard").unwrap(),
            "web-guard script registration must be migrated to native"
        );
    }

    #[test]
    fn register_guard_is_idempotent() {
        let gh = GUARD_HOOKS.iter().find(|g| g.stem == "edit-guard").unwrap();
        let mut settings = json!({});
        register_guard(&mut settings, "PreToolUse", gh).unwrap();
        // Second run: no change, no duplicate.
        let before = settings.clone();
        register_guard(&mut settings, "PreToolUse", gh).unwrap();
        assert_eq!(settings, before, "re-install must be a no-op");
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
    }

    /// Regression gate for the shell detector twins (codesearch-common.sh's
    /// command_grep_family and codesearch-common.ps1's Test-GrepFamilyCommand)
    /// over the same case list as the TS twin's testBashCommandGrep. The
    /// backtick bypass (review round on fix/bash-grep-guard) drifted exactly
    /// these two files with nothing failing — this test runs the copies from
    /// integrations/ (the contract source of truth) under bash and pwsh,
    /// skipping a twin whose interpreter is not installed.
    #[test]
    fn grep_family_detector_twins_match_contract() {
        const COMMON_SH: &str =
            include_str!("../../integrations/claude-code/hooks/codesearch-common.sh");
        const COMMON_PS1: &str =
            include_str!("../../integrations/claude-code/hooks/codesearch-common.ps1");
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

        let dir = std::env::temp_dir().join(format!(
            "codesearch-detector-twins-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("codesearch-common.sh"), COMMON_SH).unwrap();
        std::fs::write(dir.join("codesearch-common.ps1"), COMMON_PS1).unwrap();

        for (cmd, want_deny) in cases {
            // sh twin: sourcing the common file defines command_grep_family.
            // Windows skips it: `bash` on PATH there resolves to the WSL stub
            // (System32), which cannot run the hook scripts; the sh twin is
            // gated on Linux/macOS runs, the ps1 twin on Windows.
            if !cfg!(windows) {
                if let Some(out) = run_detector_probe(
                    "bash",
                    &[
                        "-c",
                        ". \"$CODESEARCH_TEST_DIR\"/codesearch-common.sh; if command_grep_family \"$CODESEARCH_TEST_CMD\"; then echo YES; else echo no; fi",
                    ],
                    &dir,
                    cmd,
                ) {
                    assert_eq!(
                        out,
                        if *want_deny { "YES" } else { "no" },
                        "bash twin mismatch for {cmd:?}"
                    );
                }
            }
            // ps1 twin.
            if let Some(out) = run_detector_probe(
                "pwsh",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    ". (Join-Path $env:CODESEARCH_TEST_DIR 'codesearch-common.ps1'); if (Test-GrepFamilyCommand $env:CODESEARCH_TEST_CMD) { 'YES' } else { 'no' }",
                ],
                &dir,
                cmd,
            ) {
                assert_eq!(
                    out,
                    if *want_deny { "YES" } else { "no" },
                    "pwsh twin mismatch for {cmd:?}"
                );
            }
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Run one detector probe with `CODESEARCH_TEST_CMD=<cmd>`; returns trimmed
    /// stdout, or None when the interpreter is not installed (skip, not fail).
    fn run_detector_probe(program: &str, args: &[&str], dir: &Path, cmd: &str) -> Option<String> {
        let output = match std::process::Command::new(program)
            .args(args)
            .env("CODESEARCH_TEST_CMD", cmd)
            .env("CODESEARCH_TEST_DIR", dir)
            .output()
        {
            Ok(out) => out,
            // Interpreter not installed: skip this twin rather than fail.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => panic!("{program} probe failed for {cmd:?}: {err}"),
        };
        assert!(
            output.status.success(),
            "{program} probe exited {:?} for {cmd:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}
