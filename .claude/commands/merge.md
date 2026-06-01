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
- Flow: `feature/*` | `features/*` | `fix/*` → PR → **`develop`** → (later) PR → **`master`**.
- `master` is protected (`.github/workflows/protect-master.yml`): it accepts PRs only from
  `develop` or `release/*`.
- The pre-commit hook **bumps the patch version (+1) and rebuilds the binary on feature
  branches only** (`feature/*`, `features/*`, `fix/*`). On `develop`/`master`/`release`/`chore`
  it runs `cargo fmt` only — no bump. So **the feature-branch commit here fixes the release
  version**; it carries forward unchanged through develop, master, and the tag.

## Guardrails
- ABORT unless the current branch matches `feature/*`, `features/*`, or `fix/*` — i.e. the
  branches the pre-commit hook version-bumps. Never run from `develop`, `master`, `release/*`,
  or `chore/*`: on those the hook does **not** bump, so the version/CHANGELOG premise below
  would silently break.
- NEVER push directly to `develop` or `master` — everything lands via a PR.
- NEVER pass `--no-verify` / `--no-gpg-sign` — let the pre-commit hook run (it bumps + rebuilds).
- Do NOT create or push a tag here. That is `/release`'s job.
- Do NOT force-push.

## Steps

1. **Context**
   - `git rev-parse --abbrev-ref HEAD` → current branch. If it is NOT `feature/*`, `features/*`,
     or `fix/*`, STOP with an error (see Guardrails).
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
   - Ensure `CHANGELOG.md` has an entry for this change under a `## [X.Y.Z] - YYYY-MM-DD`
     heading with `Added` / `Changed` / `Fixed` subsections describing every user-facing change.
   - **Version for the heading**: the hook bumps the patch by +1 on **every** feature-branch
     commit where the working-tree version still equals HEAD's. The most reliable approach is to
     land this branch in a **single commit** — then the heading version = current
     `Cargo.toml` version + 1 (`grep -m1 '^version' Cargo.toml`). If you commit more than once,
     the version advances once per commit; after the final commit, read the actual
     `Cargo.toml` version and make sure the CHANGELOG heading matches it (fix it if not).
   - Use today's date. If an accurate entry already exists for the pending version, leave it.

4. **Commit**
   - Stage code + doc changes (`git add -A`, plus `git add -f` for any tracked-but-gitignored file).
   - Commit with a clear, scoped message. End the message with:
     `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
   - Let the pre-commit hook finish (fmt → version bump → rebuild). This can take 60–120s.

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
   - Capture the PR number for the next step:
     `PR=$(gh pr view --json number --jq .number)`.

8. **Auto-merge after CI**
   - `gh pr merge "$PR" --auto --merge` so the PR lands automatically once required checks pass.
   - If auto-merge is not enabled on the repo (command errors), fall back: poll
     `gh pr checks "$PR" --watch`, then `gh pr merge "$PR" --merge` once green.

## Report
Branch, pending release version, doc updates made, PR URL, and merge status
(auto-merge enabled / merged).
