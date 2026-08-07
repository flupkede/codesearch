---
description: Land the current feature branch on develop — README/CHANGELOG checks, commit, push, PR, auto-merge
argument-hint: [optional PR title]
allowed-tools: Bash(git:*), Bash(gh:*), Bash(cargo:*), Bash(grep:*), Read, Edit, Grep, Glob
---

# /merge — land the current feature branch on `develop`

Run the project's **merge workflow**: verify docs are current, then bring the current
feature branch into `develop` through a pull request. This command does **not** tag a
release — tagging happens only in `/release`.

## Branch & version facts (this repo)
- Flow: any working branch (`feature/*`, `features/*`, `fix/*`, `chore/*`, `docs/*`) → PR →
  **`develop`** → (later) PR → **`master`**.
- `master` is protected (`.github/workflows/protect-master.yml`): it accepts PRs only from
  `develop` or `release/*`. The `.githooks/pre-push` hook also refuses a direct push to it.
- **The pre-commit hook runs `cargo fmt` only.** It does *not* bump the version and does not
  build a binary — see its own header comment and `RELEASING.md` ("Does **not** bump the
  version or build a binary"). The per-commit auto-bump was removed: it churned the base
  version and forced a slow debug rebuild before every commit, while `build.rs` already
  stamps each build uniquely, so it bought no traceability.
- **The patch version bumps after the merge, not before it** — CI does it via
  `.github/workflows/bump-develop.yml` once the PR lands on `develop` (visible in history as
  `chore: bump version to X.Y.Z (auto, PR #N merged to develop)`). Nothing you commit on this
  branch changes the version.
- **Merge style: feature→`develop` is a merge commit (`--merge`), NOT a squash.** `develop`→
  `master` release PRs are the squashed ones, and that is `/release`'s job. Both `AGENTS.md`
  ("Merge style = merge commits (`--merge`), not squash") and `RELEASING.md` say so, and the
  history agrees — `develop` is full of two-parent `Merge PR #N` commits.

## Guardrails
- ABORT if the current branch is `develop`, `master`, or `release/*` — those are merge
  *targets*, not sources. Any other branch is fine; there is no version-bump premise to
  protect, since the bump happens in CI after the merge.
- NEVER push directly to `develop` or `master` — everything lands via a PR.
- NEVER pass `--no-verify` / `--no-gpg-sign` — let the pre-commit hook run its `cargo fmt`.
- Do NOT create or push a tag here. That is `/release`'s job.
- Do NOT force-push.

## Steps

1. **Context**
   - `git rev-parse --abbrev-ref HEAD` → current branch. If it IS `develop`, `master` or
     `release/*`, STOP with an error (see Guardrails).
   - `git fetch origin`.
   - Compute the change set landing on develop: `git log origin/develop..HEAD --oneline`
     plus `git status --short` for uncommitted work. If there is nothing to land, report and STOP.

2. **README up to date?**
   - Inspect the change set for user-facing changes: new/removed CLI flags or subcommands,
     behavior changes, new env vars, new supported languages, new MCP tools.
   - Compare against `README.md`. If anything is missing, wrong, or stale, **UPDATE `README.md`**
     so it matches reality. Keep examples free of hardcoded config strings (per CLAUDE.md).
   - If README already matches, state that and move on.

3. **CHANGELOG up to date?**
   - Ensure `CHANGELOG.md` describes every user-facing change under `Added` / `Changed` /
     `Fixed` subsections.
   - **Version for the heading: no arithmetic.** This repo has no `[Unreleased]` section —
     entries go under the heading for the version `Cargo.toml` *currently reads*
     (`grep -m1 '^version' Cargo.toml`), which is the one develop is building toward. Do **not**
     add +1: the bump lands in CI *after* this PR merges, and the next PR's entries go under
     that next heading. See the convention comment at the top of `CHANGELOG.md`.
   - Sanity-check that the existing pending heading still matches `Cargo.toml`, and fix it in
     place if not. Expect drift: CI bumps on **every** merge to `develop` while the heading
     only moves when someone edits it, so the two part company routinely — that is how the
     heading came to read `[1.2.4]` while `Cargo.toml` was already at `1.2.6`.
   - The heading carries a date only once the release is actually tagged; until then
     `## [X.Y.Z] (unreleased)` is correct. If an accurate entry already exists, leave it.

4. **Commit**
   - Stage code + doc changes (`git add -A`, plus `git add -f` for any tracked-but-gitignored file).
   - Commit with a clear, scoped message. End the message with:
     `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
   - Let the pre-commit hook finish. It is `cargo fmt` only, so it is fast — if a commit here
     stalls for a minute, something is wrong; do not "fix" it with `--no-verify`.

5. **Validate** (fast loop, per CLAUDE.md — do NOT run `--release`):
   - `cargo fmt --all -- --check`
   - `cargo check --all-targets`
   - `cargo clippy --all-targets -- -D warnings`
   - Fix any failures and commit again before pushing. Never push code that fails these.

6. **Push**
   - `git push -u origin HEAD`.

7. **Open PR → develop**
   - `gh pr create --base develop --head <branch> --title "<title>" --body "<body>"`.
   - Title: use `$ARGUMENTS` if provided; otherwise summarize the branch concisely.
   - Body: bullet summary of changes; end with:
     `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
   - Get the PR number for the next step by running `gh pr view --json number --jq .number`
     as its own command and reading the output. Do **not** wrap it as `PR=$(…)`: a shell
     assignment does not match this command's `Bash(gh:*)` grant, so it prompts instead of
     running pre-approved.

8. **Auto-merge after CI**
   - Feature→`develop` uses a **merge commit**: `--merge`, never `--squash`. (Squash is for the
     `develop`→`master` release PR, which `/release` handles.) Keeping the merge commit is what
     makes `develop`'s history readable as `Merge PR #N` and keeps each feature's commits
     reachable.
   - **Auto-merge is disabled on this repo** (`allow_auto_merge: false` — check with
     `gh api repos/{owner}/{repo} --jq .allow_auto_merge`), so `--auto` errors out. Expect to
     take the polling path, not to fall back to it: poll `gh pr checks <PR> --watch`, then
     `gh pr merge <PR> --merge` once green.
   - The review requirement is enforced by a repo **ruleset**, not branch protection. As repo
     owner you can override it: `gh pr merge <PR> --merge --admin --delete-branch`.

## Report
Branch, the pending version the CHANGELOG heading is filed under, doc updates made, PR URL,
and merge status (auto-merge enabled / merged). Note that the version bump itself appears on
`develop` only *after* the merge, as a CI commit.
