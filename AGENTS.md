# AGENTS.md — features/improve_doctor

## Goal

Extend `codesearch doctor` with two new modes:

1. `codesearch doctor --all` — runs all checks on every repo in `~/.codesearch/repos.json`
   and prints a consolidated report.
2. `codesearch doctor --repo <alias>` — runs all checks on a specific registered alias,
   from any working directory.

Current behaviour (no flags): checks the current directory only — this stays unchanged.

---

## CLI changes

### File: `src/cli/mod.rs`

Find the `Doctor` variant in the `Commands` enum and add two new optional args:

```rust
/// Run diagnostics on the index
Doctor {
    /// Apply automatic fixes where possible
    #[arg(long)]
    fix: bool,

    /// Output results as JSON
    #[arg(long)]
    json: bool,

    /// Run diagnostics on all registered repositories (from repos.json)
    #[arg(long)]
    all: bool,

    /// Run diagnostics on a specific registered alias (e.g. --repo husq-aprimo)
    #[arg(long, value_name = "ALIAS")]
    repo: Option<String>,
},
```

Then in the `match` arm that calls `crate::cli::doctor::run(fix, json)`,
pass the new args:

```rust
Commands::Doctor { fix, json, all, repo } => {
    crate::cli::doctor::run(fix, json, all, repo).await
}
```

### File: `src/cli/doctor.rs`

Change the signature of `pub async fn run`:

```rust
pub async fn run(fix: bool, json: bool, all: bool, repo: Option<String>) -> Result<()>
```

---

## Implementation

### New helper: `run_for_path`

Extract the existing body of `run()` (from `let project_path = Path::new(".")` down to
`Ok(())`) into a new private async function:

```rust
async fn run_for_path(
    project_path: &Path,
    fix: bool,
    json: bool,
) -> Result<(usize, usize)>  // returns (warnings, errors)
```

This function runs all checks for a single project path and returns the warning/error
counts. It should NOT call `anyhow::bail!` on errors — instead return `Ok((0, errors))`.
The caller decides whether to bail.

### Updated `run()`

```rust
pub async fn run(fix: bool, json: bool, all: bool, repo: Option<String>) -> Result<()> {
    use crate::db_discovery::repos::ReposConfig;

    // --repo <alias> mode
    if let Some(alias) = repo {
        let config = ReposConfig::load().unwrap_or_default();
        match config.repos.get(&alias) {
            Some(path) => {
                let (_, errors) = run_for_path(path, fix, json).await?;
                if errors > 0 {
                    anyhow::bail!("Doctor found {} error(s) in '{}'", errors, alias);
                }
                return Ok(());
            }
            None => {
                anyhow::bail!(
                    "Unknown alias '{}'. Run 'codesearch index list' to see registered repos.",
                    alias
                );
            }
        }
    }

    // --all mode
    if all {
        let config = ReposConfig::load().unwrap_or_default();
        if config.repos.is_empty() {
            println!("No repositories registered.");
            return Ok(());
        }

        let mut total_warnings = 0usize;
        let mut total_errors = 0usize;
        let mut entries: Vec<_> = config.repos.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));

        for (alias, path) in &entries {
            println!();
            println!("{}", format!("── {} ──", alias).bright_cyan().bold());
            let (w, e) = run_for_path(path, fix, json).await.unwrap_or((0, 1));
            total_warnings += w;
            total_errors += e;
        }

        println!();
        println!("{}", "═".repeat(60));
        println!(
            "  All repos: {} warnings, {} errors across {} repositories",
            total_warnings,
            total_errors,
            entries.len()
        );

        if total_errors > 0 {
            anyhow::bail!("Doctor found errors in one or more repositories");
        }
        return Ok(());
    }

    // Default: current directory (existing behaviour unchanged)
    let (_, errors) = run_for_path(Path::new("."), fix, json).await?;
    if errors > 0 {
        anyhow::bail!("Doctor found {} error(s)", errors);
    }
    Ok(())
}
```

---

## Output examples

### `codesearch doctor --repo husq-aprimo`

```
🔍 Codesearch Doctor
============================================================
  ✅ Database found
  ✅ Database structure
  ✅ Model consistency
  ✅ Git root placement
  ⚠️  File integrity — 3 stale files
  ...

Summary
============================================================
  1 warning, 0 errors
```

### `codesearch doctor --all`

```
── aprimo_mcp ──
🔍 Codesearch Doctor
  ✅ Database found
  ...

── BRU.Aprimo ──
🔍 Codesearch Doctor
  ✅ Database found
  ...

══════════════════════════════════════════════════════════════
  All repos: 2 warnings, 0 errors across 12 repositories
```

---

## Quality gates

- [ ] `cargo check` clean
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo test --lib --bins` — all existing doctor tests pass, no changes to
      test logic needed (tests call `run_for_path` directly or mock the path)
- [ ] Manual: `codesearch doctor` (no flags) — behaviour unchanged
- [ ] Manual: `codesearch doctor --repo codesearch-git` — checks only that alias
- [ ] Manual: `codesearch doctor --all` — checks all 12 repos, consolidated summary
- [ ] Manual: `codesearch doctor --repo nonexistent` — clear error message

## CHANGELOG

Add under a new version section:

```markdown
### Added

- `codesearch doctor --repo <alias>` — run diagnostics on a specific registered
  alias from any working directory.
- `codesearch doctor --all` — run diagnostics on all repos in `repos.json` with
  a consolidated warning/error summary.
```

## Branch flow

```powershell
git push origin features/improve_doctor
# PR features/improve_doctor → develop
# merge, then run ..\release.ps1
```

## Done when

- [ ] `run_for_path` extracted and working
- [ ] `--repo` mode implemented and tested
- [ ] `--all` mode implemented and tested
- [ ] Default mode (no flags) unchanged
- [ ] Quality gates pass
- [ ] CHANGELOG updated
- [ ] PR opened against `develop`
