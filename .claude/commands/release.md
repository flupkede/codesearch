---
description: Cut a release — run /merge (feature → develop), then promote develop → master and push the version tag
argument-hint: [optional PR/release title]
allowed-tools: Bash(git:*), Bash(gh:*), Bash(cargo:*), Bash(grep:*), Read, Edit, Grep, Glob
---

# /release — full release: land on `develop`, promote to `master`, tag

This is `/merge` **plus** the `develop → master` promotion and the version-tag push that
triggers the build/publish pipeline.

## Branch & version facts (this repo)
- Flow: `feature/*` → PR → **`develop`** → PR → **`master`** → push tag `vX.Y.Z`.
- `master` is protected: PRs to it may come **only** from `develop` or `release/*`
  (`.github/workflows/protect-master.yml`).
- Pushing a `vX.Y.Z` tag triggers `.github/workflows/release.yml` (builds Windows/Linux/macOS
  archives, plain + `-with-csharp`, and publishes the GitHub release). **Push the tag only
  AFTER the develop→master PR has merged.**
- The version is fixed by the feature-branch commit (the pre-commit hook bumps only on
  feature branches). develop/master merges and the tag all carry that same version.

## Guardrails
- NEVER use `--no-verify`. NEVER force-push shared branches.
- Push the tag exactly once, only after master has the release commit.
- If CI fails at any gate, STOP and report — do not promote or tag a red build.

## Part 1 — land on `develop` (the `/merge` workflow)
Execute every step of **`/merge`** (README/CHANGELOG checks → commit → push → PR → auto-merge
to `develop`). Then **wait for the develop PR to actually merge** (auto-merge waits on CI):
- Capture the PR number (`PR=$(gh pr view --json number --jq .number)`), then poll
  `gh pr view "$PR" --json state,mergedAt,mergeStateStatus` until `state` is `MERGED`.
- If checks fail, STOP and report. Do not proceed to Part 2.

## Part 2 — promote `develop` → `master`
1. `git fetch origin && git checkout develop && git pull --ff-only origin develop`.
2. Determine the release version: `VERSION=v$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.+)".*/\1/')`.
3. Open the release PR (source `develop`, which protect-master allows):
   - `gh pr create --base master --head develop --title "Release $VERSION — <summary>" --body "<body>"`.
   - Title: prefix `Release $VERSION — ` then a short summary (or `$ARGUMENTS` if provided),
     matching history (e.g. `Release v1.0.142 — serve responsive during warmup`).
   - Body ends with: `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
   - Capture the PR number: `RELEASE_PR=$(gh pr view develop --json number --jq .number)`.
4. This repo **disallows merge commits** — always use `--squash`, never `--merge`.
   `gh pr merge "$RELEASE_PR" --auto --squash`. Wait until `state` is
   `MERGED` (poll as in Part 1). If auto-merge is unavailable, `gh pr checks "$RELEASE_PR" --watch`
   then `gh pr merge "$RELEASE_PR" --squash`. If CI fails, STOP.

## Part 3 — tag the release
1. `git fetch origin --tags && git checkout master && git pull --ff-only origin master`.
2. Confirm the version on master matches: `grep -m1 '^version' Cargo.toml` equals `$VERSION` (minus the `v`).
   If it does not match, STOP and report (do not guess a tag).
3. Guard against a double release: if `$VERSION` already exists as a tag
   (`git tag -l "$VERSION"` non-empty, or `git ls-remote --tags origin "$VERSION"` non-empty),
   STOP — the release was already cut.
4. `git tag "$VERSION" && git push origin "$VERSION"` → triggers `release.yml`.
5. Report the pushed tag and remind the user to watch the Actions "Release" run for artifacts.

## Part 4 — keep `develop` in sync (only if needed)
If `master` ended up ahead of `develop` (e.g. a CHANGELOG/version edit merged only on master),
open a sync PR `master → develop` (or fast-forward develop) — matching the repo's post-release
sync convention (e.g. PR #90 "sync: backfill CHANGELOG … from master"). Skip if already in sync.

## Report
develop PR URL, release PR URL, tag pushed (`vX.Y.Z`), final version, and sync action (if any).
