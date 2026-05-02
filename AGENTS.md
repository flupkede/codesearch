# AGENTS.md — feature/index-list-fix

## Goal

Fix `codesearch index list` so it actually lists all registered repositories
from `~/.codesearch/repos.json` instead of only checking the current directory.

The command currently has two `TODO` comments and prints almost nothing useful
for a user who has registered repos via `serve` or `codesearch index add`.

## Why this matters

A user who downloads a release runs `codesearch index list` to discover what
is registered. Today they get an empty output unless they happen to be standing
in a registered repo. This is the primary "what do I have?" entry point and it
must work.

## Files to change

- `src/index/mod.rs` — replace the body of `pub async fn list()`

No other files need changes. No new dependencies.

## Required behaviour

```
$ codesearch index list
📚 Indexed Repositories
============================================================

  codesearch-git    \\?\C:\WorkArea\AI\codesearch\codesearch.git
                    1727 chunks in 54 files

  husq-aprimo       \\?\C:\Users\develterf\source\repos\HUSQ.Aprimo
                    Could not open database (locked by serve)

  investing         C:\WorkArea\AI\investing
                    2369 chunks in 227 files

  ... (one entry per registered repo, sorted alphabetically by alias)

12 repositories registered.
```

If the current directory has a `.codesearch.db` that is **not** registered in
`repos.json`, append a separate "Local (unregistered)" section at the end so
the user sees their loose DB too.

If `repos.json` does not exist or is empty, print `No repositories registered.`
and continue to the local-DB check.

## Implementation

Use `crate::db_discovery::repos::ReposConfig` (already imported elsewhere in
`src/index/mod.rs`). It exposes:

- `ReposConfig::load() -> Result<Self>` — reads `~/.codesearch/repos.json`
- `config.repos: HashMap<String, PathBuf>` — alias → project path

Pseudocode:

```rust
pub async fn list() -> Result<()> {
    use crate::db_discovery::repos::ReposConfig;

    println!("{}", "📚 Indexed Repositories".bright_cyan().bold());
    println!("{}", "=".repeat(60));

    let config = ReposConfig::load().unwrap_or_default();

    if config.repos.is_empty() {
        println!("\n  No repositories registered.");
    } else {
        let mut entries: Vec<_> = config.repos.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));

        for (alias, project_path) in &entries {
            println!();
            println!("  {}", alias.bright_green());
            let db_path = project_path.join(".codesearch.db");
            print_repo_stats(project_path, &db_path)?;
        }

        println!();
        println!("{} repositories registered.", entries.len());
    }

    // Also show a loose local DB if the user is standing in one
    let current_dir = std::env::current_dir()?;
    let current_db = current_dir.join(".codesearch.db");
    let current_alias = config.alias_for_path(&current_dir);

    if current_db.exists() && current_alias.is_none() {
        println!();
        println!("{}", "Local (unregistered):".bright_yellow());
        print_repo_stats(&current_dir, &current_db)?;
    }

    Ok(())
}
```

Notes:
- `print_repo_stats` already handles "could not open database" gracefully
  (returns the dimmed message). Don't change it.
- `alias_for_path` already exists on `ReposConfig` (chunk 1751 in serve_hub
  index — see `src/db_discovery/repos.rs:221`).
- Remove both `TODO` comments — they are now resolved.
- Keep the `#[allow(dead_code)]` on `print_repo_stats` removed if you can —
  it's now actively used. If clippy complains, leave the attribute.

## Quality gates

- [ ] `cargo check` clean
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo test --lib --bins` — all tests pass (no test changes expected)
- [ ] Manual: `codesearch index list` prints all 12+ registered aliases
- [ ] Manual: standing in a registered repo, that alias is shown (not duplicated
      as "Local (unregistered)")
- [ ] Manual: standing in a directory with a stale `.codesearch.db` not in
      `repos.json`, it appears under "Local (unregistered)"

## CHANGELOG

Add under a new `## [1.0.82] - 2026-05-02` section (or whatever version the
hook bumps to):

```markdown
### Fixed

- `codesearch index list` now actually lists all repositories registered in
  `~/.codesearch/repos.json` instead of only checking the current directory.
  A loose `.codesearch.db` in an unregistered directory is shown separately
  under "Local (unregistered)".
```

## Branch flow

When done:

```powershell
git push origin feature/index-list-fix
# then from claude.ai or similar: open PR feature/index-list-fix → develop
# merge, then run release.ps1 in C:\WorkArea\AI\codesearch
```

## Done when

- [ ] `pub async fn list()` rewritten and both TODOs removed
- [ ] Quality gates pass
- [ ] Manual smoke tests pass
- [ ] CHANGELOG.md updated
- [ ] PR opened against `develop`
